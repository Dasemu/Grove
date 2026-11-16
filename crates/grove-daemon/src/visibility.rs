//! Snapshot privacy: WIP snapshots hold uncommitted work, so under the default `private-only`
//! policy they are pushed only to repos the forge reports as private. Anything unknown (no
//! matching provider, no token, network error, odd answer) is treated as public.
//!
//! Answers are cached in `<state dir>/visibility.json` and refreshed every [`TTL`] seconds;
//! failed lookups are retried at most every [`RETRY`] seconds.

use grove_core::config::{Config, Provider as ProviderCfg, ProviderKind, SnapshotPolicy};
use grove_core::git::Git;
use grove_core::sync::git_dir_for;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// How long a successful answer is trusted.
pub const TTL: u64 = 6 * 3600;
/// Minimum wait before asking the forge again after a failed lookup.
pub const RETRY: u64 = 300;

/// Host (lowercase, no port) and `owner/name` of a remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoId {
    pub host: String,
    pub owner: String,
    pub name: String,
}

/// Parses `https://host[:port]/owner/repo(.git)`, `ssh://git@host[:port]/owner/repo` and the
/// scp-like `git@host:owner/repo.git`. The last two path segments are owner and name.
pub fn parse_remote(url: &str) -> Option<RepoId> {
    let url = url.trim();
    let (authority, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => url.split_once(':')?,
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?, // [::1]:22
        None => host.split(':').next()?,
    };
    let mut segs: Vec<&str> = path
        .split(['/', '?', '#'])
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
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// Host (lowercase, no port) a provider answers for. GitHub without a URL is `github.com`; a
/// leading `api.` is dropped so API URLs map to the clone host.
fn provider_host(cfg: &ProviderCfg) -> Option<String> {
    let url = cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let Some(url) = url else {
        return (cfg.kind == ProviderKind::Github).then(|| "github.com".to_string());
    };
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    let host = auth.split(':').next()?.to_ascii_lowercase();
    let host = match cfg.kind {
        ProviderKind::Github => host.strip_prefix("api.").unwrap_or(&host).to_string(),
        _ => host,
    };
    (!host.is_empty()).then_some(host)
}

/// The configured provider (id and config) that serves `host`.
pub fn provider_for<'a>(cfg: &'a Config, host: &str) -> Option<(&'a str, &'a ProviderCfg)> {
    cfg.providers
        .iter()
        .find(|(_, p)| provider_host(p).as_deref() == Some(host))
        .map(|(id, p)| (id.as_str(), p))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Private,
    Public,
    /// Could not be determined; treated as public. Carries the reason.
    Unknown(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    private: bool,
    checked: u64,
}

/// Asks the forge whether `owner/name` is private: `(provider id, provider config, owner, name)`.
pub type Fetch<'a> = &'a dyn Fn(&str, &ProviderCfg, &str, &str) -> Result<bool, String>;

/// The real lookup: token via `token::resolve` (grove login, Secret Service, tea/gh), then
/// `GET /repos/{owner}/{repo}`.
pub fn forge_fetch(id: &str, pc: &ProviderCfg, owner: &str, name: &str) -> Result<bool, String> {
    let (token, _) = grove_providers::token::resolve(id, pc).map_err(|e| e.to_string())?;
    let p = grove_providers::build(pc, token);
    p.get_repo(owner, name)
        .map(|r| r.private)
        .map_err(|e| e.to_string())
}

/// Visibility cache. Owned by the daemon's single worker thread.
#[derive(Debug, Default)]
pub struct Visibility {
    path: Option<PathBuf>,
    entries: BTreeMap<String, Entry>,
    failed: HashMap<String, (u64, String)>,
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
            failed: HashMap::new(),
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if let Ok(text) = serde_json::to_string_pretty(&self.entries)
            && std::fs::write(&tmp, text).is_ok()
        {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// Visibility of the repo behind `origin_url`, from the cache or the forge.
    pub fn check(&mut self, cfg: &Config, origin_url: &str, now: u64, fetch: Fetch) -> Verdict {
        let Some(id) = parse_remote(origin_url) else {
            return Verdict::Unknown("origin URL not understood".into());
        };
        let Some((pid, pc)) = provider_for(cfg, &id.host) else {
            return Verdict::Unknown(format!("no configured provider for host {}", id.host));
        };
        let key = format!("{}/{}/{}", id.host, id.owner, id.name).to_ascii_lowercase();
        let verdict = |private| {
            if private {
                Verdict::Private
            } else {
                Verdict::Public
            }
        };
        if let Some(e) = self.entries.get(&key)
            && now.saturating_sub(e.checked) < TTL
        {
            return verdict(e.private);
        }
        if let Some((at, why)) = self.failed.get(&key)
            && now.saturating_sub(*at) < RETRY
        {
            return Verdict::Unknown(why.clone());
        }
        match fetch(pid, pc, &id.owner, &id.name) {
            Ok(private) => {
                self.failed.remove(&key);
                self.entries.insert(
                    key,
                    Entry {
                        private,
                        checked: now,
                    },
                );
                self.save();
                verdict(private)
            }
            Err(why) => {
                let why = format!("lookup on {pid:?} failed: {why}");
                self.failed.insert(key, (now, why.clone()));
                Verdict::Unknown(why)
            }
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

/// Whether the daemon may push WIP snapshots of `repo` (a bare-layout repo in a group).
pub fn snapshot_gate(
    cfg: &Config,
    repo: &Path,
    remote: &str,
    vis: &mut Visibility,
    now: u64,
    fetch: Fetch,
) -> Result<(), String> {
    let group = crate::discover::group_of(repo).unwrap_or_default();
    decide(cfg.snapshot_policy(&group), || {
        let url = Git::new(git_dir_for(repo))
            .probe(&["config", "--get", &format!("remote.{remote}.url")])
            .ok()
            .flatten();
        match url {
            Some(u) => vis.check(cfg, &u, now, fetch),
            None => Verdict::Unknown(format!("no {remote} remote")),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

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
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_remote_url_forms() {
        let id = |h: &str, o: &str, n: &str| {
            Some(RepoId {
                host: h.into(),
                owner: o.into(),
                name: n.into(),
            })
        };
        assert_eq!(
            parse_remote("https://github.com/Org/repo.git"),
            id("github.com", "Org", "repo")
        );
        assert_eq!(
            parse_remote("git@GitHub.com:Org/repo.git"),
            id("github.com", "Org", "repo")
        );
        assert_eq!(
            parse_remote("ssh://git@git.example.com:2222/o/r"),
            id("git.example.com", "o", "r")
        );
        assert_eq!(
            parse_remote("https://u:p@h.io:3000/sub/o/r.git/"),
            id("h.io", "o", "r")
        );
        assert_eq!(parse_remote("/local/path/repo.git"), None);
        assert_eq!(parse_remote("https://host/onlyone"), None);
    }

    #[test]
    fn matches_providers_by_host() {
        let c = cfg();
        assert_eq!(provider_for(&c, "git.example.com").unwrap().0, "gitea");
        assert_eq!(provider_for(&c, "github.com").unwrap().0, "github");
        assert!(provider_for(&c, "gitlab.com").is_none());
    }

    #[test]
    fn unknown_visibility_is_public_and_withholds_snapshots() {
        let c = cfg();
        let mut v = Visibility::default();
        let boom = |_: &str, _: &ProviderCfg, _: &str, _: &str| Err("no token".to_string());
        // Unmatched host, unparsable URL and a failing lookup are all "unknown".
        for url in [
            "https://gitlab.com/o/r.git",
            "/some/path",
            "https://github.com/o/r",
        ] {
            let verdict = v.check(&c, url, 1000, &boom);
            assert!(matches!(verdict, Verdict::Unknown(_)), "{url}: {verdict:?}");
            assert!(decide(SnapshotPolicy::PrivateOnly, || verdict).is_err());
        }
        // Public and private answers.
        let public = |_: &str, _: &ProviderCfg, _: &str, _: &str| Ok(false);
        let private = |_: &str, _: &ProviderCfg, _: &str, _: &str| Ok(true);
        let p = v.check(&c, "https://github.com/o/pub", 1000, &public);
        assert_eq!(p, Verdict::Public);
        assert!(decide(SnapshotPolicy::PrivateOnly, || p).is_err());
        let q = v.check(&c, "https://github.com/o/priv", 1000, &private);
        assert_eq!(q, Verdict::Private);
        assert!(decide(SnapshotPolicy::PrivateOnly, || q).is_ok());
    }

    #[test]
    fn policies_all_and_none_do_not_consult_the_forge() {
        let boom = || panic!("must not look up visibility");
        assert!(decide(SnapshotPolicy::All, boom).is_ok());
        assert!(decide(SnapshotPolicy::None, boom).is_err());
    }

    #[test]
    fn answers_are_cached_until_the_ttl_and_failures_are_throttled() {
        let c = cfg();
        let mut v = Visibility::default();
        let calls = Cell::new(0);
        let ok = |_: &str, _: &ProviderCfg, _: &str, _: &str| {
            calls.set(calls.get() + 1);
            Ok(true)
        };
        let url = "https://github.com/o/r";
        assert_eq!(v.check(&c, url, 0, &ok), Verdict::Private);
        assert_eq!(v.check(&c, url, TTL - 1, &ok), Verdict::Private);
        assert_eq!(calls.get(), 1);
        assert_eq!(v.check(&c, url, TTL, &ok), Verdict::Private);
        assert_eq!(calls.get(), 2);
        // After the TTL a failing refresh does not fall back to the stale answer.
        let fails = Cell::new(0);
        let bad = |_: &str, _: &ProviderCfg, _: &str, _: &str| {
            fails.set(fails.get() + 1);
            Err("down".to_string())
        };
        let t = 2 * TTL;
        assert!(matches!(v.check(&c, url, t, &bad), Verdict::Unknown(_)));
        assert!(matches!(v.check(&c, url, t + 1, &bad), Verdict::Unknown(_)));
        assert_eq!(fails.get(), 1, "retry is throttled");
        assert!(matches!(
            v.check(&c, url, t + RETRY, &bad),
            Verdict::Unknown(_)
        ));
        assert_eq!(fails.get(), 2);
    }

    #[test]
    fn cache_persists_in_the_state_dir() {
        let dir = std::env::temp_dir().join(format!("grove-vis-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("visibility.json");
        let c = cfg();
        let public = |_: &str, _: &ProviderCfg, _: &str, _: &str| Ok(false);
        let mut v = Visibility::load(Some(file.clone()));
        assert_eq!(
            v.check(&c, "https://github.com/o/r", 50, &public),
            Verdict::Public
        );
        let never = |_: &str, _: &ProviderCfg, _: &str, _: &str| Err("unused".to_string());
        let mut again = Visibility::load(Some(file));
        assert_eq!(
            again.check(&c, "https://github.com/o/r", 60, &never),
            Verdict::Public
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gate_uses_the_group_policy_and_the_origin_url() {
        let c = cfg();
        let base = std::env::temp_dir().join(format!("grove-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mk = |group: &str, url: Option<&str>| {
            let repo = base.join(group).join("r");
            let bare = repo.join(".bare");
            std::fs::create_dir_all(&bare).unwrap();
            let g = Git::new(&bare);
            g.run(&["init", "-q", "--bare"]).unwrap();
            if let Some(u) = url {
                g.run(&["remote", "add", "origin", u]).unwrap();
            }
            repo
        };
        let private = |_: &str, _: &ProviderCfg, _: &str, _: &str| Ok(true);
        let mut v = Visibility::default();
        let url = Some("https://git.example.com:3000/o/r.git");
        let gate =
            |repo: &Path, v: &mut Visibility| snapshot_gate(&c, repo, "origin", v, 0, &private);
        assert!(gate(&mk("priv", url), &mut v).is_ok());
        assert!(gate(&mk("off", url), &mut v).is_err());
        assert!(gate(&mk("all", Some("https://unknown.example/o/r")), &mut v).is_ok());
        // Private-only without an origin remote: unknown, withheld.
        std::fs::remove_dir_all(base.join("priv")).unwrap();
        mk("priv", None);
        assert!(
            gate(&base.join("priv/r"), &mut v)
                .unwrap_err()
                .contains("unknown")
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
