//! Snapshot privacy: WIP snapshots hold uncommitted work, so under the default `private-only`
//! policy they are pushed only when EVERY place the push can reach is known to be private: all
//! push URLs of `origin` (pushurl, several urls, pushInsteadOf) and the repo's mirror targets
//! configured for its group (forge-side push/pull mirrors copy `refs/sync/*` too). Anything
//! unknown (no or several matching providers, no token, network error, odd answer) counts as
//! public.
//!
//! Answers are cached in `<state dir>/visibility.json` (mode 0600). A "private" answer is trusted
//! for [`PRIVATE_TTL`] only; failed lookups are retried at most every [`RETRY`] seconds.
//! Residual risk: mirrors made by hand outside grove's config are not known to the daemon.

use grove_core::config::{Config, Provider as ProviderCfg, ProviderKind, SnapshotPolicy};
use grove_core::git::Git;
use grove_core::sync::git_dir_for;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// How long a "private" answer is trusted (a repo can be made public at any time).
pub const PRIVATE_TTL: u64 = 3600;
/// How long a "not private" answer (which only withholds) is trusted.
pub const OTHER_TTL: u64 = 6 * 3600;
/// Minimum wait before asking the forge again after a failed lookup.
pub const RETRY: u64 = 300;

/// A parsed remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoId {
    pub host: String,
    /// Explicit port, if any.
    pub port: Option<u16>,
    /// `http`, `https` or `ssh` (scp-like URLs are `ssh`).
    pub scheme: String,
    /// Path segments before `owner` (a forge installed under a sub-path).
    pub prefix: Vec<String>,
    pub owner: String,
    pub name: String,
}

/// Parses `https://host[:port]/owner/repo(.git)`, `ssh://git@host[:port]/owner/repo` and the
/// scp-like `git@host:owner/repo.git`. The last two path segments are owner and name.
pub fn parse_remote(url: &str) -> Option<RepoId> {
    let url = url.trim();
    let (scheme, authority, path) = match url.split_once("://") {
        Some((s, rest)) => {
            let (a, p) = rest.split_once('/')?;
            (s.to_ascii_lowercase(), a, p)
        }
        None => {
            let (a, p) = url.split_once(':')?;
            ("ssh".to_string(), a, p)
        }
    };
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match hostport.strip_prefix('[') {
        Some(v6) => {
            let (h, rest) = v6.split_once(']')?;
            (h, rest.strip_prefix(':').and_then(|p| p.parse().ok()))
        }
        None => match hostport.split_once(':') {
            Some((h, p)) if scheme != "ssh" || url.contains("://") => (h, p.parse().ok()),
            Some((h, _)) => (h, None),
            None => (hostport, None),
        },
    };
    let mut segs: Vec<&str> = path
        .split(['?', '#'])
        .next()?
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let last = segs.pop()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    let owner = segs.pop()?;
    if host.is_empty() || name.is_empty() {
        return None;
    }
    Some(RepoId {
        host: host.to_ascii_lowercase(),
        port,
        scheme,
        prefix: segs.iter().map(|s| s.to_string()).collect(),
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// Whether the provider `cfg` serves the repo behind `id`: same host, same port (http/https
/// only: an ssh port says nothing about the web port), and the provider's URL path is a prefix
/// of the repo path (not checked for GitHub, whose API URL differs from its clone URL).
fn serves(cfg: &ProviderCfg, id: &RepoId) -> bool {
    let url = cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let (scheme, authority, path) = match url {
        None => {
            return cfg.kind == ProviderKind::Github
                && id.host == "github.com"
                && (id.scheme == "ssh"
                    || id.port.is_none_or(|p| Some(p) == default_port(&id.scheme)));
        }
        Some(u) => match u.split_once("://") {
            Some((s, r)) => {
                let (a, p) = r.split_once('/').unwrap_or((r, ""));
                (s.to_ascii_lowercase(), a, p)
            }
            None => ("https".to_string(), u, ""),
        },
    };
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()),
        None => (hostport, None),
    };
    let mut host = host.to_ascii_lowercase();
    if cfg.kind == ProviderKind::Github {
        host = host.strip_prefix("api.").unwrap_or(&host).to_string();
    }
    if host != id.host {
        return false;
    }
    if id.scheme != "ssh" {
        let mine = port.or(default_port(&scheme));
        let theirs = id.port.or(default_port(&id.scheme));
        if mine != theirs {
            return false;
        }
    }
    if cfg.kind == ProviderKind::Github || id.scheme == "ssh" {
        return true;
    }
    let want: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    id.prefix.len() >= want.len() && id.prefix.iter().zip(&want).all(|(a, b)| a == b)
}

/// The one configured provider (id and config) that serves `id`. Zero or several matches are an
/// error: with several candidates the repo's visibility cannot be told.
pub fn provider_for<'a>(
    cfg: &'a Config,
    id: &RepoId,
) -> Result<(&'a str, &'a ProviderCfg), String> {
    let mut hits = cfg.providers.iter().filter(|(_, p)| serves(p, id));
    match (hits.next(), hits.next()) {
        (Some((i, p)), None) => Ok((i.as_str(), p)),
        (None, _) => Err(format!("no configured provider for host {}", id.host)),
        _ => Err(format!("several providers match host {}", id.host)),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Private,
    Public,
    /// Could not be determined; treated as public. Carries the reason.
    Unknown(String),
}

/// What a forge says about one repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Seen {
    Private,
    NotPrivate,
    /// 404: no such repo (or not visible to the token, so not public either).
    Absent,
}

