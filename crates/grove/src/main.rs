use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use grove_core::config::{Config, ProviderKind};
use grove_core::git::Git;
use grove_core::inspect::{state, worktrees};
use grove_core::migrate::migrate;
use grove_core::receive::{Applied, Archived, Received};
use grove_core::resolve::{Choice, resolve};
use grove_core::snapshot::{Outcome, take};
use grove_core::state::State;
use grove_core::sync::{Options, SyncReport, git_dir_for, sync_repo};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod workflow;

#[derive(Parser)]
#[command(name = "grove", about = "Keep git workspaces in sync across machines")]
struct Cli {
    /// Path to config.toml (default: $XDG_CONFIG_HOME/grove/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the sync state of every worktree of the repo at PATH
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Also say what the next sync pass would do (no network, nothing changed)
        #[arg(long)]
        plan: bool,
    },
    /// Resolve a diverged worktree (an explicit choice; backups are always written)
    Resolve {
        /// The worktree directory
        path: PathBuf,
        /// Discard local commits, move to the remote tip
        #[arg(long, group = "how")]
        take_remote: bool,
        /// Overwrite the remote branch with the local one
        #[arg(long, group = "how")]
        keep_local: bool,
        /// Keep local commits on a new branch (own worktree), then take the remote tip
        #[arg(long, value_name = "BRANCH", group = "how")]
        save_as: Option<String>,
        /// Open both sides in your difftool and change nothing
        #[arg(long, group = "how")]
        compare: bool,
    },
    /// Take a snapshot of the worktree at PATH into refs/sync/<machine>/<branch>
    Snapshot {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Migrate the plain clone at PATH to bare + worktrees (backs up first)
    Add {
        path: PathBuf,
        /// Where to write the safety backup (default: <state dir>/backups)
        #[arg(long)]
        backup_dir: Option<PathBuf>,
    },
    /// Run one full sync pass on the repo at PATH
    Sync {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Skip all network operations
        #[arg(long)]
        offline: bool,
    },
    /// Check the local setup
    Doctor,
    /// Write a first config file for this machine
    Init {
        #[arg(long)]
        machine: Option<String>,
        #[arg(long, default_value = "~/Workspace")]
        root: String,
        /// Provider id (name used in the config and for the token)
        #[arg(long, default_value = "main")]
        provider: String,
        #[arg(long, default_value = "gitea")]
        kind: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long, default_value = "default")]
        group: String,
        /// Name of the private config repo to keep the repo registry in
        #[arg(long)]
        config_repo: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Store the API token for a provider (read from stdin, never from argv)
    Login { provider: String },
    /// Create a repo on the group's primary forge and set it up locally
    New {
        group: String,
        name: String,
        #[arg(long)]
        public: bool,
    },
    /// List (or, with names, clone) remote repos that are not on this machine
    Import { group: String, names: Vec<String> },
    /// Create the config repo and register every repo already on disk
    ConfigRepoInit,
    /// Fresh machine: clone every repo listed in the config repo
    Bootstrap,
    /// Talk to the running daemon (`groved`)
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Show every managed repo and its state
    Status,
    /// Stop syncing until `resume`
    Pause,
    /// Resume syncing (runs a full pass right away)
    Resume,
    /// Sync now: one repo (path as shown by `status`) or all
    Sync { repo: Option<PathBuf> },
    /// Show recent activity
    Log {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
}

fn load_config(arg: &Option<PathBuf>) -> Result<Config> {
    let path = arg
        .clone()
        .or_else(Config::default_path)
        .context("cannot determine config path (set --config)")?;
    Config::load(&path).with_context(|| format!("loading {}", path.display()))
}

fn stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .to_string()
}

