//! Onboarding, new-repo, import and bootstrap flows (Phase 4).

use anyhow::{Context, Result, bail};
use grove_core::config::{
    Config, Group, Layout, Machine, Provider as ProviderCfg, ProviderKind, Root,
};
use grove_core::git::Git;
use grove_core::registry::{Entry, Registry, clone_bare_layout, init_bare_layout, push_initial};
use grove_providers::{MirrorSpec, NewRepo, Provider, build, token};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Forge {
    pub api: Box<dyn Provider>,
    pub login: String,
    /// Git environment that authenticates HTTPS operations with the stored token.
    pub git_env: Vec<(String, String)>,
}

pub fn connect(cfg: &Config, id: &str) -> Result<Forge> {
    let pc = cfg
        .providers
        .get(id)
        .with_context(|| format!("provider {id:?} is not in the config"))?;
    let tok = token::get(id)?;
    let api = build(pc, tok.clone());
    let login = api.test_connection()?;
    let git_env = match pc.kind {
        ProviderKind::Gitea | ProviderKind::Forgejo => vec![
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
            (
                "GIT_CONFIG_VALUE_0".into(),
                format!("Authorization: token {tok}"),
            ),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ],
        // GitHub git-over-HTTPS needs Basic auth; leave it to the user's credential helper.
        ProviderKind::Github => vec![("GIT_TERMINAL_PROMPT".into(), "0".into())],
    };
    Ok(Forge {
        api,
        login,
        git_env,
    })
}

fn group<'a>(cfg: &'a Config, name: &str) -> Result<&'a Group> {
    cfg.groups
        .iter()
        .find(|g| g.name == name)
        .with_context(|| format!("group {name:?} is not in the config"))
}

fn with_env(g: Git, envs: &[(String, String)]) -> Git {
    envs.iter().fold(g, |g, (k, v)| g.with_env(k, v))
}

pub struct InitArgs {
    pub machine: String,
    pub root: String,
    pub provider: String,
    pub kind: ProviderKind,
    pub url: Option<String>,
    pub group: String,
    pub config_repo: Option<String>,
}

pub fn init(path: &Path, a: InitArgs, force: bool) -> Result<Config> {
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        a.provider.clone(),
        ProviderCfg {
            kind: a.kind,
            url: a.url,
        },
    );
    let cfg = Config {
        machine: Machine { name: a.machine },
        root: Root { path: a.root },
        providers,
        groups: vec![Group {
            name: a.group,
            primary: a.provider.clone(),
            mirror_to: vec![],
            layout: Layout::BareWorktrees,
            auto_materialize: true,
        }],
        sync: Default::default(),
        config_repo: a.config_repo.map(|name| grove_core::config::ConfigRepo {
            provider: a.provider,
            name,
        }),
    };
    // Validate by round-tripping through the parser before writing anything.
    let text = toml::to_string_pretty(&cfg)?;
    Config::parse(&text)?;
    cfg.save(path)?;
    Ok(cfg)
}

pub fn login(cfg: &Config, id: &str, tok: &str) -> Result<String> {
    let pc = cfg
        .providers
        .get(id)
        .with_context(|| format!("provider {id:?} is not in the config"))?;
    let who = build(pc, tok.to_string()).test_connection()?;
    // Keep the secret out of the store when the env override is in use.
    token::set(id, tok)?;
    Ok(who)
}

/// Creates the remote repo, the local bare layout, the mirrors and the registry entry.
pub fn new_repo(cfg: &Config, group_name: &str, name: &str, private: bool) -> Result<PathBuf> {
    let grp = group(cfg, group_name)?;
    let dir = cfg.root_path().join(&grp.name).join(name);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    let primary = connect(cfg, &grp.primary)?;
    let remote = primary.api.create_repo(&NewRepo {
        name: name.into(),
        owner: None,
        private,
        description: String::new(),
    })?;
    println!(
        "created {}/{} on {}",
        remote.owner, remote.name, grp.primary
    );
    let wt = init_bare_layout(&dir, &remote.clone_url)?;
    push_initial(&dir, &primary.git_env)?;
    println!("pushed initial commit; worktree {}", wt.display());

    for mid in &grp.mirror_to {
        match configure_mirror(cfg, &primary, mid, &remote.owner, name, private) {
            Ok(()) => println!("push mirror to {mid} configured"),
            Err(e) => println!("warning: mirror to {mid} not configured: {e}"),
        }
    }
    register(
        cfg,
        &Entry {
            group: grp.name.clone(),
            name: name.into(),
            url: remote.clone_url,
        },
    )?;
    Ok(wt)
}