/// The forge lookups the daemon needs; faked in tests.
pub trait Forge {
    fn seen(&self, id: &str, pc: &ProviderCfg, owner: &str, name: &str) -> Result<Seen, String>;
    /// The login the token belongs to (owner of repos without an `owners` entry).
    fn login(&self, id: &str, pc: &ProviderCfg) -> Result<String, String>;
}

/// The real forges: token via `token::resolve` (grove login, Secret Service, tea/gh).
pub struct RealForge;

impl RealForge {
    fn provider(id: &str, pc: &ProviderCfg) -> Result<Box<dyn grove_providers::Provider>, String> {
        let (token, _) = grove_providers::token::resolve(id, pc).map_err(|e| e.to_string())?;
        Ok(grove_providers::build(pc, token))
    }
}

impl Forge for RealForge {
    fn seen(&self, id: &str, pc: &ProviderCfg, owner: &str, name: &str) -> Result<Seen, String> {
        match Self::provider(id, pc)?.get_repo(owner, name) {
            Ok(r) if r.private => Ok(Seen::Private),
            Ok(_) => Ok(Seen::NotPrivate),
            Err(grove_providers::Error::Api { status: 404, .. }) => Ok(Seen::Absent),
            Err(e) => Err(e.to_string()),
        }
    }

    fn login(&self, id: &str, pc: &ProviderCfg) -> Result<String, String> {
        Self::provider(id, pc)?
            .test_connection()
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    seen: Seen,
    checked: u64,
}

impl Entry {
    /// A timestamp in the future (clock change, hand-edited file) counts as expired.
    fn fresh(&self, now: u64) -> bool {
        // Only a restrictive answer may live long; "private" and "absent" both permit pushes.
        let ttl = match self.seen {
            Seen::NotPrivate => OTHER_TTL,
            Seen::Private | Seen::Absent => PRIVATE_TTL,
        };
        self.checked <= now && now - self.checked < ttl
    }
}

/// Visibility cache. Owned by the daemon's single worker thread.
#[derive(Debug, Default)]
pub struct Visibility {
    path: Option<PathBuf>,
    entries: BTreeMap<String, Entry>,
    failed: HashMap<String, (u64, String)>,
    logins: HashMap<String, String>,
}

impl Visibility {
    /// Loads the cache from `path` (an unreadable or corrupt file is an empty cache).
    pub fn load(path: Option<PathBuf>) -> Visibility {
        let entries = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Visibility {
            path,
            entries,
            ..Visibility::default()
        }
    }

