//! Editing namespaces (`[[groups]]`) and providers in `config.toml` from the CLI and GUI, so
//! nobody has to hand-edit TOML. Every change is validated before anything is written, and the
//! previous file is kept as `config.toml.bak` (the TOML writer drops comments).

use anyhow::{Context, Result, bail};
use grove_core::config::{
    Config, Group, Layout, Protocol, Provider, ProviderKind, Route, SnapshotPolicy, is_plain_name,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct GroupArgs {
    pub name: String,
    pub primary: String,
    pub mirror_to: Vec<String>,
    /// `PROVIDER=OWNER`, or a bare `OWNER` for the primary provider.
    pub owners: Vec<String>,
    pub public_primary: Option<String>,
    pub public_mirror_to: Vec<String>,
    pub snapshots: Option<SnapshotPolicy>,
    pub no_materialize: bool,
}

pub fn parse_kind(k: &str) -> Result<ProviderKind> {
    Ok(match k {
        "gitea" => ProviderKind::Gitea,
        "forgejo" => ProviderKind::Forgejo,
        "github" => ProviderKind::Github,
        k => bail!("unknown provider kind {k:?} (gitea, forgejo, github)"),
    })
}

pub fn parse_protocol(p: &str) -> Result<Protocol> {
    Ok(match p {
        "https" => Protocol::Https,
        "ssh" => Protocol::Ssh,
        p => bail!("unknown protocol {p:?} (https, ssh)"),
    })
}

pub fn parse_snapshots(s: &str) -> Result<SnapshotPolicy> {
    Ok(match s {
        "private-only" => SnapshotPolicy::PrivateOnly,
        "all" => SnapshotPolicy::All,
        "none" => SnapshotPolicy::None,
        s => bail!("unknown snapshot policy {s:?} (private-only, all, none)"),
    })
}

fn kind_str(k: ProviderKind) -> &'static str {
    match k {
        ProviderKind::Gitea => "gitea",
        ProviderKind::Forgejo => "forgejo",
        ProviderKind::Github => "github",
    }
}

/// Adds the namespace `a.name` (or replaces it with `replace`, keeping its layout).
pub fn add_group(cfg: &mut Config, a: GroupArgs, replace: bool) -> Result<()> {
    let existing = cfg.groups.iter().position(|g| g.name == a.name);
    if existing.is_some() && !replace {
        bail!(
            "namespace {:?} already exists (use --replace to change it)",
            a.name
        );
    }
    let mut owners = BTreeMap::new();
    for o in &a.owners {
        let (p, who) = o
            .split_once('=')
            .map_or((a.primary.as_str(), o.as_str()), |(p, w)| (p, w));
        owners.insert(p.trim().to_string(), who.trim().to_string());
    }
    let public = a.public_primary.map(|primary| Route {
        primary,
        mirror_to: a.public_mirror_to.clone(),
    });
    if public.is_none() && !a.public_mirror_to.is_empty() {
        bail!("--public-mirror-to needs --public-primary");
    }
    let g = Group {
        name: a.name,
        primary: a.primary,
        mirror_to: a.mirror_to,
        layout: existing.map_or(Layout::BareWorktrees, |i| cfg.groups[i].layout),
        auto_materialize: !a.no_materialize,
        snapshots: a.snapshots,
        owners,
        public,
    };
    match existing {
        Some(i) => cfg.groups[i] = g,
        None => cfg.groups.push(g),
    }
    Ok(())
}

pub fn remove_group(cfg: &mut Config, name: &str) -> Result<()> {
    let before = cfg.groups.len();
    cfg.groups.retain(|g| g.name != name);
    if cfg.groups.len() == before {
        bail!("no namespace {name:?}");
    }
    Ok(())
}

pub fn add_provider(cfg: &mut Config, id: &str, p: Provider, replace: bool) -> Result<()> {
    if !is_plain_name(id) {
        bail!("provider id {id:?} must use only letters, digits, '-' or '_'");
    }
    if p.kind != ProviderKind::Github && p.url.as_deref().is_none_or(str::is_empty) {
        bail!("a {} provider needs --url", kind_str(p.kind));
    }
    if let Some(u) = &p.url
        && !u.starts_with("https://")
        && !u.starts_with("http://")
    {
        bail!("--url must start with https:// (got {u:?})");
    }
    if cfg.providers.contains_key(id) && !replace {
        bail!("provider {id:?} already exists (use --replace to change it)");
    }
    cfg.providers.insert(id.to_string(), p);
    Ok(())
}

