//! `config.toml` model, loading and validation (section 6 of the plan).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Config {
    pub machine: Machine,
    pub root: Root,
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub sync: Sync,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_repo: Option<ConfigRepo>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Machine {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Root {
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Gitea,
    Forgejo,
    Github,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Provider {
    pub kind: ProviderKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    #[default]
    BareWorktrees,
    Plain,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Group {
    pub name: String,
    pub primary: String,
    #[serde(default)]
    pub mirror_to: Vec<String>,
    #[serde(default)]
    pub layout: Layout,
    #[serde(default = "yes")]
    pub auto_materialize: bool,
    /// Overrides `sync.snapshots` for this group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<SnapshotPolicy>,
    /// Account or organisation that owns the group's repos, per provider id. A provider not
    /// listed here uses the authenticated user.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub owners: BTreeMap<String, String>,
    /// Where public repos go when that differs from `primary`/`mirror_to` (which then apply to
    /// private repos only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public: Option<Route>,
}

/// Whether the daemon pushes WIP snapshots (`refs/sync/<machine>/<branch>`) of a repo.
/// Snapshots contain uncommitted work, so they must not land in public repos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotPolicy {
    /// Push only when the forge confirms the repo is private (unknown counts as public).
    #[default]
    PrivateOnly,
    /// Push for every repo, public ones included.
    All,
    /// Never push snapshots.
    None,
}

/// A primary forge plus the forges it is mirrored to.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Route {
    pub primary: String,
    #[serde(default)]
    pub mirror_to: Vec<String>,
}

impl Group {
    /// The route a repo of this visibility takes.
    pub fn route(&self, private: bool) -> Route {
        match (&self.public, private) {
            (Some(r), false) => r.clone(),
            _ => Route {
                primary: self.primary.clone(),
                mirror_to: self.mirror_to.clone(),
            },
        }
    }

    /// The owner configured for `provider`, if any (otherwise: the authenticated user).
    pub fn owner(&self, provider: &str) -> Option<&str> {
        self.owners.get(provider).map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Sync {
    #[serde(default = "default_debounce")]
    pub snapshot_debounce_ms: u64,
    #[serde(default = "default_fetch")]
    pub fetch_interval_s: u64,
    #[serde(default)]
    pub ignore_extra: Vec<String>,
    /// Changed files larger than this many MiB are not snapshotted (0 = no limit).
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
    /// Default snapshot push policy; a group can override it.
    #[serde(default)]
    pub snapshots: SnapshotPolicy,
}

impl Default for Sync {
    fn default() -> Self {
        Sync {
            snapshot_debounce_ms: default_debounce(),
            fetch_interval_s: default_fetch(),
            ignore_extra: Vec::new(),
            max_file_mb: default_max_file_mb(),
            snapshots: SnapshotPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ConfigRepo {
    pub provider: String,
    pub name: String,
}

fn yes() -> bool {
    true
}
fn default_debounce() -> u64 {
    2000
}
fn default_max_file_mb() -> u64 {
    50
}
fn default_fetch() -> u64 {
    60
}

#[derive(Debug)]
pub enum ConfigError {
    Io(PathBuf, std::io::Error),
    Parse(String),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(p, e) => write!(f, "cannot read {}: {e}", p.display()),
            ConfigError::Parse(m) => write!(f, "invalid config syntax: {m}"),
            ConfigError::Invalid(m) => write!(f, "invalid config: {m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Writes the config atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let io = |e| ConfigError::Io(path.to_path_buf(), e);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }

    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        Config::parse(&text)
    }

    /// Default location: `$XDG_CONFIG_HOME/grove/config.toml` or `~/.config/...`.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("grove/config.toml"))
    }

    /// Local state directory: `$XDG_STATE_HOME/grove` or `~/.local/state/grove`.
    pub fn state_dir() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
        Some(base.join("grove"))
    }

    /// The snapshot policy of the group named `group`, falling back to `sync.snapshots`.
    pub fn snapshot_policy(&self, group: &str) -> SnapshotPolicy {
        self.groups
            .iter()
            .find(|g| g.name == group)
            .and_then(|g| g.snapshots)
            .unwrap_or(self.sync.snapshots)
    }

    /// `root.path` with a leading `~` expanded.
    pub fn root_path(&self) -> PathBuf {
        expand_home(&self.root.path)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));
        // The machine name is embedded in ref names: keep it ref-safe.
        let n = &self.machine.name;
        if n.is_empty()
            || !n
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return bad(format!(
                "machine.name {n:?} must be non-empty and use only letters, digits, '-' or '_'"
            ));
        }
        for g in &self.groups {
            // Group names are directory names under the root.
            if g.name.is_empty() || g.name.starts_with('.') || g.name.contains(['/', '\\']) {
                return bad(format!(
                    "group name {:?} must be a plain directory name",
                    g.name
                ));
            }
            let mut routes = vec![("", g.route(true))];
            if let Some(p) = &g.public {
                routes.push(("public.", p.clone()));
            }
            for (pfx, r) in routes {
                if !self.providers.contains_key(&r.primary) {
                    return bad(format!(
                        "group {:?}: unknown {pfx}primary provider {:?}",
                        g.name, r.primary
                    ));
                }
                if let Some(m) = r
                    .mirror_to
                    .iter()
                    .find(|m| !self.providers.contains_key(*m))
                {
                    return bad(format!(
                        "group {:?}: unknown {pfx}mirror provider {m:?}",
                        g.name
                    ));
                }
                if r.mirror_to.contains(&r.primary) {
                    return bad(format!(
                        "group {:?}: {pfx}primary cannot also be a mirror target",
                        g.name
                    ));
                }
            }
            for (p, o) in &g.owners {
                if !self.providers.contains_key(p) {
                    return bad(format!(
                        "group {:?}: owner for unknown provider {p:?}",
                        g.name
                    ));
                }
                if o.trim().is_empty() {
                    return bad(format!("group {:?}: empty owner for {p:?}", g.name));
                }
            }
        }
        if let Some(c) = &self.config_repo
            && !self.providers.contains_key(&c.provider)
        {
            return bad(format!("config_repo: unknown provider {:?}", c.provider));
        }
        Ok(())
    }
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if p == "~" => std::env::var_os("HOME").map_or_else(|| PathBuf::from(p), PathBuf::from),
        _ => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[machine]
name = "desktop"
[root]
path = "~/Hacking"
[providers.gitea-main]
kind = "gitea"
url = "https://git.example.com"
[providers.github]
kind = "github"
[[groups]]
name = "work"
primary = "gitea-main"
mirror_to = ["github"]
[sync]
fetch_interval_s = 30
[config_repo]
provider = "gitea-main"
name = "grove-config"
"#;

    #[test]
    fn parses_full_config_with_defaults() {
        let c = Config::parse(FULL).unwrap();
        assert_eq!(c.machine.name, "desktop");
        assert_eq!(c.groups[0].layout, Layout::BareWorktrees);
        assert!(c.groups[0].auto_materialize);
        assert_eq!(c.sync.fetch_interval_s, 30);
        assert_eq!(c.sync.snapshot_debounce_ms, 2000);
        assert_eq!(c.providers["github"].kind, ProviderKind::Github);
    }

    #[test]
    fn minimal_config_is_valid() {
        let c = Config::parse("[machine]\nname=\"a\"\n[root]\npath=\"/x\"\n").unwrap();
        assert!(c.groups.is_empty());
        assert_eq!(c.root_path(), PathBuf::from("/x"));
    }

    #[test]
    fn rejects_unsafe_machine_name() {
        for n in ["", "a/b", "a b", "a..b"] {
            let t = format!("[machine]\nname=\"{n}\"\n[root]\npath=\"/x\"\n");
            assert!(
                matches!(Config::parse(&t), Err(ConfigError::Invalid(_))),
                "{n:?}"
            );
        }
    }

    #[test]
    fn rejects_unknown_providers_and_self_mirror() {
        let base = "[machine]\nname=\"a\"\n[root]\npath=\"/x\"\n[providers.p]\nkind=\"gitea\"\n";
        let unknown = format!("{base}[[groups]]\nname=\"g\"\nprimary=\"nope\"\n");
        assert!(matches!(
            Config::parse(&unknown),
            Err(ConfigError::Invalid(_))
        ));
        let selfm = format!("{base}[[groups]]\nname=\"g\"\nprimary=\"p\"\nmirror_to=[\"p\"]\n");
        assert!(matches!(
            Config::parse(&selfm),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn snapshot_policy_defaults_to_private_only_and_groups_override() {
        let c = Config::parse(FULL).unwrap();
        assert_eq!(c.sync.snapshots, SnapshotPolicy::PrivateOnly);
        assert_eq!(c.snapshot_policy("work"), SnapshotPolicy::PrivateOnly);
        assert_eq!(c.snapshot_policy("unknown"), SnapshotPolicy::PrivateOnly);
        let t = FULL
            .replace(
                "fetch_interval_s = 30",
                "fetch_interval_s = 30\nsnapshots = \"none\"",
            )
            .replace(
                "mirror_to = [\"github\"]",
                "mirror_to = [\"github\"]\nsnapshots = \"all\"",
            );
        let c = Config::parse(&t).unwrap();
        assert_eq!(c.snapshot_policy("work"), SnapshotPolicy::All);
        assert_eq!(c.snapshot_policy("other"), SnapshotPolicy::None);
        let again = toml::to_string_pretty(&c).unwrap();
        assert_eq!(Config::parse(&again).unwrap(), c);
    }

    #[test]
    fn rejects_unknown_snapshot_policy_and_bad_group_names() {
        let bad = FULL.replace("fetch_interval_s = 30", "snapshots = \"public\"");
        assert!(matches!(Config::parse(&bad), Err(ConfigError::Parse(_))));
        for n in ["a/b", ".hidden", ""] {
            let t = FULL.replace("name = \"work\"", &format!("name = \"{n}\""));
            assert!(
                matches!(Config::parse(&t), Err(ConfigError::Invalid(_))),
                "{n:?}"
            );
        }
    }

    const ROUTED: &str = r#"
[machine]
name = "a"
[root]
path = "/x"
[providers.gitea]
kind = "gitea"
[providers.github]
kind = "github"
[[groups]]
name = "g"
primary = "gitea"
owners = { gitea = "Org", github = "org-gh" }
[groups.public]
primary = "github"
mirror_to = ["gitea"]
"#;

    #[test]
    fn routes_by_visibility_and_resolves_owners() {
        let c = Config::parse(ROUTED).unwrap();
        let g = &c.groups[0];
        let private = g.route(true);
        assert_eq!(
            (private.primary.as_str(), private.mirror_to.len()),
            ("gitea", 0)
        );
        let public = g.route(false);
        assert_eq!(public.primary, "github");
        assert_eq!(public.mirror_to, vec!["gitea".to_string()]);
        assert_eq!(g.owner("gitea"), Some("Org"));
        assert_eq!(g.owner("nope"), None);
        // Without a public route both visibilities use the base route.
        let plain = Config::parse(FULL).unwrap();
        assert_eq!(plain.groups[0].route(false), plain.groups[0].route(true));
        // Round-trips through save's serializer.
        let text = toml::to_string_pretty(&c).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), c);
    }

    #[test]
    fn rejects_bad_public_routes_and_owners() {
        for (from, to) in [
            (
                "primary = \"github\"\nmirror_to",
                "primary = \"nope\"\nmirror_to",
            ),
            ("mirror_to = [\"gitea\"]", "mirror_to = [\"github\"]"),
            ("mirror_to = [\"gitea\"]", "mirror_to = [\"nope\"]"),
            ("github = \"org-gh\"", "nope = \"x\""),
            ("github = \"org-gh\"", "github = \" \""),
        ] {
            let t = ROUTED.replace(from, to);
            assert_ne!(t, ROUTED, "{from:?} not found");
            assert!(
                matches!(Config::parse(&t), Err(ConfigError::Invalid(_))),
                "{to:?}"
            );
        }
    }

    #[test]
    fn rejects_bad_syntax_and_unknown_kind() {
        assert!(matches!(Config::parse("["), Err(ConfigError::Parse(_))));
        let t = "[machine]\nname=\"a\"\n[root]\npath=\"/x\"\n[providers.p]\nkind=\"gitlab\"\n";
        assert!(matches!(Config::parse(t), Err(ConfigError::Parse(_))));
    }
}