    fn save(&self) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let Ok(text) = serde_json::to_string_pretty(&self.entries) else {
            return;
        };
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(text.as_bytes()));
        if written.is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// What the forge says about `owner/name` on provider `pid`, from the cache or the forge.
    fn seen(
        &mut self,
        pid: &str,
        pc: &ProviderCfg,
        owner: &str,
        name: &str,
        now: u64,
        forge: &dyn Forge,
    ) -> Result<Seen, String> {
        let url = pc.url.as_deref().unwrap_or_default().trim_end_matches('/');
        let key = format!("{pid}|{url}|{owner}/{name}").to_ascii_lowercase();
        if let Some(e) = self.entries.get(&key)
            && e.fresh(now)
        {
            return Ok(e.seen);
        }
        if let Some((at, why)) = self.failed.get(&key)
            && at.saturating_sub(0) <= now
            && now - at < RETRY
        {
            return Err(why.clone());
        }
        match forge.seen(pid, pc, owner, name) {
            Ok(seen) => {
                self.failed.remove(&key);
                self.entries.insert(key, Entry { seen, checked: now });
                self.save();
                Ok(seen)
            }
            Err(why) => {
                let why = format!("lookup on {pid:?} failed: {why}");
                self.failed.insert(key, (now, why.clone()));
                Err(why)
            }
        }
    }

    /// Visibility of the repo behind one push URL.
    pub fn check_url(&mut self, cfg: &Config, url: &str, now: u64, forge: &dyn Forge) -> Verdict {
        let Some(id) = parse_remote(url) else {
            return Verdict::Unknown("push URL not understood".into());
        };
        let (pid, pc) = match provider_for(cfg, &id) {
            Ok(x) => x,
            Err(e) => return Verdict::Unknown(e),
        };
        match self.seen(pid, pc, &id.owner, &id.name, now, forge) {
            Ok(Seen::Private) => Verdict::Private,
            Ok(Seen::NotPrivate) => Verdict::Public,
            Ok(Seen::Absent) => Verdict::Unknown(format!("repo not found on {pid:?}")),
            Err(why) => Verdict::Unknown(why),
        }
    }

    /// Mirror target `name` on provider `pid`: `Ok` when it is private or does not exist.
    fn check_mirror(
        &mut self,
        cfg: &Config,
        group: &str,
        pid: &str,
        name: &str,
        now: u64,
        forge: &dyn Forge,
    ) -> Result<(), String> {
        let pc = cfg
            .providers
            .get(pid)
            .ok_or_else(|| format!("unknown mirror provider {pid:?}"))?;
        let owner = match cfg
            .groups
            .iter()
            .find(|g| g.name == group)
            .and_then(|g| g.owner(pid))
        {
            Some(o) => o.to_string(),
            None => match self.logins.get(pid) {
                Some(l) => l.clone(),
                None => {
                    let l = forge
                        .login(pid, pc)
                        .map_err(|e| format!("mirror on {pid:?}: cannot get login: {e}"))?;
                    self.logins.insert(pid.to_string(), l.clone());
                    l
                }
            },
        };
        match self.seen(pid, pc, &owner, name, now, forge) {
            Ok(Seen::NotPrivate) => Err(format!("mirror {pid}:{owner}/{name} is public")),
            Ok(_) => Ok(()),
            Err(why) => Err(format!("mirror {pid}:{owner}/{name}: {why}")),
        }
    }
}

/// Policy decision: `Ok(())` to push snapshots, `Err(reason)` to withhold them. Unknown
/// visibility counts as public.
pub fn decide(policy: SnapshotPolicy, verdict: impl FnOnce() -> Verdict) -> Result<(), String> {
    match policy {
        SnapshotPolicy::All => Ok(()),
        SnapshotPolicy::None => Err("snapshots = \"none\"".into()),
        SnapshotPolicy::PrivateOnly => match verdict() {
            Verdict::Private => Ok(()),
            Verdict::Public => Err("repo is public (snapshots = \"private-only\")".into()),
            Verdict::Unknown(why) => Err(format!(
                "visibility unknown, treated as public ({why}; snapshots = \"private-only\")"
            )),
        },
    }
}