/// Removes a provider no namespace or the config repo still uses.
pub fn remove_provider(cfg: &mut Config, id: &str) -> Result<()> {
    if !cfg.providers.contains_key(id) {
        bail!("no provider {id:?}");
    }
    let users: Vec<&str> = cfg
        .groups
        .iter()
        .filter(|g| {
            let routes = [Some(g.route(true)), g.public.clone()];
            routes
                .iter()
                .flatten()
                .any(|r| r.primary == id || r.mirror_to.iter().any(|m| m == id))
                || g.owners.contains_key(id)
        })
        .map(|g| g.name.as_str())
        .collect();
    if !users.is_empty() {
        bail!(
            "provider {id:?} is used by namespace(s) {}; change or remove them first",
            users.join(", ")
        );
    }
    if cfg.config_repo.as_ref().is_some_and(|c| c.provider == id) {
        bail!("provider {id:?} holds the config repo; it cannot be removed");
    }
    cfg.providers.remove(id);
    Ok(())
}

/// Validates `cfg`, keeps the current file as `<path>.bak` and writes the new one.
pub fn save(cfg: &Config, path: &Path) -> Result<PathBuf> {
    let text = toml::to_string_pretty(cfg)?;
    Config::parse(&text)?;
    let bak = path.with_extension("toml.bak");
    if path.exists() {
        std::fs::copy(path, &bak).with_context(|| format!("backing up {}", path.display()))?;
    }
    cfg.save(path)?;
    Ok(bak)
}

/// One line per namespace: `name  primary -> mirrors  [public: ...]  owners`.
pub fn describe_groups(cfg: &Config) -> Vec<String> {
    cfg.groups
        .iter()
        .map(|g| {
            let mut s = format!("{:<16} {}", g.name, route_str(&g.route(true)));
            if let Some(p) = &g.public {
                s.push_str(&format!("  public: {}", route_str(p)));
            }
            if !g.owners.is_empty() {
                let o: Vec<String> = g.owners.iter().map(|(p, w)| format!("{p}={w}")).collect();
                s.push_str(&format!("  owners: {}", o.join(",")));
            }
            if let Some(sp) = g.snapshots {
                let name = match sp {
                    SnapshotPolicy::PrivateOnly => "private-only",
                    SnapshotPolicy::All => "all",
                    SnapshotPolicy::None => "none",
                };
                s.push_str(&format!("  snapshots: {name}"));
            }
            s
        })
        .collect()
}

fn route_str(r: &Route) -> String {
    if r.mirror_to.is_empty() {
        r.primary.clone()
    } else {
        format!("{} -> {}", r.primary, r.mirror_to.join(", "))
    }
}

