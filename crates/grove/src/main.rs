use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use grove_core::config::Config;
use grove_core::git::Git;
use grove_core::inspect::{state, worktrees};
use grove_core::migrate::migrate;
use grove_core::prune::{Pruned, merged_how};
use grove_core::receive::{Applied, Archived, Received};
use grove_core::resolve::{Choice, resolve};
use grove_core::snapshot::{Outcome, take};
use grove_core::state::State;
use grove_core::sync::{Options, SyncReport, git_dir_for, sync_repo};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod namespace;
mod secrets;
#[cfg(test)]
#[path = "../../../test-support/tmp.rs"]
mod test_tmp;
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
    /// Run one full sync pass on the repo at PATH (pushes WIP snapshots whatever the
    /// `snapshots` policy says; only the daemon enforces it)
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
        /// How git talks to the forge: https (API token) or ssh (your keys)
        #[arg(long, default_value = "https")]
        protocol: String,
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
        /// Open the new worktree afterwards (with LAUNCHER, or the default one)
        #[arg(long, value_name = "LAUNCHER", num_args = 0..=1, default_missing_value = "")]
        open: Option<String>,
    },
    /// Open a worktree in an editor or tool (Zed, VS Code, Claude Code, ...)
    Open {
        /// A path, or `[group/]repo[@branch]` under the root (default: the current directory)
        #[arg(default_value = ".")]
        target: String,
        /// Launcher name (see --list; default: `open.default`, else the first installed)
        #[arg(long = "with", short = 'w', value_name = "LAUNCHER")]
        with: Option<String>,
        /// List the launchers and whether they are installed
        #[arg(long)]
        list: bool,
        /// Print the command instead of running it
        #[arg(long)]
        print: bool,
    },
    /// Manage namespaces: the groups of repos under `<root>/<name>` and where they live
    Group {
        #[command(subcommand)]
        action: GroupCmd,
    },
    /// Manage forge accounts (Gitea, Forgejo, GitHub)
    Provider {
        #[command(subcommand)]
        action: ProviderCmd,
    },
    /// List (or, with names, clone) remote repos that are not on this machine
    Import { group: String, names: Vec<String> },
    /// Create the config repo and register every repo already on disk
    ConfigRepoInit,
    /// Fresh machine: clone every repo listed in the config repo
    Bootstrap,
    /// Encrypted sync of files git must not hold (.env, ...), via the config repo
    Secrets {
        #[command(subcommand)]
        action: SecretsCmd,
    },
    /// Talk to the running daemon (`groved`)
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum GroupCmd {
    /// List the namespaces
    List,
    /// Add a namespace (or change one with --replace) and create its directory
    Add {
        name: String,
        /// Provider the repos are created on
        #[arg(long)]
        primary: String,
        /// Provider(s) the repos are mirrored to (repeatable)
        #[arg(long = "mirror-to", value_name = "PROVIDER")]
        mirror_to: Vec<String>,
        /// Owning account or organisation: `PROVIDER=OWNER`, or `OWNER` for the primary
        /// (repeatable; default: your own account)
        #[arg(long = "owner", value_name = "[PROVIDER=]OWNER")]
        owners: Vec<String>,
        /// Public repos go to this provider instead (then --primary is for private ones)
        #[arg(long, value_name = "PROVIDER")]
        public_primary: Option<String>,
        /// Mirrors of public repos (repeatable; needs --public-primary)
        #[arg(long = "public-mirror-to", value_name = "PROVIDER")]
        public_mirror_to: Vec<String>,
        /// WIP snapshot policy for this namespace: private-only, all, none
        #[arg(long)]
        snapshots: Option<String>,
        /// Do not create worktrees for new remote branches automatically
        #[arg(long)]
        no_materialize: bool,
        /// Replace an existing namespace of that name
        #[arg(long)]
        replace: bool,
    },
    /// Remove a namespace from the config (its directory and repos stay on disk, unsynced)
    Remove { name: String },
}