fn daemon(action: &DaemonCmd) -> Result<()> {
    use serde_json::json;
    let socket = grove_daemon::ipc::default_socket_path();
    let ask = |req: serde_json::Value| -> Result<serde_json::Value> {
        let resp = grove_daemon::ipc::request(&socket, &req).with_context(|| {
            format!(
                "cannot reach the daemon at {} (is groved running?)",
                socket.display()
            )
        })?;
        if resp["ok"] != true {
            bail!(
                "daemon error: {}",
                resp["error"].as_str().unwrap_or("unknown")
            );
        }
        Ok(resp["data"].clone())
    };
    match action {
        DaemonCmd::Status => {
            let d = ask(json!({"v": 1, "cmd": "status"}))?;
            println!(
                "machine {}  {}",
                d["machine"].as_str().unwrap_or("?"),
                if d["paused"] == true {
                    "PAUSED"
                } else {
                    "running"
                }
            );
            for r in d["repos"].as_array().into_iter().flatten() {
                println!(
                    "{}  [{}]",
                    r["path"].as_str().unwrap_or("?"),
                    r["phase"].as_str().unwrap_or("?")
                );
                for w in r["worktrees"].as_array().into_iter().flatten() {
                    println!(
                        "    {:<24} {}",
                        w["branch"].as_str().unwrap_or("(detached)"),
                        w["state"].as_str().unwrap_or("?")
                    );
                }
                if let Some(e) = r["last_error"].as_str() {
                    println!("    error: {e}");
                }
            }
        }
        DaemonCmd::Pause => {
            ask(json!({"v": 1, "cmd": "pause"}))?;
            println!("paused");
        }
        DaemonCmd::Resume => {
            ask(json!({"v": 1, "cmd": "resume"}))?;
            println!("resumed");
        }
        DaemonCmd::Sync { repo } => {
            let repo = repo.as_ref().map(|p| p.display().to_string());
            ask(json!({"v": 1, "cmd": "sync", "repo": repo}))?;
            println!("queued");
        }
        DaemonCmd::Log { limit } => {
            let d = ask(json!({"v": 1, "cmd": "log", "limit": limit}))?;
            for e in d.as_array().into_iter().flatten() {
                println!(
                    "{}  {:<12} {}  {}",
                    e["ts"],
                    e["kind"].as_str().unwrap_or("?"),
                    e["repo"].as_str().unwrap_or("-"),
                    e["msg"].as_str().unwrap_or("")
                );
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Daemon { action } => daemon(action),
        Cmd::Status { path, plan } => status(path, *plan),
        Cmd::Resolve {
            path,
            take_remote,
            keep_local,
            save_as,
            compare,
        } => {
            let cfg = load_config(&cli.config)?;
            let choice = match (take_remote, keep_local, save_as, compare) {
                (true, ..) => Choice::TakeRemote,
                (_, true, ..) => Choice::KeepLocal,
                (_, _, Some(b), _) => Choice::SaveAsBranch(b.clone()),
                (.., true) => {
                    let ok = std::process::Command::new("git")
                        .current_dir(path)
                        .args(["difftool", "-d", "HEAD", "@{upstream}"])
                        .status()?
                        .success();
                    if !ok {
                        bail!("git difftool failed (is a difftool configured?)");
                    }
                    return Ok(());
                }
                _ => bail!(
                    "choose one of --take-remote, --keep-local, --save-as <branch>, --compare"
                ),
            };
            println!("{}", resolve(path, &cfg.machine.name, &stamp(), &choice)?);
            Ok(())
        }
        Cmd::Snapshot { path } => {
            let cfg = load_config(&cli.config)?;
            grove_core::snapshot::set_max_file_bytes(cfg.sync.max_file_mb * 1024 * 1024);
            match take(path, &cfg.machine.name, &stamp())? {
                Outcome::Created(s) => {
                    println!("created {} {}", s.refname, s.commit);
                    for f in &s.skipped {
                        println!("left out (over the size limit): {f}");
                    }
                }
                Outcome::Unchanged(c) => println!("unchanged {c}"),
            }
            Ok(())
        }
        Cmd::Add { path, backup_dir } => {
            let cfg = load_config(&cli.config)?;
            let dir = backup_dir
                .clone()
                .or_else(|| Config::state_dir().map(|d| d.join("backups")))
                .context("cannot determine backup dir (set --backup-dir)")?;
            let r = migrate(path, &cfg.machine.name, &dir, &stamp())?;
            println!("migrated: {}", r.worktree.display());
            println!(
                "backup:   {} (ref {})",
                r.backup_tar.display(),
                r.backup_ref
            );
            println!("note: shells whose cwd was inside the old checkout must `cd` again");
            Ok(())
        }
        Cmd::Sync { path, offline } => {
            let cfg = load_config(&cli.config)?;
            grove_core::snapshot::set_max_file_bytes(cfg.sync.max_file_mb * 1024 * 1024);
            let rep = sync_repo(
                path,
                &Options {
                    machine: cfg.machine.name.clone(),
                    stamp: stamp(),
                    remote: "origin".into(),
                    offline: *offline,
                    materialize: true,
                },
            )?;
            print_report(&rep)
        }
        Cmd::Doctor => doctor(&cli.config),
        Cmd::Init {
            machine,
            root,
            provider,
            kind,
            url,
            group,
            config_repo,
            force,
        } => {
            let path = cli
                .config
                .clone()
                .or_else(Config::default_path)
                .context("cannot determine config path (set --config)")?;
            let kind = match kind.as_str() {
                "gitea" => ProviderKind::Gitea,
                "forgejo" => ProviderKind::Forgejo,
                "github" => ProviderKind::Github,
                k => bail!("unknown provider kind {k:?} (gitea, forgejo, github)"),
            };
            let machine = machine
                .clone()
                .or_else(|| {
                    std::fs::read_to_string("/etc/hostname")
                        .ok()
                        .map(|h| h.trim().to_string())
                })
                .context("cannot determine the machine name (set --machine)")?;
            workflow::init(
                &path,
                workflow::InitArgs {
                    machine,
                    root: root.clone(),
                    provider: provider.clone(),
                    kind,
                    url: url.clone(),
                    group: group.clone(),
                    config_repo: config_repo.clone(),
                },
                *force,
            )?;
            println!("wrote {}", path.display());
            println!("next: `grove login {provider}` to store the API token");
            Ok(())
        }
        Cmd::Login { provider } => {
            let cfg = load_config(&cli.config)?;
            let mut tok = String::new();
            std::io::stdin().read_line(&mut tok)?;
            let who = workflow::login(&cfg, provider, tok.trim())?;
            println!("logged in to {provider} as {who}; token stored in the Secret Service");
            Ok(())
        }
        Cmd::New {
            group,
            name,
            public,
        } => {
            let wt = workflow::new_repo(&load_config(&cli.config)?, group, name, !public)?;
            println!("ready: {}", wt.display());
            Ok(())
        }
        Cmd::Import { group, names } => workflow::import(&load_config(&cli.config)?, group, names),
        Cmd::ConfigRepoInit => {
            let n = workflow::config_repo_init(&load_config(&cli.config)?)?;
            println!("config repo up to date ({n} repos registered)");
            Ok(())
        }
        Cmd::Bootstrap => {
            let (done, skipped) = workflow::bootstrap(&load_config(&cli.config)?)?;
            println!("bootstrap finished: {done} cloned, {skipped} already present");
            Ok(())
        }
    }
}

fn plan_for(s: &State) -> &'static str {
    match s {
        State::Synced => "nothing to do",
        State::Dirty => "snapshot the working changes and push the snapshot ref",
        State::Ahead => "nothing automatic: push your commits yourself",
        State::Behind => "fast-forward to the remote tip",
        State::BehindDirty => {
            "keep your edits; adopt the remote tip only if the content is identical"
        }
        State::Diverged => "nothing automatic: run `grove resolve` to choose",
        State::Error(_) => "retry; see the error",
    }
}

fn status(path: &Path, plan: bool) -> Result<()> {
    for wt in worktrees(&git_dir_for(path))? {
        let branch = wt.branch.as_deref().unwrap_or("(detached)");
        let st = state(&wt.path);
        print!("{:<24} {:?}  {}", branch, st, wt.path.display());
        if plan {
            print!("\n    next: {}", plan_for(&st));
        }
        println!();
    }
    Ok(())
}

fn print_report(rep: &SyncReport) -> Result<()> {
    let mut failed = false;
    if let Some(e) = &rep.fetch_error {
        println!("fetch: ERROR {e}");
        failed = true;
    }
    for p in &rep.materialized {
        println!("materialized {}", p.display());
    }
    for a in &rep.archived {
        match a {
            Archived::Removed { branch, backup_ref } => {
                println!("archived {branch} (backup {backup_ref})")
            }
            Archived::KeptDirty { branch } => {
                println!("kept {branch}: gone upstream but has local changes")
            }
        }
    }
    for w in &rep.worktrees {
        let branch = w.branch.as_deref().unwrap_or("(detached)");
        let mut notes = Vec::new();
        if let Some(s) = w.snapshot {
            notes.push(format!("snapshot {s}"));
        }
        if !w.skipped.is_empty() {
            notes.push(format!("left out (too large): {}", w.skipped.join(", ")));
        }
        match &w.received {
            Some(Received::FastForwarded { .. }) => notes.push("fast-forwarded".into()),
            Some(Received::Declined(why)) => notes.push(format!("not fast-forwarded: {why}")),
            _ => {}
        }
        if let Some((m, a)) = &w.applied {
            match a {
                Applied::Applied => notes.push(format!("applied snapshot from {m}")),
                Applied::Declined(why) => {
                    notes.push(format!("snapshot from {m} not applied: {why}"))
                }
                Applied::Nothing => {}
            }
        }
        println!("{:<24} {:?}  {}", branch, w.state, notes.join("; "));
        for e in &w.errors {
            println!("  error: {e}");
            failed = true;
        }
    }
    if failed {
        bail!("sync finished with errors");
    }
    Ok(())
}

fn doctor(cfg_arg: &Option<PathBuf>) -> Result<()> {
    let mut bad = 0;
    let mut line = |ok: Option<bool>, msg: String| {
        let tag = match ok {
            Some(true) => "ok  ",
            Some(false) => {
                bad += 1;
                "FAIL"
            }
            None => "warn",
        };
        println!("[{tag}] {msg}");
    };
    match Git::new(".").run(&["--version"]) {
        Ok(v) => line(Some(true), v),
        Err(e) => line(Some(false), format!("git not usable: {e}")),
    }
    let cfg = match load_config(cfg_arg) {
        Ok(c) => {
            line(
                Some(true),
                format!("config valid (machine {:?})", c.machine.name),
            );
            Some(c)
        }
        Err(e) => {
            line(Some(false), format!("{e:#}"));
            None
        }
    };
    if let Some(c) = &cfg {
        let root = c.root_path();
        line(Some(root.is_dir()), format!("root {}", root.display()));
        for g in &c.groups {
            let gdir = root.join(&g.name);
            let Ok(rd) = std::fs::read_dir(&gdir) else {
                line(
                    None,
                    format!("group {:?}: {} missing", g.name, gdir.display()),
                );
                continue;
            };
            for e in rd.filter_map(Result::ok) {
                let bare = e.path().join(".bare");
                if !bare.is_dir() {
                    continue;
                }
                let git = Git::new(&bare);
                let has_remote = matches!(
                    git.probe(&["config", "--get", "remote.origin.url"]),
                    Ok(Some(_))
                );
                let has_refspec = matches!(
                    git.probe(&["config", "--get", "remote.origin.fetch"]),
                    Ok(Some(_))
                );
                line(
                    Some(has_remote && has_refspec),
                    format!("{}: origin remote and fetch refspec", e.path().display()),
                );
            }
        }
    }
    match std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(n) if n >= 65536 => line(Some(true), format!("inotify max_user_watches = {n}")),
        Some(n) => line(
            None,
            format!("inotify max_user_watches = {n} (raise fs.inotify.max_user_watches)"),
        ),
        None => line(None, "cannot read inotify limit".into()),
    }
    if bad > 0 {
        bail!("{bad} check(s) failed");
    }
    Ok(())
}