/// Where `git push <remote>` goes: `git remote get-url --push --all` honours `pushurl`, several
/// urls and `pushInsteadOf`.
fn push_urls(repo: &Path, remote: &str) -> Vec<String> {
    Git::new(git_dir_for(repo))
        .run(&["remote", "get-url", "--push", "--all", remote])
        .map(|o| o.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

/// Whether the daemon may push WIP snapshots of `repo` (a bare-layout repo in a group).
pub fn snapshot_gate(
    cfg: &Config,
    repo: &Path,
    remote: &str,
    vis: &mut Visibility,
    now: u64,
    forge: &dyn Forge,
) -> Result<(), String> {
    let group = crate::discover::group_of(repo).unwrap_or_default();
    let mut name = None;
    decide(cfg.snapshot_policy(&group), || {
        let urls = push_urls(repo, remote);
        if urls.is_empty() {
            return Verdict::Unknown(format!("no push URL for {remote}"));
        }
        for u in &urls {
            match vis.check_url(cfg, u, now, forge) {
                Verdict::Private => {}
                other => return other,
            }
        }
        name = parse_remote(&urls[0]).map(|i| i.name);
        Verdict::Private
    })?;
    if cfg.snapshot_policy(&group) != SnapshotPolicy::PrivateOnly {
        return Ok(());
    }
    // Forge-side mirrors copy refs/sync/* to their targets, which must be private too.
    let Some(name) = name else {
        return Err("repo name unknown".into());
    };
    let Some(g) = cfg.groups.iter().find(|g| g.name == group) else {
        return Ok(());
    };
    let mut targets: Vec<String> = [g.route(true), g.route(false)]
        .into_iter()
        .flat_map(|r| r.mirror_to)
        .collect();
    targets.sort();
    targets.dedup();
    for pid in targets {
        vis.check_mirror(cfg, &group, &pid, &name, now, forge)
            .map_err(|e| format!("{e} (snapshots = \"private-only\")"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Scripted forge: answers by `owner/name` (lowercase); anything else is an error.
    #[derive(Default)]
    struct Fake {
        repos: HashMap<String, Result<Seen, String>>,
        calls: Cell<u32>,
        asked: RefCell<Vec<String>>,
    }

    impl Fake {
        fn with(mut self, key: &str, v: Result<Seen, &str>) -> Fake {
            self.repos.insert(key.into(), v.map_err(str::to_string));
            self
        }
    }

    impl Forge for Fake {
        fn seen(&self, id: &str, _: &ProviderCfg, owner: &str, name: &str) -> Result<Seen, String> {
            self.calls.set(self.calls.get() + 1);
            let key = format!("{owner}/{name}").to_ascii_lowercase();
            self.asked.borrow_mut().push(format!("{id}:{key}"));
            self.repos
                .get(&key)
                .cloned()
                .unwrap_or(Err("no such fake".into()))
        }
        fn login(&self, _: &str, _: &ProviderCfg) -> Result<String, String> {
            Ok("me".into())
        }
    }

    fn cfg() -> Config {
        Config::parse(
            r#"
[machine]
name = "m"
[root]
path = "/x"
[providers.gitea]
kind = "gitea"
url = "https://Git.Example.com:3000/"
[providers.github]
kind = "github"
[[groups]]
name = "priv"
primary = "gitea"
[[groups]]
name = "all"
primary = "gitea"
snapshots = "all"
[[groups]]
name = "off"
primary = "gitea"
snapshots = "none"
[[groups]]
name = "mir"
primary = "gitea"
mirror_to = ["github"]
owners = { github = "gh-org" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_remote_url_forms() {
        let r = |s: &str| parse_remote(s).unwrap();
        let a = r("https://github.com/Org/repo.git");
        assert_eq!(
            (a.host.as_str(), a.owner.as_str(), a.name.as_str()),
            ("github.com", "Org", "repo")
        );
        assert_eq!(a.port, None);
        let b = r("git@GitHub.com:Org/repo.git");
        assert_eq!(
            (b.host.as_str(), b.scheme.as_str(), b.port),
            ("github.com", "ssh", None)
        );
        let c = r("ssh://git@git.example.com:2222/o/r");
        assert_eq!((c.host.as_str(), c.port), ("git.example.com", Some(2222)));
        let d = r("https://u:p@h.io:3000/sub/o/r.git/");
        assert_eq!(
            (d.port, d.prefix.clone(), d.owner.as_str()),
            (Some(3000), vec!["sub".to_string()], "o")
        );
        assert!(parse_remote("/local/path/repo.git").is_none());
        assert!(parse_remote("https://host/onlyone").is_none());
    }

    #[test]
    fn matches_providers_by_host_port_and_path_prefix() {
        let c = cfg();
        let m = |u: &str| provider_for(&c, &parse_remote(u).unwrap()).map(|(i, _)| i.to_string());
        assert_eq!(m("https://git.example.com:3000/o/r").unwrap(), "gitea");
        assert_eq!(m("ssh://git@git.example.com:22/o/r").unwrap(), "gitea");
        assert_eq!(m("git@git.example.com:o/r.git").unwrap(), "gitea");
        assert!(m("https://git.example.com/o/r").is_err(), "other web port");
        assert_eq!(m("https://github.com/o/r").unwrap(), "github");
        assert_eq!(m("git@github.com:o/r.git").unwrap(), "github");
        assert!(m("https://gitlab.com/o/r").is_err());

        let sub = Config::parse(
            "[machine]\nname=\"m\"\n[root]\npath=\"/x\"\n[providers.a]\nkind=\"gitea\"\nurl=\"https://h.io/a\"\n[providers.b]\nkind=\"gitea\"\nurl=\"https://h.io/b\"\n",
        )
        .unwrap();
        let m = |u: &str| provider_for(&sub, &parse_remote(u).unwrap()).map(|(i, _)| i.to_string());
        assert_eq!(m("https://h.io/a/o/r").unwrap(), "a");
        assert_eq!(m("https://h.io/b/o/r").unwrap(), "b");
        assert!(m("https://h.io/o/r").is_err());

        // Two providers for the same host: ambiguous, so unknown.
        let dup = Config::parse(
            "[machine]\nname=\"m\"\n[root]\npath=\"/x\"\n[providers.a]\nkind=\"gitea\"\nurl=\"https://h.io\"\n[providers.b]\nkind=\"forgejo\"\nurl=\"https://h.io\"\n",
        )
        .unwrap();
        let e = provider_for(&dup, &parse_remote("https://h.io/o/r").unwrap()).unwrap_err();
        assert!(e.contains("several"));
        let mut v = Visibility::default();
        let fake = Fake::default().with("o/r", Ok(Seen::Private));
        assert!(matches!(
            v.check_url(&dup, "https://h.io/o/r", 0, &fake),
            Verdict::Unknown(_)
        ));
        assert_eq!(fake.calls.get(), 0);
    }

    #[test]
    fn unknown_visibility_is_public_and_withholds_snapshots() {
        let c = cfg();
        let mut v = Visibility::default();
        let boom = Fake::default();
        for url in [
            "https://gitlab.com/o/r.git",
            "/some/path",
            "https://github.com/o/r",
        ] {
            let verdict = v.check_url(&c, url, 1000, &boom);
            assert!(matches!(verdict, Verdict::Unknown(_)), "{url}: {verdict:?}");
            assert!(decide(SnapshotPolicy::PrivateOnly, || verdict).is_err());
        }
        let f = Fake::default()
            .with("o/pub", Ok(Seen::NotPrivate))
            .with("o/priv", Ok(Seen::Private))
            .with("o/gone", Ok(Seen::Absent));
        let p = v.check_url(&c, "https://github.com/o/pub", 1000, &f);
        assert_eq!(p, Verdict::Public);
        assert!(decide(SnapshotPolicy::PrivateOnly, || p).is_err());
        let q = v.check_url(&c, "https://github.com/o/priv", 1000, &f);
        assert_eq!(q, Verdict::Private);
        assert!(decide(SnapshotPolicy::PrivateOnly, || q).is_ok());
        assert!(matches!(
            v.check_url(&c, "https://github.com/o/gone", 1000, &f),
            Verdict::Unknown(_)
        ));
    }

    #[test]
    fn policies_all_and_none_do_not_consult_the_forge() {
        let boom = || panic!("must not look up visibility");
        assert!(decide(SnapshotPolicy::All, boom).is_ok());
        assert!(decide(SnapshotPolicy::None, boom).is_err());
    }

    #[test]
    fn private_answers_expire_after_an_hour_and_failures_are_throttled() {
        let c = cfg();
        let mut v = Visibility::default();
        let f = Fake::default().with("o/r", Ok(Seen::Private));
        let url = "https://github.com/o/r";
        assert_eq!(v.check_url(&c, url, 100, &f), Verdict::Private);
        assert_eq!(
            v.check_url(&c, url, 100 + PRIVATE_TTL - 1, &f),
            Verdict::Private
        );
        assert_eq!(f.calls.get(), 1);
        assert_eq!(
            v.check_url(&c, url, 100 + PRIVATE_TTL, &f),
            Verdict::Private
        );
        assert_eq!(f.calls.get(), 2);
        // A timestamp from the future is expired.
        assert_eq!(v.check_url(&c, url, 50, &f), Verdict::Private);
        assert_eq!(f.calls.get(), 3);
        // A "not private" answer is kept longer.
        let f2 = Fake::default().with("o/p", Ok(Seen::NotPrivate));
        v.check_url(&c, "https://github.com/o/p", 0, &f2);
        v.check_url(&c, "https://github.com/o/p", OTHER_TTL - 1, &f2);
        assert_eq!(f2.calls.get(), 1);
        // An "absent" mirror answer permits pushing, so it gets the short TTL.
        let f3 = Fake::default().with("o/a", Ok(Seen::Absent));
        v.check_url(&c, "https://github.com/o/a", 0, &f3);
        v.check_url(&c, "https://github.com/o/a", PRIVATE_TTL, &f3);
        assert_eq!(f3.calls.get(), 2);
        // After the TTL a failing refresh does not fall back to the stale answer.
        let bad = Fake::default().with("o/r", Err("down"));
        let t = 100 + 10 * PRIVATE_TTL;
        assert!(matches!(v.check_url(&c, url, t, &bad), Verdict::Unknown(_)));
        assert!(matches!(
            v.check_url(&c, url, t + 1, &bad),
            Verdict::Unknown(_)
        ));
        assert_eq!(bad.calls.get(), 1, "retry is throttled");
        assert!(matches!(
            v.check_url(&c, url, t + RETRY, &bad),
            Verdict::Unknown(_)
        ));
        assert_eq!(bad.calls.get(), 2);
    }

    #[test]
    fn cache_keys_include_provider_and_are_case_normalised() {
        let c = cfg();
        let mut v = Visibility::default();
        let f = Fake::default().with("o/r", Ok(Seen::Private));
        v.check_url(&c, "https://github.com/O/R.git", 0, &f);
        v.check_url(&c, "git@github.com:o/r", 0, &f);
        assert_eq!(f.calls.get(), 1);
        v.check_url(&c, "https://git.example.com:3000/o/r", 0, &f);
        assert_eq!(
            f.calls.get(),
            2,
            "same owner/name on another provider is another entry"
        );
    }

    #[test]
    fn cache_persists_in_the_state_dir_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("grove-vis-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("visibility.json");
        let c = cfg();
        let f = Fake::default().with("o/r", Ok(Seen::NotPrivate));
        let mut v = Visibility::load(Some(file.clone()));
        assert_eq!(
            v.check_url(&c, "https://github.com/o/r", 50, &f),
            Verdict::Public
        );
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let never = Fake::default();
        let mut again = Visibility::load(Some(file));
        assert_eq!(
            again.check_url(&c, "https://github.com/o/r", 60, &never),
            Verdict::Public
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn mk(base: &Path, group: &str, urls: &[&str], push: &[&str]) -> PathBuf {
        let repo = base.join(group).join("r");
        let bare = repo.join(".bare");
        std::fs::create_dir_all(&bare).unwrap();
        let g = Git::new(&bare);
        g.run(&["init", "-q", "--bare"]).unwrap();
        for (i, u) in urls.iter().enumerate() {
            if i == 0 {
                g.run(&["remote", "add", "origin", u]).unwrap();
            } else {
                g.run(&["remote", "set-url", "--add", "origin", u]).unwrap();
            }
        }
        for u in push {
            g.run(&["remote", "set-url", "--push", "--add", "origin", u])
                .unwrap();
        }
        repo
    }

    #[test]
    fn gate_uses_group_policy_and_every_push_url() {
        let c = cfg();
        let base = std::env::temp_dir().join(format!("grove-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let f = Fake::default()
            .with("o/r", Ok(Seen::Private))
            .with("o/open", Ok(Seen::NotPrivate));
        let mut v = Visibility::default();
        let gate = |repo: &Path, v: &mut Visibility| snapshot_gate(&c, repo, "origin", v, 0, &f);
        let ok = "https://git.example.com:3000/o/r.git";
        assert!(gate(&mk(&base, "priv", &[ok], &[]), &mut v).is_ok());
        assert!(gate(&mk(&base, "off", &[ok], &[]), &mut v).is_err());
        assert!(
            gate(
                &mk(&base, "all", &["https://unknown.example/o/r"], &[]),
                &mut v
            )
            .is_ok()
        );
        // Fetch URL private, but the push goes to a public repo (pushurl).
        let e = gate(
            &mk(&base, "priv2", &[ok], &["https://github.com/o/open"]),
            &mut v,
        );
        assert!(e.is_err());
        // Private-only without any remote: unknown, withheld.
        let bare = base.join("priv/none/.bare");
        std::fs::create_dir_all(&bare).unwrap();
        Git::new(&bare).run(&["init", "-q", "--bare"]).unwrap();
        assert!(
            gate(&base.join("priv/none"), &mut v)
                .unwrap_err()
                .contains("unknown")
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn gate_checks_all_push_urls_and_push_instead_of() {
        let c = cfg();
        let base = std::env::temp_dir().join(format!("grove-gate2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let f = Fake::default()
            .with("o/r", Ok(Seen::Private))
            .with("o/open", Ok(Seen::NotPrivate));
        let mut v = Visibility::default();
        let ok = "https://git.example.com:3000/o/r.git";
        let open = "https://github.com/o/open";
        // Group "priv": private fetch URL + public pushurl => withheld; both private => allowed.
        let both = mk(&base, "priv", &[ok], &[ok, "https://github.com/o/r"]);
        assert!(snapshot_gate(&c, &both, "origin", &mut v, 0, &f).is_ok());
        std::fs::remove_dir_all(base.join("priv")).unwrap();
        let leaky = mk(&base, "priv", &[ok, open], &[]);
        let e = snapshot_gate(&c, &leaky, "origin", &mut v, 0, &f).unwrap_err();
        assert!(e.contains("public"), "{e}");
        // pushInsteadOf rewrites the push URL to a public repo.
        let g = Git::new(leaky.join(".bare"));
        g.run(&["remote", "set-url", "--delete", "origin", open])
            .unwrap();
        assert!(snapshot_gate(&c, &leaky, "origin", &mut v, 0, &f).is_ok());
        g.run(&["config", &format!("url.{open}.pushInsteadOf"), ok])
            .unwrap();
        assert!(snapshot_gate(&c, &leaky, "origin", &mut v, 0, &f).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn gate_requires_mirror_targets_to_be_private_or_absent() {
        let c = cfg();
        let base = std::env::temp_dir().join(format!("grove-gate3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = mk(&base, "mir", &["https://git.example.com:3000/o/r.git"], &[]);
        let gate = |f: &Fake| {
            let mut v = Visibility::default();
            snapshot_gate(&c, &repo, "origin", &mut v, 0, f)
        };
        let base_f = || Fake::default().with("o/r", Ok(Seen::Private));
        // No mirror (404): fine. Mirror owner comes from `owners`.
        let f = base_f().with("gh-org/r", Ok(Seen::Absent));
        assert!(gate(&f).is_ok());
        assert!(f.asked.borrow().iter().any(|a| a == "github:gh-org/r"));
        assert!(gate(&base_f().with("gh-org/r", Ok(Seen::Private))).is_ok());
        let e = gate(&base_f().with("gh-org/r", Ok(Seen::NotPrivate))).unwrap_err();
        assert!(e.contains("mirror") && e.contains("public"), "{e}");
        let e = gate(&base_f().with("gh-org/r", Err("down"))).unwrap_err();
        assert!(e.contains("mirror"), "{e}");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
