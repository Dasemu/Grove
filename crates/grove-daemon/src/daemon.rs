//! Event loop: file events are debounced per repo, a periodic pass covers fetches and anything
//! the watcher missed, failures back off exponentially. One worker thread runs the sync passes,
//! so passes never overlap.

use crate::activity::{Activity, now_secs};
use crate::discover::{discover, group_of, is_ready, subdirs};
use crate::ipc;
use crate::visibility::{RealForge, Visibility, snapshot_gate};
use grove_core::config::Config;
use grove_core::receive::Received;
use grove_core::sync::{Options, sync_repo};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Quiet time after the first layout event before rediscovery runs (`grove add` does several
/// renames in a row).
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct Settings {
    pub socket: PathBuf,
    pub activity_log: Option<PathBuf>,
    pub remote: String,
    /// Where forge visibility answers are cached (`None` = in memory only).
    pub visibility_cache: Option<PathBuf>,
}

impl Settings {
    /// Standard locations: socket in `$XDG_RUNTIME_DIR/grove`, log in the state dir.
    pub fn standard() -> Settings {
        let state = Config::state_dir().unwrap_or_else(|| std::env::temp_dir().join("grove"));
        Settings {
            socket: ipc::default_socket_path(),
            activity_log: Some(state.join("activity.jsonl")),
            remote: "origin".into(),
            visibility_cache: Some(state.join("visibility.json")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WtStatus {
    pub branch: Option<String>,
    pub state: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct RepoStatus {
    /// `idle`, `pending`, `syncing` or `error`.
    pub phase: &'static str,
    pub last_sync: Option<u64>,
    pub last_error: Option<String>,
    pub worktrees: Vec<WtStatus>,
    pub failures: u32,
    /// Why WIP snapshots are not pushed for this repo (`None` = they are).
    pub snapshots_withheld: Option<String>,
    /// Last verdict of the snapshot gate (`None` = not evaluated yet).
    pub gate_allowed: Option<bool>,
}

impl Default for RepoStatus {
    fn default() -> Self {
        RepoStatus {
            phase: "idle",
            last_sync: None,
            last_error: None,
            worktrees: Vec::new(),
            failures: 0,
            snapshots_withheld: None,
            gate_allowed: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Shared {
    pub paused: bool,
    pub repos: BTreeMap<PathBuf, RepoStatus>,
    /// Plain clones in configured groups: ignored until migrated with `grove add`.
    pub not_migrated: Vec<PathBuf>,
}

pub enum Msg {
    Changed(PathBuf),
    /// The layout under a group directory may have changed: rediscover (debounced).
    Rescan,
    /// Sync one repo (or all) as soon as possible.
    SyncNow(Option<PathBuf>),
    Shutdown,
}

#[derive(Clone)]
pub struct Ctx {
    pub shared: Arc<Mutex<Shared>>,
    pub activity: Arc<Activity>,
    pub tx: Sender<Msg>,
    pub machine: String,
}

pub struct Daemon {
    pub ctx: Ctx,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl Daemon {
    pub fn start(cfg: Config, st: Settings) -> std::io::Result<Daemon> {
        let (tx, rx) = channel();
        let ctx = Ctx {
            shared: Arc::default(),
            activity: Activity::new(st.activity_log.clone()),
            tx,
            machine: cfg.machine.name.clone(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server = ipc::serve(&st.socket, ctx.clone(), stop.clone())?;
        let worker = {
            let ctx = ctx.clone();
            std::thread::spawn(move || run(cfg, st, rx, ctx))
        };
        ctx.activity.log("start", None, "daemon started");
        Ok(Daemon {
            socket: server.socket.clone(),
            ctx,
            stop,
            worker: Some(worker),
            server: Some(server.handle),
        })
    }

    /// Stop the worker and the IPC server. Work in progress is never lost: the worker finishes
    /// the pass it is running and everything else lives in git.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.ctx.tx.send(Msg::Shutdown);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        if let Some(h) = self.server.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn stamp() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms}-{}", SEQ.fetch_add(1, Ordering::SeqCst))
}

/// Matches one path component against an ignore pattern: exact name or `*suffix`.
fn pattern_matches(pat: &str, name: &str) -> bool {
    match pat.strip_prefix('*') {
        Some(suffix) => name.ends_with(suffix),
        None => name == pat,
    }
}

/// True for paths whose changes must not trigger a sync: git internals and user-ignored names.
pub fn ignored(rel: &Path, extra: &[String]) -> bool {
    rel.components().any(|c| {
        let n = c.as_os_str().to_string_lossy();
        n == ".git"
            || n == ".bare"
            || n == ".grove-busy"
            || n == ".grove-stage"
            || extra.iter().any(|p| pattern_matches(p, &n))
    })
}

fn repo_of<'a>(path: &Path, repos: &'a [PathBuf]) -> Option<&'a PathBuf> {
    repos.iter().find(|r| path.starts_with(r))
}

/// True for events that may change what is managed: a child directory of a configured group
/// appearing, disappearing or being renamed, a group directory itself doing so under the root, or
/// `.bare` / `.git` appearing or disappearing inside such a child.
pub fn triggers_rediscovery(path: &Path, root: &Path, groups: &[String]) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let in_group = |n: &String| groups.contains(n);
    match parts.as_slice() {
        [g] => in_group(g),
        [g, _] => in_group(g),
        [g, _, f] => {
            in_group(g)
                && matches!(
                    f.as_str(),
                    ".bare" | ".git" | ".grove-busy" | ".grove-stage"
                )
        }
        _ => false,
    }
}

struct Watch {
    watcher: Option<notify::RecommendedWatcher>,
    repos: Arc<Mutex<Vec<PathBuf>>>,
    /// Watched paths: whether the watch is recursive, and the (device, inode) of the directory it
    /// was set on, so a directory deleted and recreated under the same name is watched anew.
    watched: HashMap<PathBuf, (bool, Option<(u64, u64)>)>,
    /// Paths whose watch failed and was already reported (reported once until it succeeds).
    watch_failed: HashSet<PathBuf>,
    /// Stalled repos already warned about.
    warned_stalled: HashSet<PathBuf>,
}

fn dir_id(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
}

fn make_watch(
    tx: Sender<Msg>,
    extra: Vec<String>,
    root: PathBuf,
    groups: Vec<String>,
    ctx: &Ctx,
) -> Watch {
    let repos: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let cb_repos = repos.clone();
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        let repos = cb_repos.lock().unwrap();
        for p in &ev.paths {
            if triggers_rediscovery(p, &root, &groups) {
                let _ = tx.send(Msg::Rescan);
            }
            if let Some(r) = repo_of(p, &repos)
                && let Ok(rel) = p.strip_prefix(r)
                && !ignored(rel, &extra)
            {
                let _ = tx.send(Msg::Changed(r.clone()));
            }
        }
    };
    let watcher = match notify::recommended_watcher(handler) {
        Ok(w) => Some(w),
        Err(e) => {
            ctx.activity.log(
                "watch_error",
                None,
                format!("file watching unavailable, periodic passes only: {e}"),
            );
            None
        }
    };
    Watch {
        watcher,
        repos,
        watched: HashMap::new(),
        watch_failed: HashSet::new(),
        warned_stalled: HashSet::new(),
    }
}

impl Watch {
    /// Adds the watches in `wanted` that are missing, and replaces watches whose directory was
    /// deleted and recreated. With `prune`, also removes watches that are no longer wanted or
    /// whose mode changed. Nothing in the watched directories is touched.
    fn sync(&mut self, wanted: &HashMap<PathBuf, bool>, prune: bool, ctx: &Ctx) {
        let stale: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|(p, (rec, id))| *id != dir_id(p) || (prune && wanted.get(*p) != Some(rec)))
            .map(|(p, _)| p.clone())
            .collect();
        for p in stale {
            self.watched.remove(&p);
            if let Some(w) = &mut self.watcher {
                let _ = w.unwatch(&p);
            }
        }
        for (p, rec) in wanted {
            if self.watched.contains_key(p) {
                continue;
            }
            let Some(id) = dir_id(p) else { continue };
            let mode = if *rec {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            let Some(w) = &mut self.watcher else { continue };
            match w.watch(p, mode) {
                Ok(()) => {
                    self.watched.insert(p.clone(), (*rec, Some(id)));
                    self.watch_failed.remove(p);
                }
                // Not recorded as watched, so the next rescan retries.
                Err(e) => {
                    if self.watch_failed.insert(p.clone()) {
                        ctx.activity.log(
                            "watch_error",
                            Some(&p.display().to_string()),
                            format!(
                                "cannot watch (raise fs.inotify.max_user_watches?): {e}; relying on periodic passes"
                            ),
                        );
                    }
                }
            }
        }
    }
}

/// Re-runs discovery and makes the shared status, the watches and the repo list match it.
/// Returns the managed repos found now and the ones that were not managed before.
fn rescan(
    root: &Path,
    groups: &[String],
    watch: &mut Watch,
    ctx: &Ctx,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    // Shallow watches first, then discover: a `.bare` created in a child between the two steps
    // is then still seen as an event.
    let mut wanted: HashMap<PathBuf, bool> = HashMap::new();
    wanted.insert(root.to_path_buf(), false);
    for g in groups {
        let gdir = root.join(g);
        wanted.insert(gdir.clone(), false);
        for child in subdirs(&gdir) {
            // Children that are not managed yet are watched shallowly so `.bare` appearing
            // inside them (`grove add`, `grove import`) is noticed.
            wanted.insert(child, false);
        }
    }
    watch.sync(&wanted, false, ctx);
    let d = discover(root, groups);
    let found = d.managed;
    for r in &d.stalled {
        if watch.warned_stalled.insert(r.clone()) {
            ctx.activity.log(
                "repo_stalled",
                Some(&r.display().to_string()),
                "WARNING: left alone: an interrupted `grove add`/clone left `.grove-busy` or `.grove-stage` behind; check the repo and restore from the backups, then remove the marker by hand",
            );
        }
    }
    watch.warned_stalled.retain(|p| d.stalled.contains(p));
    let mut added = Vec::new();
    let mut removed = Vec::new();
    {
        let mut shared = ctx.shared.lock().unwrap();
        shared.not_migrated = d.not_migrated;
        shared.repos.retain(|p, _| {
            let keep = found.contains(p);
            if !keep {
                removed.push(p.clone());
            }
            keep
        });
        for r in &found {
            if !shared.repos.contains_key(r) {
                shared.repos.insert(r.clone(), RepoStatus::default());
                added.push(r.clone());
            }
        }
    }
    for r in &added {
        ctx.activity
            .log("repo_added", Some(&r.display().to_string()), "now managed");
    }
    for r in &removed {
        ctx.activity.log(
            "repo_removed",
            Some(&r.display().to_string()),
            "no longer managed (directory gone or no longer a bare layout); files untouched",
        );
    }
    for r in &found {
        wanted.insert(r.clone(), true);
    }
    *watch.repos.lock().unwrap() = found.clone();
    watch.sync(&wanted, true, ctx);
    (found, added)
}

fn run(cfg: Config, st: Settings, rx: Receiver<Msg>, ctx: Ctx) {
    let debounce = Duration::from_millis(cfg.sync.snapshot_debounce_ms);
    let fetch_every = Duration::from_secs(cfg.sync.fetch_interval_s.max(1));
    let root = cfg.root_path();
    let groups: Vec<String> = cfg.groups.iter().map(|g| g.name.clone()).collect();
    let mut visibility = Visibility::load(st.visibility_cache.clone());
    let mut watch = make_watch(
        ctx.tx.clone(),
        cfg.sync.ignore_extra.clone(),
        root.clone(),
        groups.clone(),
        &ctx,
    );
    rescan(&root, &groups, &mut watch, &ctx);

    // due_at: when a repo should next be synced; backoff_until: earliest retry after failures.
    let mut due_at: HashMap<PathBuf, Instant> = HashMap::new();
    let mut backoff_until: HashMap<PathBuf, Instant> = HashMap::new();
    // first_at: first unsynced event of a repo, so a constant stream of events cannot postpone
    // its snapshot beyond `max_wait`.
    let mut first_at: HashMap<PathBuf, Instant> = HashMap::new();
    let max_wait = (debounce * 10).max(Duration::from_secs(5));
    let mut next_full = Instant::now();
    // When a debounced rediscovery is due (set by the first event of a burst, never pushed back).
    let mut rescan_at: Option<Instant> = None;

    loop {
        let now = Instant::now();
        let wake = due_at
            .values()
            .copied()
            .chain([next_full])
            .chain(rescan_at)
            .min()
            .unwrap_or(next_full);
        let mut timeout = wake
            .saturating_duration_since(now)
            .min(Duration::from_secs(1));
        if ctx.shared.lock().unwrap().paused {
            // Nothing is due while paused: only wait for commands, never spin on past deadlines.
            timeout = Duration::from_secs(1);
        }
        match rx.recv_timeout(timeout) {
            Ok(Msg::Changed(r)) => {
                let now = Instant::now();
                let first = *first_at.entry(r.clone()).or_insert(now);
                let at = (now + debounce).min(first + max_wait);
                // An earlier deadline (e.g. an immediate `sync`) is never pushed back.
                let at = due_at.get(&r).map_or(at, |old| at.min(*old));
                due_at.insert(r.clone(), at);
                if let Some(s) = ctx.shared.lock().unwrap().repos.get_mut(&r)
                    && s.phase == "idle"
                {
                    s.phase = "pending";
                }
            }
            Ok(Msg::Rescan) => {
                rescan_at.get_or_insert(Instant::now() + RESCAN_DEBOUNCE);
            }
            Ok(Msg::SyncNow(Some(r))) => {
                backoff_until.remove(&r);
                due_at.insert(r, Instant::now());
            }
            Ok(Msg::SyncNow(None)) => {
                backoff_until.clear();
                next_full = Instant::now();
            }
            Ok(Msg::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        // Discovery also runs while paused so the status stays current; only syncing stops.
        if rescan_at.is_some_and(|t| t <= Instant::now()) {
            rescan_at = None;
            let (found, added) = rescan(&root, &groups, &mut watch, &ctx);
            forget_removed(&found, &mut due_at, &mut first_at, &mut backoff_until);
            for r in added {
                due_at.insert(r, Instant::now());
            }
        }
        if ctx.shared.lock().unwrap().paused {
            continue;
        }
        let now = Instant::now();
        if now >= next_full {
            // Safety net for events the watcher missed (one `read_dir` per group).
            let (found, _) = rescan(&root, &groups, &mut watch, &ctx);
            forget_removed(&found, &mut due_at, &mut first_at, &mut backoff_until);
            for r in found {
                due_at.insert(r, now);
            }
            next_full = now + fetch_every;
        }
        let mut due: Vec<PathBuf> = due_at
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(r, _)| r.clone())
            .collect();
        due.sort();
        for r in due {
            if !ctx.shared.lock().unwrap().repos.contains_key(&r) {
                due_at.remove(&r);
                continue;
            }
            due_at.remove(&r);
            first_at.remove(&r);
            if !is_ready(&r) {
                // Removed or migrated away since the last discovery: drop it, no error.
                let (found, _) = rescan(&root, &groups, &mut watch, &ctx);
                forget_removed(&found, &mut due_at, &mut first_at, &mut backoff_until);
                continue;
            }
            // While backing off after a failure, still snapshot local work (no network needed);
            // the network is retried at the first pass after the backoff expires.
            let offline = backoff_until.get(&r).is_some_and(|u| *u > now);
            let failures = pass(&r, &cfg, &st, &mut visibility, offline, &ctx);
            if !is_ready(&r) {
                // It vanished mid-pass: whatever the pass reported is moot.
                let (found, _) = rescan(&root, &groups, &mut watch, &ctx);
                forget_removed(&found, &mut due_at, &mut first_at, &mut backoff_until);
                continue;
            }
            if offline {
                continue;
            }
            if failures == 0 {
                backoff_until.remove(&r);
            } else {
                let wait =
                    Duration::from_secs(5u64.saturating_mul(1 << failures.min(6))).min(MAX_BACKOFF);
                backoff_until.insert(r, Instant::now() + wait);
            }
            if ctx.shared.lock().unwrap().paused {
                break;
            }
        }
    }
    ctx.activity.log("stop", None, "daemon stopped");
}

/// Drops scheduling state of repos that are no longer managed.
fn forget_removed(
    found: &[PathBuf],
    due_at: &mut HashMap<PathBuf, Instant>,
    first_at: &mut HashMap<PathBuf, Instant>,
    backoff_until: &mut HashMap<PathBuf, Instant>,
) {
    due_at.retain(|r, _| found.contains(r));
    first_at.retain(|r, _| found.contains(r));
    backoff_until.retain(|r, _| found.contains(r));
}

/// One sync pass for `repo`; returns the number of consecutive failures (0 = success).
fn pass(
    repo: &Path,
    cfg: &Config,
    st: &Settings,
    visibility: &mut Visibility,
    offline: bool,
    ctx: &Ctx,
) -> u32 {
    let name = repo.display().to_string();
    if !is_ready(repo) {
        // Being migrated/cloned (or gone): leave it alone, the rediscovery sorts it out.
        return 0;
    }
    set_phase(ctx, repo, "syncing");
    let group = group_of(repo).unwrap_or_default();
    // `auto_materialize` is per group; a repo whose group is gone from the config keeps the default.
    let materialize = cfg
        .groups
        .iter()
        .find(|g| g.name == group)
        .is_none_or(|g| g.auto_materialize);
    // Offline there is no lookup: use the last verdict, and take no snapshot if there is none.
    let push_snapshots = if offline {
        ctx.shared
            .lock()
            .unwrap()
            .repos
            .get(repo)
            .is_some_and(|s| s.gate_allowed == Some(true))
    } else {
        let gate = snapshot_gate(cfg, repo, &st.remote, visibility, now_secs(), &RealForge);
        let allowed = gate.is_ok();
        note_snapshot_gate(ctx, repo, &name, gate.err());
        if !allowed {
            purge_snapshots(ctx, repo, &name, cfg, st);
        }
        allowed
    };
    let opts = Options {
        machine: cfg.machine.name.clone(),
        stamp: stamp(),
        remote: st.remote.clone(),
        offline,
        materialize,
        push_snapshots,
    };
    let result = sync_repo(repo, &opts);
    let mut shared = ctx.shared.lock().unwrap();
    // The repo may have been dropped by a rediscovery while the pass ran.
    let Some(status) = shared.repos.get_mut(repo) else {
        return 0;
    };
    let mut errors: Vec<String> = Vec::new();
    match result {
        Err(e) => errors.push(e.to_string()),
        Ok(rep) => {
            errors.extend(rep.fetch_error.clone());
            let mut now_wts = Vec::new();
            for w in &rep.worktrees {
                let branch = w.branch.clone().unwrap_or_else(|| "(detached)".into());
                let state = format!("{:?}", w.state).to_lowercase();
                if let Some(kind) = w.snapshot {
                    let mut msg = format!("{branch}: {kind}");
                    if !w.skipped.is_empty() {
                        msg.push_str(&format!(
                            " (left out, over the size limit: {})",
                            w.skipped.join(", ")
                        ));
                    }
                    ctx.activity.log("snapshot", Some(&name), msg);
                }
                match &w.received {
                    Some(Received::FastForwarded { .. }) => {
                        ctx.activity
                            .log("fast_forward", Some(&name), branch.clone())
                    }
                    Some(Received::Declined(why)) => ctx.activity.log(
                        "declined",
                        Some(&name),
                        format!("{branch}: not fast-forwarded: {why}"),
                    ),
                    _ => {}
                }
                if let Some((m, a)) = &w.applied {
                    ctx.activity
                        .log("applied", Some(&name), format!("{branch}: {a:?} from {m}"));
                }
                let prev = status
                    .worktrees
                    .iter()
                    .find(|p| p.branch.as_deref() == Some(&branch))
                    .map(|p| p.state.clone());
                if prev.as_deref() != Some(&state) {
                    ctx.activity
                        .log("state", Some(&name), format!("{branch}: {state}"));
                }
                errors.extend(w.errors.iter().map(|e| format!("{branch}: {e}")));
                now_wts.push(WtStatus {
                    branch: w.branch.clone(),
                    state,
                    path: w.path.display().to_string(),
                });
            }
            status.worktrees = now_wts;
            for p in &rep.materialized {
                ctx.activity
                    .log("materialized", Some(&name), p.display().to_string());
            }
            for a in &rep.archived {
                ctx.activity.log("archived", Some(&name), format!("{a:?}"));
            }
        }
    }
    if offline {
        // A snapshot-only pass during backoff changes neither the failure count nor the phase.
        status.phase = if status.failures > 0 { "error" } else { "idle" };
        if !errors.is_empty() {
            ctx.activity.log("error", Some(&name), errors.join("; "));
        }
    } else if errors.is_empty() {
        status.phase = "idle";
        status.failures = 0;
        status.last_error = None;
        status.last_sync = Some(now_secs());
    } else {
        status.phase = "error";
        status.failures += 1;
        status.last_error = Some(errors.join("; "));
        ctx.activity.log("error", Some(&name), errors.join("; "));
    }
    status.failures
}

/// Records why snapshots are withheld (or that they are pushed again), logging only changes.
fn note_snapshot_gate(ctx: &Ctx, repo: &Path, name: &str, withheld: Option<String>) {
    let mut shared = ctx.shared.lock().unwrap();
    let Some(status) = shared.repos.get_mut(repo) else {
        return;
    };
    status.gate_allowed = Some(withheld.is_none());
    if status.snapshots_withheld == withheld {
        return;
    }
    match &withheld {
        Some(why) => ctx.activity.log(
            "snapshots_withheld",
            Some(name),
            format!("WIP snapshots not pushed: {why}"),
        ),
        None => ctx
            .activity
            .log("snapshots_enabled", Some(name), "WIP snapshots are pushed"),
    }
    status.snapshots_withheld = withheld;
}

/// Snapshots are withheld: remove this machine's snapshot refs from the remote (they may have
/// been pushed while the repo was still private) and locally. Only `refs/sync/<machine>/*`;
/// never other machines' refs or branches. Does nothing when there are no local refs, and a
/// failed deletion keeps them so the next pass retries.
fn purge_snapshots(ctx: &Ctx, repo: &Path, name: &str, cfg: &Config, st: &Settings) {
    let gitdir = grove_core::sync::git_dir_for(repo);
    match grove_core::receive::purge_own_snapshots(&gitdir, &cfg.machine.name, &st.remote) {
        Ok(done) if done.is_empty() => {}
        Ok(done) => ctx.activity.log(
            "snapshots_purged",
            Some(name),
            format!(
                "WARNING: snapshots are no longer allowed; deleted {} snapshot ref(s) of this machine from the remote: {}",
                done.len(),
                done.join(", ")
            ),
        ),
        Err(e) => ctx.activity.log(
            "error",
            Some(name),
            format!("could not delete this machine's snapshot refs from the remote: {e}"),
        ),
    }
}

fn set_phase(ctx: &Ctx, repo: &Path, phase: &'static str) {
    if let Some(s) = ctx.shared.lock().unwrap().repos.get_mut(repo) {
        s.phase = phase;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_git_internals_and_patterns() {
        let extra = vec!["node_modules".to_string(), "*.swp".to_string()];
        assert!(ignored(Path::new(".bare/objects/ab"), &extra));
        assert!(ignored(Path::new("main/.git"), &extra));
        assert!(ignored(Path::new("main/node_modules/x/y.js"), &extra));
        assert!(ignored(Path::new("main/src/.a.rs.swp"), &extra));
        assert!(!ignored(Path::new("main/src/a.rs"), &extra));
    }

    #[test]
    fn layout_events_trigger_rediscovery() {
        let root = Path::new("/r");
        let g = vec!["g".to_string()];
        let t = |p: &str| triggers_rediscovery(Path::new(p), root, &g);
        assert!(t("/r/g"));
        assert!(t("/r/g/repo"));
        assert!(t("/r/g/repo/.bare"));
        assert!(t("/r/g/repo/.git"));
        assert!(t("/r/g/repo/.grove-busy"));
        assert!(t("/r/g/repo/.grove-stage"));
        assert!(!t("/r/g/repo/main"));
        assert!(!t("/r/g/repo/main/.bare"));
        assert!(!t("/r/other"));
        assert!(!t("/r/other/x/.bare"));
        assert!(!t("/elsewhere/g/x"));
    }
}
