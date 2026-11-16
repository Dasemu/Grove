use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use grove_core::config::Config;
use grove_core::git::Git;
use grove_core::inspect::{state, worktrees};
use grove_core::migrate::migrate;
use grove_core::receive::{Applied, Archived, Received};
use grove_core::snapshot::{Outcome, take};
use grove_core::sync::{Options, SyncReport, git_dir_for, sync_repo};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Status { path } => status(path),
        Cmd::Snapshot { path } => {
            let cfg = load_config(&cli.config)?;
            match take(path, &cfg.machine.name, &stamp())? {
                Outcome::Created(s) => println!("created {} {}", s.refname, s.commit),
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
    }
}

fn status(path: &Path) -> Result<()> {
    for wt in worktrees(&git_dir_for(path))? {
        let branch = wt.branch.as_deref().unwrap_or("(detached)");
        println!(
            "{:<24} {:?}  {}",
            branch,
            state(&wt.path),
            wt.path.display()
        );
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
