use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gitsync_core::config::Config;
use gitsync_core::inspect::{state, worktrees};
use gitsync_core::snapshot::{Outcome, take};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    name = "gitsync",
    about = "Keep git workspaces in sync across machines"
)]
struct Cli {
    /// Path to config.toml (default: $XDG_CONFIG_HOME/gitsync/config.toml)
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
}

fn load_config(arg: Option<PathBuf>) -> Result<Config> {
    let path = arg
        .or_else(Config::default_path)
        .context("cannot determine config path (set --config)")?;
    Config::load(&path).with_context(|| format!("loading {}", path.display()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Status { path } => {
            for wt in worktrees(&path)? {
                let branch = wt.branch.as_deref().unwrap_or("(detached)");
                println!(
                    "{:<24} {:?}  {}",
                    branch,
                    state(&wt.path),
                    wt.path.display()
                );
            }
        }
        Cmd::Snapshot { path } => {
            let cfg = load_config(cli.config)?;
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .to_string();
            match take(&path, &cfg.machine.name, &stamp)? {
                Outcome::Created(s) => println!("created {} {}", s.refname, s.commit),
                Outcome::Unchanged(c) => println!("unchanged {c}"),
            }
        }
    }
    Ok(())
}