fn configure_mirror(
    cfg: &Config,
    primary: &Forge,
    mirror_id: &str,
    owner: &str,
    name: &str,
    private: bool,
) -> Result<()> {
    let m = connect(cfg, mirror_id)?;
    let target = m.api.create_repo(&NewRepo {
        name: name.into(),
        owner: None,
        private,
        description: "Mirror managed by grove".into(),
    })?;
    primary.api.configure_mirror(
        owner,
        name,
        &MirrorSpec {
            remote_url: target.clone_url,
            remote_username: m.login,
            remote_token: token::get(mirror_id)?,
            interval: "8h0m0s".into(),
            sync_on_commit: true,
        },
    )?;
    Ok(())
}

/// Lists remote repos of the group's primary that are not on disk; clones the named ones.
pub fn import(cfg: &Config, group_name: &str, wanted: &[String]) -> Result<()> {
    let grp = group(cfg, group_name)?;
    let primary = connect(cfg, &grp.primary)?;
    let gdir = cfg.root_path().join(&grp.name);
    let missing: Vec<_> = primary
        .api
        .list_repos()?
        .into_iter()
        .filter(|r| !gdir.join(&r.name).join(".bare").exists())
        .collect();
    if wanted.is_empty() {
        println!("repos on {} not yet in {}:", grp.primary, gdir.display());
        for r in &missing {
            println!(
                "  {}/{}{}",
                r.owner,
                r.name,
                if r.private { " (private)" } else { "" }
            );
        }
        println!("run `grove import {group_name} <name>...` to clone them");
        return Ok(());
    }
    for w in wanted {
        let r = missing
            .iter()
            .find(|r| &r.name == w)
            .with_context(|| format!("{w:?} is not a remote repo missing locally"))?;
        let dir = gdir.join(&r.name);
        clone_bare_layout(&r.clone_url, &dir, &primary.git_env)?;
        println!("imported {} -> {}", r.name, dir.display());
        register(
            cfg,
            &Entry {
                group: grp.name.clone(),
                name: r.name.clone(),
                url: r.clone_url.clone(),
            },
        )?;
    }
    Ok(())
}

// ---- config repo -------------------------------------------------------------------------

fn config_repo_dir() -> Result<PathBuf> {
    Ok(Config::state_dir()
        .context("cannot determine state dir")?
        .join("config-repo"))
}

/// Makes sure the local clone of the config repo exists and is current. Returns its path.
fn config_repo_open(cfg: &Config, create: bool) -> Result<(PathBuf, Forge)> {
    let cr = cfg
        .config_repo
        .as_ref()
        .context("no [config_repo] in the config")?;
    let forge = connect(cfg, &cr.provider)?;
    let dir = config_repo_dir()?;
    if !dir.join(".git").exists() {
        let exists = forge
            .api
            .list_repos()?
            .into_iter()
            .find(|r| r.name == cr.name);
        let url = match exists {
            Some(r) => r.clone_url,
            None if create => {
                let r = forge.api.create_repo(&NewRepo {
                    name: cr.name.clone(),
                    owner: None,
                    private: true,
                    description: "grove configuration (managed repos, machines)".into(),
                })?;
                r.clone_url
            }
            None => bail!(
                "config repo {:?} does not exist on {}",
                cr.name,
                cr.provider
            ),
        };
        fs::create_dir_all(dir.parent().unwrap())?;
        let g = with_env(Git::new(dir.parent().unwrap()), &forge.git_env);
        g.run(&["clone", "-q", &url, &dir.to_string_lossy()])?;
    } else {
        // An empty remote has nothing to pull yet.
        let _ = with_env(Git::new(&dir), &forge.git_env).run(&["pull", "-q", "--ff-only"]);
    }
    Ok((dir, forge))
}

