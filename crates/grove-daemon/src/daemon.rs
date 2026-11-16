//! Event loop: file events are debounced per repo, a periodic pass covers fetches and anything
//! the watcher missed, failures back off exponentially. One worker thread runs the sync passes,
//! so passes never overlap.

use crate::activity::{Activity, now_secs};
use crate::discover::discover;
use crate::ipc;
use grove_core::config::Config;
use grove_core::receive::Received;
use grove_core::sync::{Options, sync_repo};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct Settings {
    pub socket: PathBuf,
    pub activity_log: Option<PathBuf>,
    pub remote: String,
}

impl Settings {
    /// Standard locations: socket in `$XDG_RUNTIME_DIR/grove`, log in the state dir.
    pub fn standard() -> Settings {
        let state = Config::state_dir().unwrap_or_else(|| std::env::temp_dir().join("grove"));
        Settings {
            socket: ipc::default_socket_path(),
            activity_log: Some(state.join("activity.jsonl")),
            remote: "origin".into(),
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
}

impl Default for RepoStatus {
    fn default() -> Self {
        RepoStatus {
            phase: "idle",
            last_sync: None,
            last_error: None,
            worktrees: Vec::new(),
            failures: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct Shared {
    pub paused: bool,
    pub repos: BTreeMap<PathBuf, RepoStatus>,
}

pub enum Msg {
    Changed(PathBuf),
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
            || n == ".grove-stage"
            || extra.iter().any(|p| pattern_matches(p, &n))
    })
}

fn repo_of<'a>(path: &Path, repos: &'a [PathBuf]) -> Option<&'a PathBuf> {
    repos.iter().find(|r| path.starts_with(r))
}

struct Watch {
    watcher: Option<notify::RecommendedWatcher>,
    repos: Arc<Mutex<Vec<PathBuf>>>,
    watched: Vec<PathBuf>,
}

fn make_watch(tx: Sender<Msg>, extra: Vec<String>, ctx: &Ctx) -> Watch {
    let repos: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let cb_repos = repos.clone();
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        let repos = cb_repos.lock().unwrap();
        for p in &ev.paths {
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
        watched: Vec::new(),
    }
}

impl Watch {
    fn add(&mut self, repo: &Path, ctx: &Ctx) {
        if self.watched.iter().any(|w| w == repo) {
            return;
        }
        self.watched.push(repo.to_path_buf());
        self.repos.lock().unwrap().push(repo.to_path_buf());
        if let Some(w) = &mut self.watcher
            && let Err(e) = w.watch(repo, RecursiveMode::Recursive)
        {
            ctx.activity.log(
                "watch_error",
                Some(&repo.display().to_string()),
                format!(
                    "cannot watch (raise fs.inotify.max_user_watches?): {e}; relying on periodic passes"
                ),
            );
        }
    }
}

fn rescan(root: &Path, watch: &mut Watch, ctx: &Ctx) -> Vec<PathBuf> {
    let found = discover(root);
    let mut shared = ctx.shared.lock().unwrap();
    shared.repos.retain(|p, _| found.contains(p));
    for r in &found {
        shared.repos.entry(r.clone()).or_default();
    }
    drop(shared);
    for r in &found {
        watch.add(r, ctx);
    }
    found
}

fn run(cfg: Config, st: Settings, rx: Receiver<Msg>, ctx: Ctx) {
    let debounce = Duration::from_millis(cfg.sync.snapshot_debounce_ms);
    let fetch_every = Duration::from_secs(cfg.sync.fetch_interval_s.max(1));
    let root = cfg.root_path();
    let mut watch = make_watch(ctx.tx.clone(), cfg.sync.ignore_extra.clone(), &ctx);
    rescan(&root, &mut watch, &ctx);
    let materialize = cfg.groups.iter().all(|g| g.auto_materialize);

    // due_at: when a repo should next be synced; backoff_until: earliest retry after failures.
    let mut due_at: HashMap<PathBuf, Instant> = HashMap::new();
    let mut backoff_until: HashMap<PathBuf, Instant> = HashMap::new();
    // first_at: first unsynced event of a repo, so a constant stream of events cannot postpone
    // its snapshot beyond `max_wait`.
    let mut first_at: HashMap<PathBuf, Instant> = HashMap::new();
    let max_wait = (debounce * 10).max(Duration::from_secs(5));
    let mut next_full = Instant::now();

    loop {
        let now = Instant::now();
        let wake = due_at
            .values()
            .copied()
            .chain([next_full])
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
        if ctx.shared.lock().unwrap().paused {
            continue;
        }
        let now = Instant::now();
        if now >= next_full {
            let found = rescan(&root, &mut watch, &ctx);
            due_at.retain(|r, _| found.contains(r));
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
            // While backing off after a failure, still snapshot local work (no network needed);
            // the network is retried at the first pass after the backoff expires.
            let offline = backoff_until.get(&r).is_some_and(|u| *u > now);
            let failures = pass(&r, &cfg, &st, materialize, offline, &ctx);
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

/// One sync pass for `repo`; returns the number of consecutive failures (0 = success).
fn pass(
    repo: &Path,
    cfg: &Config,
    st: &Settings,
    materialize: bool,
    offline: bool,
    ctx: &Ctx,
) -> u32 {
    let name = repo.display().to_string();
    set_phase(ctx, repo, "syncing");
    let opts = Options {
        machine: cfg.machine.name.clone(),
        stamp: stamp(),
        remote: st.remote.clone(),
        offline,
        materialize,
    };
    let result = sync_repo(repo, &opts);
    let mut shared = ctx.shared.lock().unwrap();
    let status = shared.repos.entry(repo.to_path_buf()).or_default();
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
}