#[derive(Subcommand)]
enum ProviderCmd {
    /// List the providers
    List,
    /// Add a provider (or change one with --replace); then `grove login ID`
    Add {
        id: String,
        /// gitea, forgejo or github
        #[arg(long)]
        kind: String,
        /// Base URL (required for gitea/forgejo)
        #[arg(long)]
        url: Option<String>,
        /// How git talks to it: https (API token) or ssh (your keys)
        #[arg(long, default_value = "https")]
        protocol: String,
        #[arg(long)]
        replace: bool,
    },
    /// Remove a provider no namespace uses
    Remove { id: String },
}

#[derive(Subcommand)]
enum SecretsCmd {
    /// Create this machine's age key and publish the public half
    Init,
    /// Encrypt FILE (inside a workspace worktree) to every machine key and store it
    Push { file: PathBuf },
    /// Decrypt stored files into the worktrees that lack them (never overwrites)
    Pull,
    /// Re-encrypt everything to the current set of machine keys
    Rekey,
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
                if let Some(why) = r["snapshots_withheld"].as_str() {
                    println!("    snapshots withheld: {why}");
                }
            }
            for p in d["not_migrated"].as_array().into_iter().flatten() {
                println!(
                    "{}  [ignored] not migrated (run grove add)",
                    p.as_str().unwrap_or("?")
                );
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

/// Opens `dir` with the launcher `with` (or the default one); `print` only shows the command.
fn open_dir(cfg: &Config, dir: &Path, with: Option<&str>, print: bool) -> Result<()> {
    use grove_core::launch::{System, argv, launchers, pick, spawn};
    let all = launchers(&cfg.open, &System);
    let l = pick(&all, &cfg.open, with).map_err(anyhow::Error::msg)?;
    let cmd = argv(l, dir, &cfg.open, &System).map_err(anyhow::Error::msg)?;
    if print {
        println!("{}", cmd.join(" "));
        return Ok(());
    }
    spawn(&cmd, dir).map_err(anyhow::Error::msg)?;
    println!("opened {} in {}", dir.display(), l.label);
    Ok(())
}

fn list_launchers(cfg: &Config) -> Result<()> {
    let all = grove_core::launch::launchers(&cfg.open, &grove_core::launch::System);
    let default = grove_core::launch::pick(&all, &cfg.open, None)
        .ok()
        .map(|l| l.name.clone());
    for l in &all {
        println!(
            "{} {:<16} {:<16} {}{}",
            if Some(&l.name) == default.as_ref() {
                "*"
            } else {
                " "
            },
            l.name,
            l.label,
            if l.available {
                l.command[0].as_str()
            } else {
                "(not installed)"
            },
            if l.terminal { "  [terminal]" } else { "" }
        );
    }
    Ok(())
}

fn config_path(arg: &Option<PathBuf>) -> Result<PathBuf> {
    arg.clone()
        .or_else(Config::default_path)
        .context("cannot determine config path (set --config)")
}

/// Saves an edited config and says what to do next.
fn save_edited(cfg: &Config, path: &Path) -> Result<()> {
    let bak = namespace::save(cfg, path)?;
    println!(
        "saved {} (previous version: {})",
        path.display(),
        bak.display()
    );
    if grove_daemon::ipc::default_socket_path().exists() {
        println!("restart the daemon to apply it: systemctl --user restart groved");
    }
    Ok(())
}

fn group_cmd(arg: &Option<PathBuf>, action: &GroupCmd) -> Result<()> {
    let path = config_path(arg)?;
    let mut cfg = load_config(arg)?;
    match action {
        GroupCmd::List => {
            for l in namespace::describe_groups(&cfg) {
                println!("{l}");
            }
            Ok(())
        }
        GroupCmd::Add {
            name,
            primary,
            mirror_to,
            owners,
            public_primary,
            public_mirror_to,
            snapshots,
            no_materialize,
            replace,
        } => {
            let snapshots = snapshots
                .as_deref()
                .map(namespace::parse_snapshots)
                .transpose()?;
            namespace::add_group(
                &mut cfg,
                namespace::GroupArgs {
                    name: name.clone(),
                    primary: primary.clone(),
                    mirror_to: mirror_to.clone(),
                    owners: owners.clone(),
                    public_primary: public_primary.clone(),
                    public_mirror_to: public_mirror_to.clone(),
                    snapshots,
                    no_materialize: *no_materialize,
                },
                *replace,
            )?;
            save_edited(&cfg, &path)?;
            let dir = cfg.root_path().join(name);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            println!("namespace {name}: {}", dir.display());
            Ok(())
        }
        GroupCmd::Remove { name } => {
            namespace::remove_group(&mut cfg, name)?;
            save_edited(&cfg, &path)?;
            println!(
                "removed namespace {name}; {} stays on disk and is no longer synced",
                cfg.root_path().join(name).display()
            );
            Ok(())
        }
    }
}

fn provider_cmd(arg: &Option<PathBuf>, action: &ProviderCmd) -> Result<()> {
    let path = config_path(arg)?;
    let mut cfg = load_config(arg)?;
    match action {
        ProviderCmd::List => {
            for l in namespace::describe_providers(&cfg) {
                println!("{l}");
            }
            Ok(())
        }
        ProviderCmd::Add {
            id,
            kind,
            url,
            protocol,
            replace,
        } => {
            let p = grove_core::config::Provider {
                kind: namespace::parse_kind(kind)?,
                protocol: namespace::parse_protocol(protocol)?,
                url: url.clone().filter(|u| !u.is_empty()),
            };
            namespace::add_provider(&mut cfg, id, p, *replace)?;
            save_edited(&cfg, &path)?;
            println!("next: `grove login {id}` (tokens from `tea`/`gh` are used if present)");
            Ok(())
        }
        ProviderCmd::Remove { id } => {
            namespace::remove_provider(&mut cfg, id)?;
            save_edited(&cfg, &path)?;
            println!("removed provider {id} (its stored token stays in the keyring)");
            Ok(())
        }
    }
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
            match workflow::repoint_mirror_origin(&cfg, &r.worktree) {
                Ok(Some(u)) => println!("origin was a read-only mirror; now {u}"),
                Ok(None) => {}
                Err(e) => println!("warning: read-only mirror check of origin: {e:#}"),
            }
            println!(
                "backup:   {} (ref {})",
                r.backup_tar.display(),
                r.backup_ref
            );
            for b in &r.linked_backups {
                println!("backup:   {} (linked worktree)", b.display());
            }
            if !r.linked.is_empty() {
                println!("linked worktrees kept:");
                for l in &r.linked {
                    println!(
                        "  {} ({})",
                        l.path.display(),
                        l.branch.as_deref().unwrap_or("detached")
                    );
                }
            }
            println!("note: shells whose cwd was inside the old checkout must `cd` again");
            Ok(())
        }
        Cmd::Sync { path, offline } => {
            let cfg = load_config(&cli.config)?;
            grove_core::snapshot::set_max_file_bytes(cfg.sync.max_file_mb * 1024 * 1024);
            // Merged pull requests, for pruning (bare layout only; best effort).
            let merged = if *offline || !cfg.sync.prune_merged || !path.join(".bare").is_dir() {
                Vec::new()
            } else {
                grove_daemon::merged::lookup(
                    &cfg,
                    path,
                    "origin",
                    &grove_daemon::visibility::RealForge,
                )
                .unwrap_or_else(|e| {
                    eprintln!("grove: cannot list merged pull requests: {e}");
                    Vec::new()
                })
            };
            let rep = sync_repo(
                path,
                &Options {
                    machine: cfg.machine.name.clone(),
                    stamp: stamp(),
                    remote: "origin".into(),
                    offline: *offline,
                    materialize: path
                        .canonicalize()
                        .ok()
                        .and_then(|p| grove_daemon::discover::group_of(&p))
                        .and_then(|g| cfg.groups.iter().find(|x| x.name == g))
                        .is_none_or(|g| g.auto_materialize),
                    push_snapshots: true,
                    merged,
                    disposable: cfg.sync.prune_disposable.clone(),
                    prune: cfg.sync.prune_merged,
                    protected: cfg.sync.protected_branches(),
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
            protocol,
            group,
            config_repo,
            force,
        } => {
            let protocol = namespace::parse_protocol(protocol)?;
            let path = cli
                .config
                .clone()
                .or_else(Config::default_path)
                .context("cannot determine config path (set --config)")?;
            let kind = namespace::parse_kind(kind)?;
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
                    protocol,
                    group: group.clone(),
                    config_repo: config_repo.clone(),
                },
                *force,
            )?;
            println!("wrote {}", path.display());
            println!(
                "next: `grove doctor` (tokens from `tea`/`gh` are used if present; else `grove login {provider}`)"
            );
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
            open,
        } => {
            let cfg = load_config(&cli.config)?;
            let wt = workflow::new_repo(&cfg, group, name, !public)?;
            println!("ready: {}", wt.display());
            if let Some(with) = open {
                let with = (!with.is_empty()).then_some(with.as_str());
                if let Err(e) = open_dir(&cfg, &wt, with, false) {
                    println!("warning: not opened: {e:#}");
                }
            }
            Ok(())
        }
        Cmd::Open {
            target,
            with,
            list,
            print,
        } => {
            let cfg = load_config(&cli.config)?;
            if *list {
                return list_launchers(&cfg);
            }
            let dir =
                grove_core::launch::resolve_target(&cfg, target).map_err(anyhow::Error::msg)?;
            open_dir(&cfg, &dir, with.as_deref(), *print)
        }
        Cmd::Group { action } => group_cmd(&cli.config, action),
        Cmd::Provider { action } => provider_cmd(&cli.config, action),
        Cmd::Import { group, names } => workflow::import(&load_config(&cli.config)?, group, names),
        Cmd::ConfigRepoInit => {
            let n = workflow::config_repo_init(&load_config(&cli.config)?)?;
            println!("config repo up to date ({n} repos registered)");
            Ok(())
        }
        Cmd::Secrets { action } => {
            let cfg = load_config(&cli.config)?;
            match action {
                SecretsCmd::Init => secrets::init(&cfg),
                SecretsCmd::Push { file } => secrets::push(&cfg, file),
                SecretsCmd::Pull => secrets::pull(&cfg),
                SecretsCmd::Rekey => secrets::rekey(&cfg),
            }
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
    for p in &rep.pruned {
        match p {
            Pruned::Removed {
                branch,
                pr,
                path,
                backup_ref,
            } => println!(
                "pruned {branch}: {}, removed {} (backup {backup_ref})",
                merged_how(*pr),
                path.display()
            ),
            Pruned::Kept { branch, pr, why } => {
                println!("kept {branch}: {} but {why}", merged_how(*pr))
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
        // Local lookup only (env, Secret Service, tea/gh): no request reaches the forge.
        for (id, pc) in &c.providers {
            match grove_providers::token::resolve(id, pc) {
                Ok((_, src)) => line(Some(true), format!("provider {id:?}: token from {src:?}")),
                Err(e) => line(None, format!("provider {id:?}: {e}")),
            }
        }
        let names: Vec<String> = c.groups.iter().map(|g| g.name.clone()).collect();
        for p in grove_daemon::discover::discover(&root, &names).not_migrated {
            line(
                None,
                format!(
                    "{}: not migrated (run grove add); the daemon ignores it",
                    p.display()
                ),
            );
        }
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