pub fn describe_providers(cfg: &Config) -> Vec<String> {
    cfg.providers
        .iter()
        .map(|(id, p)| {
            format!(
                "{id:<16} {:<8} {:<6} {}",
                kind_str(p.kind),
                p.protocol.as_str(),
                p.url.as_deref().unwrap_or("https://github.com")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::parse(
            "[machine]\nname=\"m\"\n[root]\npath=\"/x\"\n\
             [providers.gitea]\nkind=\"gitea\"\nurl=\"https://git.example.es\"\n\
             [providers.gh]\nkind=\"github\"\n\
             [[groups]]\nname=\"work\"\nprimary=\"gitea\"\nlayout=\"plain\"\n",
        )
        .unwrap()
    }

    fn args(name: &str, primary: &str) -> GroupArgs {
        GroupArgs {
            name: name.into(),
            primary: primary.into(),
            mirror_to: vec![],
            owners: vec![],
            public_primary: None,
            public_mirror_to: vec![],
            snapshots: None,
            no_materialize: false,
        }
    }

    fn valid(c: &Config) -> bool {
        Config::parse(&toml::to_string_pretty(c).unwrap()).is_ok()
    }

    #[test]
    fn add_group_with_mirror_owners_and_public_route() {
        let mut c = cfg();
        let mut a = args("oss", "gitea");
        a.mirror_to = vec!["gh".into()];
        a.owners = vec!["Zenvairo".into(), "gh=zenvairo-gh".into()];
        a.public_primary = Some("gh".into());
        a.public_mirror_to = vec!["gitea".into()];
        a.snapshots = Some(SnapshotPolicy::None);
        add_group(&mut c, a, false).unwrap();
        assert!(valid(&c));
        let g = &c.groups[1];
        assert_eq!(g.owner("gitea"), Some("Zenvairo"));
        assert_eq!(g.owner("gh"), Some("zenvairo-gh"));
        assert_eq!(g.route(false).primary, "gh");
        assert_eq!(c.snapshot_policy("oss"), SnapshotPolicy::None);
        let lines = describe_groups(&c);
        assert!(lines[1].contains("gitea -> gh") && lines[1].contains("public: gh -> gitea"));
    }

    #[test]
    fn replacing_a_group_needs_the_flag_and_keeps_its_layout() {
        let mut c = cfg();
        assert!(add_group(&mut c, args("work", "gh"), false).is_err());
        add_group(&mut c, args("work", "gh"), true).unwrap();
        assert_eq!(c.groups.len(), 1);
        assert_eq!(c.groups[0].primary, "gh");
        assert_eq!(c.groups[0].layout, Layout::Plain);
        let mut bad = args("x", "gitea");
        bad.public_mirror_to = vec!["gh".into()];
        assert!(add_group(&mut c, bad, false).is_err());
        // Unknown providers are caught by validation before saving.
        add_group(&mut c, args("y", "nope"), false).unwrap();
        assert!(!valid(&c));
    }

    #[test]
    fn providers_in_use_cannot_be_removed() {
        let mut c = cfg();
        assert!(
            remove_provider(&mut c, "gitea")
                .unwrap_err()
                .to_string()
                .contains("work")
        );
        remove_provider(&mut c, "gh").unwrap();
        assert!(remove_provider(&mut c, "gh").is_err());
        remove_group(&mut c, "work").unwrap();
        assert!(remove_group(&mut c, "work").is_err());
        remove_provider(&mut c, "gitea").unwrap();
        assert!(valid(&c));
    }

    #[test]
    fn add_provider_checks_id_url_and_duplicates() {
        let mut c = cfg();
        let p = |kind, url: Option<&str>| Provider {
            kind,
            protocol: Protocol::Ssh,
            url: url.map(String::from),
        };
        assert!(add_provider(&mut c, "a b", p(ProviderKind::Github, None), false).is_err());
        assert!(add_provider(&mut c, "fj", p(ProviderKind::Forgejo, None), false).is_err());
        assert!(
            add_provider(&mut c, "fj", p(ProviderKind::Forgejo, Some("git.x")), false).is_err()
        );
        add_provider(
            &mut c,
            "fj",
            p(ProviderKind::Forgejo, Some("https://git.x")),
            false,
        )
        .unwrap();
        assert!(add_provider(&mut c, "fj", p(ProviderKind::Github, None), false).is_err());
        add_provider(&mut c, "fj", p(ProviderKind::Github, None), true).unwrap();
        assert!(valid(&c));
        assert!(
            describe_providers(&c)
                .iter()
                .any(|l| l.starts_with("fj") && l.contains("ssh"))
        );
    }

    #[test]
    fn save_keeps_a_backup() {
        let tmp = crate::test_tmp::TempDir::new("ns-save");
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# my comment\n[machine]\nname=\"m\"\n[root]\npath=\"/x\"\n",
        )
        .unwrap();
        let mut c = Config::load(&path).unwrap();
        c.providers.insert(
            "gh".into(),
            Provider {
                kind: ProviderKind::Github,
                protocol: Protocol::Https,
                url: None,
            },
        );
        let bak = save(&c, &path).unwrap();
        assert!(
            std::fs::read_to_string(bak)
                .unwrap()
                .contains("# my comment")
        );
        assert_eq!(Config::load(&path).unwrap(), c);
        // An invalid config is never written.
        add_group(&mut c, args("g", "nope"), false).unwrap();
        assert!(save(&c, &path).is_err());
        assert!(Config::load(&path).unwrap().groups.is_empty());
    }
}