fn read_registry(dir: &Path) -> Result<Registry> {
    match fs::read_to_string(dir.join("repos.toml")) {
        Ok(t) => Registry::parse(&t).map_err(anyhow::Error::msg),
        Err(_) => Ok(Registry::default()),
    }
}

fn commit_and_push(cfg: &Config, dir: &Path, forge: &Forge, msg: &str) -> Result<()> {
    let g = Git::new(dir)
        .with_env("GIT_AUTHOR_NAME", "grove")
        .with_env("GIT_AUTHOR_EMAIL", "grove@localhost")
        .with_env("GIT_COMMITTER_NAME", "grove")
        .with_env("GIT_COMMITTER_EMAIL", "grove@localhost");
    g.run(&["add", "-A"])?;
    if g.run(&["status", "--porcelain"])?.is_empty() {
        return Ok(());
    }
    let machine = &cfg.machine.name;
    g.run(&["commit", "-q", "-m", &format!("{msg} (from {machine})")])?;
    let branch = g.run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    with_env(g, &forge.git_env).run(&["push", "-q", "-u", "origin", &branch])?;
    Ok(())
}

fn write_heartbeat(cfg: &Config, dir: &Path) -> Result<()> {
    let m = dir.join("machines");
    fs::create_dir_all(&m)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let body = serde_json::json!({"machine": cfg.machine.name, "last_registered": now});
    fs::write(
        m.join(format!("{}.json", cfg.machine.name)),
        serde_json::to_string_pretty(&body)?,
    )?;
    Ok(())
}

/// Adds a repo to the config repo, if one is configured (silently skipped otherwise).
pub fn register(cfg: &Config, e: &Entry) -> Result<()> {
    if cfg.config_repo.is_none() {
        return Ok(());
    }
    let (dir, forge) = config_repo_open(cfg, true)?;
    let mut reg = read_registry(&dir)?;
    reg.upsert(e.clone());
    fs::write(dir.join("repos.toml"), reg.to_text())?;
    write_heartbeat(cfg, &dir)?;
    commit_and_push(
        cfg,
        &dir,
        &forge,
        &format!("register {}/{}", e.group, e.name),
    )
}

/// First machine: creates the config repo (if needed) and registers every repo already on disk.
pub fn config_repo_init(cfg: &Config) -> Result<usize> {
    let (dir, forge) = config_repo_open(cfg, true)?;
    let mut reg = read_registry(&dir)?;
    for grp in &cfg.groups {
        let gdir = cfg.root_path().join(&grp.name);
        let Ok(rd) = fs::read_dir(&gdir) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            let bare = e.path().join(".bare");
            if !bare.is_dir() {
                continue;
            }
            let Some(url) = Git::new(&bare).probe(&["config", "--get", "remote.origin.url"])?
            else {
                continue;
            };
            reg.upsert(Entry {
                group: grp.name.clone(),
                name: e.file_name().to_string_lossy().into_owned(),
                url,
            });
        }
    }
    fs::write(dir.join("repos.toml"), reg.to_text())?;
    write_heartbeat(cfg, &dir)?;
    commit_and_push(cfg, &dir, &forge, "update registry")?;
    Ok(reg.repos.len())
}

/// Fresh machine: materializes every repo of the config repo under the root.
pub fn bootstrap(cfg: &Config) -> Result<(usize, usize)> {
    let (dir, forge) = config_repo_open(cfg, false)?;
    let reg = read_registry(&dir)?;
    let (mut done, mut skipped) = (0, 0);
    for e in &reg.repos {
        let target = cfg.root_path().join(&e.group).join(&e.name);
        if target.join(".bare").exists() {
            skipped += 1;
            continue;
        }
        match clone_bare_layout(&e.url, &target, &forge.git_env) {
            Ok(_) => {
                println!("cloned {}/{}", e.group, e.name);
                done += 1;
            }
            Err(err) => println!("FAILED {}/{}: {err}", e.group, e.name),
        }
    }
    write_heartbeat(cfg, &dir)?;
    commit_and_push(cfg, &dir, &forge, "machine heartbeat")?;
    Ok((done, skipped))
}
