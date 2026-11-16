//! Onboarding, new-repo, import and bootstrap flows (Phase 4).

use anyhow::{Context, Result, bail};
use grove_core::config::{
    Config, Group, Layout, Machine, Protocol, Provider as ProviderCfg, ProviderKind, Root,
};
use grove_core::git::Git;
use grove_core::registry::{Entry, Registry, clone_bare_layout, init_bare_layout, push_initial};
use grove_providers::{MirrorSpec, NewRepo, Provider, PullMirrorSpec, RemoteRepo, build, token};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Forge {
    pub api: Box<dyn Provider>,
    pub login: String,
    /// API token (also used to authorise mirrors that read from this forge).
    pub token: String,
    /// Git environment that authenticates HTTPS operations with the stored token.
    pub git_env: Vec<(String, String)>,
    pub protocol: Protocol,
}

pub fn connect(cfg: &Config, id: &str) -> Result<Forge> {
    let pc = cfg
        .providers
        .get(id)
        .with_context(|| format!("provider {id:?} is not in the config"))?;
    let (tok, _) = token::resolve(id, pc)?;
    let api = build(pc, tok.clone());
    let login = api.test_connection()?;
    let git_env = if pc.protocol == Protocol::Ssh {
        // The user's SSH keys/agent authenticate; never prompt.
        ssh_env()
    } else {
        match pc.kind {
            ProviderKind::Gitea | ProviderKind::Forgejo => vec![
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
                (
                    "GIT_CONFIG_VALUE_0".into(),
                    format!("Authorization: token {tok}"),
                ),
                ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ],
            // GitHub git-over-HTTPS takes the token as Basic auth (any user name works).
            ProviderKind::Github => vec![
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
                (
                    "GIT_CONFIG_VALUE_0".into(),
                    format!(
                        "Authorization: Basic {}",
                        base64(&format!("x-access-token:{tok}"))
                    ),
                ),
                ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ],
        }
    };
    Ok(Forge {
        api,
        login,
        token: tok,
        git_env,
        protocol: pc.protocol,
    })
}

/// Git environment for SSH remotes: the user's keys/agent authenticate and nothing prompts.
/// `BatchMode` is only forced when the user has no ssh command of their own.
fn ssh_env() -> Vec<(String, String)> {
    let mut env = vec![("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())];
    let custom = std::env::var_os("GIT_SSH_COMMAND").is_some()
        || std::env::var_os("GIT_SSH").is_some()
        || std::process::Command::new("git")
            .args(["config", "--get", "core.sshCommand"])
            .output()
            .is_ok_and(|o| o.status.success() && !o.stdout.is_empty());
    if !custom {
        env.push(("GIT_SSH_COMMAND".into(), "ssh -o BatchMode=yes".into()));
    }
    env
}

/// The group's owner on `provider`, or `None` when that is the authenticated user itself
/// (personal repos use `/user/repos`, not the org endpoint).
fn org_owner(grp: &Group, provider: &str, login: &str) -> Option<String> {
    grp.owner(provider)
        .filter(|o| !o.eq_ignore_ascii_case(login))
        .map(str::to_string)
}

/// Standard base64 with padding (enough for an auth header; avoids a dependency).
fn base64(s: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in s.as_bytes().chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
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
    pub protocol: Protocol,
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
            protocol: a.protocol,
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
            snapshots: None,
            owners: Default::default(),
            public: None,
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
    let route = grp.route(private);
    let primary = connect(cfg, &route.primary)?;
    let remote = primary.api.create_repo(&NewRepo {
        name: name.into(),
        owner: org_owner(grp, &route.primary, &primary.login),
        private,
        description: String::new(),
    })?;
    println!(
        "created {}/{} on {}",
        remote.owner, remote.name, route.primary
    );
    let local_url = remote.git_url(primary.protocol)?.to_string();
    let wt = init_bare_layout(&dir, &local_url)?;
    push_initial(&dir, &primary.git_env)?;
    println!("pushed initial commit; worktree {}", wt.display());

    for mid in &route.mirror_to {
        match configure_mirror(cfg, grp, &primary, &remote, mid) {
            Ok(()) => println!("mirror to {mid} configured"),
            Err(e) => println!("warning: mirror to {mid} not configured: {e}"),
        }
    }
    register(
        cfg,
        &Entry {
            group: grp.name.clone(),
            name: name.into(),
            url: local_url,
        },
    )?;
    Ok(wt)
}

/// Mirrors `remote` (on `primary`) to `mirror_id`: a push mirror from the primary when it can
/// do that (Gitea/Forgejo), otherwise a pull mirror created on the target (e.g. GitHub -> Gitea).
fn configure_mirror(
    cfg: &Config,
    grp: &Group,
    primary: &Forge,
    remote: &RemoteRepo,
    mirror_id: &str,
) -> Result<()> {
    let m = connect(cfg, mirror_id)?;
    let owner = org_owner(grp, mirror_id, &m.login);
    if primary.api.capabilities().push_mirror {
        let target = m.api.create_repo(&NewRepo {
            name: remote.name.clone(),
            owner,
            private: remote.private,
            description: "Mirror managed by grove".into(),
        })?;
        primary.api.configure_mirror(
            &remote.owner,
            &remote.name,
            &MirrorSpec {
                remote_url: target.clone_url,
                remote_username: m.login.clone(),
                remote_token: m.token.clone(),
                interval: "8h0m0s".into(),
                sync_on_commit: true,
            },
        )?;
    } else {
        // Credentials are only handed to the mirror when the source is private.
        let source_auth = remote
            .private
            .then(|| (primary.login.clone(), primary.token.clone()));
        m.api.create_pull_mirror(&PullMirrorSpec {
            name: remote.name.clone(),
            owner,
            private: remote.private,
            source_url: remote.clone_url.clone(),
            source_auth,
            interval: "8h0m0s".into(),
        })?;
    }
    Ok(())
}

/// Remote repos of one route that belong to the group: owned by `owner` (case-insensitive, as
/// forge logins are) and, when the group splits routes by visibility, of that visibility.
fn select(repos: Vec<RemoteRepo>, owner: &str, private: Option<bool>) -> Vec<RemoteRepo> {
    repos
        .into_iter()
        .filter(|r| r.owner.eq_ignore_ascii_case(owner))
        .filter(|r| private.is_none_or(|p| r.private == p))
        .collect()
}

/// `host/owner/name` (lowercase) of a git remote URL in https, `ssh://` or scp-like form.
fn remote_key(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    let (host, path) = match rest.split_once(['/', ':']) {
        Some((h, p)) => (h, p),
        None => return None,
    };
    // `host:2222/owner/repo` (ssh with a port): drop the port.
    let path = match path.split_once('/') {
        Some((port, p)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => p,
        _ => path,
    };
    let path = path.trim_matches('/').trim_end_matches(".git");
    let (owner, name) = path.rsplit_once('/')?;
    Some(format!("{host}/{owner}/{name}").to_lowercase())
}

/// What is already in a group directory: bare-layout repos by lowercase directory name and by
/// the owner/name their `origin` points to.
struct Local {
    names: std::collections::HashSet<String>,
    origins: std::collections::HashSet<String>,
}

impl Local {
    fn scan(gdir: &Path) -> Local {
        let mut l = Local {
            names: Default::default(),
            origins: Default::default(),
        };
        let Ok(rd) = fs::read_dir(gdir) else { return l };
        for e in rd.filter_map(Result::ok) {
            let bare = e.path().join(".bare");
            if !bare.exists() {
                continue;
            }
            l.names
                .insert(e.file_name().to_string_lossy().to_lowercase());
            if let Ok(Some(url)) = Git::new(&bare).probe(&["config", "--get", "remote.origin.url"])
                && let Some(k) = remote_key(&url)
            {
                l.origins.insert(k);
            }
        }
        l
    }

    /// Present when a repo of that name exists (case-insensitively: forge names are, and so are
    /// many file systems) or an existing repo's origin is this very remote.
    fn has(&self, r: &RemoteRepo) -> bool {
        self.names.contains(&r.name.to_lowercase())
            || [&r.clone_url, &r.ssh_url]
                .iter()
                .filter_map(|u| remote_key(u))
                .any(|k| self.origins.contains(&k))
    }
}

/// Lists the group's remote repos that are not on disk; clones the named ones.
pub fn import(cfg: &Config, group_name: &str, wanted: &[String]) -> Result<()> {
    let grp = group(cfg, group_name)?;
    // (provider id, visibility filter): one side per route.
    let sides = match &grp.public {
        Some(p) => vec![
            (grp.primary.clone(), Some(true)),
            (p.primary.clone(), Some(false)),
        ],
        None => vec![(grp.primary.clone(), None)],
    };
    let gdir = cfg.root_path().join(&grp.name);
    // Both routes of a split group are queried at the same time.
    let scanned: Vec<Result<(String, Forge, Vec<RemoteRepo>)>> = std::thread::scope(|sc| {
        let hs: Vec<_> = sides
            .iter()
            .map(|(pid, vis)| {
                let grp = &grp;
                sc.spawn(move || {
                    let f = connect(cfg, pid)?;
                    let owner = grp.owner(pid).unwrap_or(&f.login).to_string();
                    let repos = select(f.api.list_owner_repos(&owner, &f.login)?, &owner, *vis);
                    Ok((pid.clone(), f, repos))
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("provider scan panicked")))
            })
            .collect()
    });
    let mut forges = Vec::new();
    let mut missing: Vec<(RemoteRepo, usize)> = Vec::new();
    let local = Local::scan(&gdir);
    for res in scanned {
        let (pid, f, repos) = res?;
        for r in repos {
            if !local.has(&r) && !missing.iter().any(|(m, _)| m.name == r.name) {
                missing.push((r, forges.len()));
            }
        }
        forges.push((pid, f));
    }
    if wanted.is_empty() {
        println!("repos of group {} not yet in {}:", grp.name, gdir.display());
        for (r, i) in &missing {
            println!(
                "  {}/{} on {}{}",
                r.owner,
                r.name,
                forges[*i].0,
                if r.private { " (private)" } else { "" }
            );
        }
        println!("run `grove import {group_name} <name>...` to clone them");
        return Ok(());
    }
    // One broken repo must not stop the others: collect failures and report them at the end.
    let mut failed: Vec<String> = Vec::new();
    for w in wanted {
        let Some((r, i)) = missing.iter().find(|(r, _)| &r.name == w) else {
            println!("FAILED {w}: not a remote repo missing locally");
            failed.push(w.clone());
            continue;
        };
        let forge = &forges[*i].1;
        let res = (|| -> Result<()> {
            let url = r.git_url(forge.protocol)?.to_string();
            let dir = gdir.join(&r.name);
            if let Err(e) = clone_bare_layout(&url, &dir, &forge.git_env) {
                // Leave no empty directory behind (it only goes away when nothing was cloned).
                let _ = fs::remove_dir(&dir);
                return Err(e.into());
            }
            println!("imported {} -> {}", r.name, dir.display());
            register(
                cfg,
                &Entry {
                    group: grp.name.clone(),
                    name: r.name.clone(),
                    url,
                },
            )
        })();
        if let Err(e) = res {
            println!("FAILED {w}: {e:#}");
            failed.push(w.clone());
        }
    }
    if !failed.is_empty() {
        bail!(
            "{} of {} imports failed: {}",
            failed.len(),
            wanted.len(),
            failed.join(", ")
        );
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
pub(crate) fn config_repo_open(cfg: &Config, create: bool) -> Result<(PathBuf, Forge)> {
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
            Some(r) => r.git_url(forge.protocol)?.to_string(),
            None if create => {
                let r = forge.api.create_repo(&NewRepo {
                    name: cr.name.clone(),
                    owner: None,
                    private: true,
                    description: "grove configuration (managed repos, machines)".into(),
                })?;
                r.git_url(forge.protocol)?.to_string()
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

pub(crate) fn commit_and_push(cfg: &Config, dir: &Path, forge: &Forge, msg: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn r(owner: &str, name: &str, private: bool) -> RemoteRepo {
        RemoteRepo {
            owner: owner.into(),
            name: name.into(),
            private,
            clone_url: format!("u/{name}.git"),
            ssh_url: String::new(),
            default_branch: "main".into(),
        }
    }

    #[test]
    fn remote_key_normalises_every_url_form() {
        let want = Some("github.com/dasemu/synapse".to_string());
        assert_eq!(remote_key("git@github.com:Dasemu/Synapse.git"), want);
        assert_eq!(remote_key("https://github.com/Dasemu/Synapse.git"), want);
        assert_eq!(remote_key("ssh://git@github.com/dasemu/synapse"), want);
        assert_eq!(
            remote_key("ssh://gitea@git.example.es:2222/dasemu/x.git").as_deref(),
            Some("git.example.es/dasemu/x")
        );
        assert_eq!(remote_key("nonsense"), None);
    }

    #[test]
    fn local_repos_match_by_name_or_origin_ignoring_case() {
        let d = std::env::temp_dir().join(format!("grove-local-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        // `synapse` (lowercase dir) whose origin is Dasemu/Synapse; `other` with a renamed origin.
        for (dir, origin) in [
            ("synapse", "git@github.com:Dasemu/Synapse.git"),
            ("renamed", "git@github.com:me/old-name.git"),
        ] {
            let bare = d.join(dir).join(".bare");
            fs::create_dir_all(&bare).unwrap();
            let g = Git::new(&bare);
            g.run(&["init", "-q", "--bare"]).unwrap();
            g.run(&["remote", "add", "origin", origin]).unwrap();
        }
        let l = Local::scan(&d);
        let mut syn = r("Dasemu", "Synapse", false);
        syn.clone_url = "https://github.com/Dasemu/Synapse.git".into();
        assert!(l.has(&syn));
        // Different directory name, same origin.
        let mut moved = r("me", "new-name", false);
        moved.ssh_url = "git@github.com:me/old-name.git".into();
        assert!(l.has(&moved));
        assert!(!l.has(&r("me", "absent", false)));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i), o);
        }
    }

    #[test]
    fn own_login_is_not_an_org_owner() {
        let mut g: Group = toml::from_str("name='g'\nprimary='p'").unwrap();
        assert_eq!(org_owner(&g, "p", "me"), None);
        g.owners.insert("p".into(), "Me".into());
        assert_eq!(org_owner(&g, "p", "me"), None);
        g.owners.insert("p".into(), "Org".into());
        assert_eq!(org_owner(&g, "p", "me").as_deref(), Some("Org"));
    }

    #[test]
    fn select_filters_by_owner_and_visibility() {
        let all = || {
            vec![
                r("dasemu", "priv", true),
                r("Dasemu", "pub", false),
                r("Zenvairo", "org", true),
            ]
        };
        let names = |v: Vec<RemoteRepo>| v.into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(names(select(all(), "dasemu", None)), ["priv", "pub"]);
        assert_eq!(names(select(all(), "dasemu", Some(true))), ["priv"]);
        assert_eq!(names(select(all(), "DASEMU", Some(false))), ["pub"]);
        assert_eq!(names(select(all(), "zenvairo", None)), ["org"]);
    }
}
