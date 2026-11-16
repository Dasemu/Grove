//! Validation and normalisation of what a forge (or a registry entry) tells us to clone: repo
//! names become directory names, URLs go to `git clone` together with credentials.

use crate::{Error, Result};
use grove_core::config::{Provider as ProviderCfg, ProviderKind};

/// A repo name is used as a directory name under the group: a plain, harmless one.
pub fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name != ".."
        && !name.starts_with(['.', '-'])
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

pub fn check_repo_name(name: &str) -> Result<()> {
    if valid_repo_name(name) {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "refusing repository name {name:?}: not a plain directory name"
        )))
    }
}

/// Host (lowercase, no user info, no port) of an `https://`, `ssh://` or scp-like
/// (`user@host:path`) URL. `None` for anything else (`file://`, local paths, `ext::`...).
pub fn url_host(url: &str) -> Option<String> {
    if url.starts_with('-') || url.chars().any(char::is_control) {
        return None;
    }
    let authority = if let Some((scheme, rest)) = url.split_once("://") {
        if !["https", "ssh"].contains(&scheme) {
            return None;
        }
        rest.split(['/', '?', '#']).next()?
    } else {
        // scp-like: user@host:path
        let (head, _) = url.split_once(':')?;
        if !head.contains('@') || head.contains('/') {
            return None;
        }
        head
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host.starts_with('[') {
        host
    } else {
        host.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// Ok when `url` is a clonable https/ssh URL on the provider's own host.
pub fn check_git_url(url: &str, expected_host: &str) -> Result<()> {
    match url_host(url) {
        Some(h) if h.eq_ignore_ascii_case(expected_host) => Ok(()),
        Some(h) => Err(Error::Other(format!(
            "refusing URL {url:?}: host {h} is not the provider's host {expected_host}"
        ))),
        None => Err(Error::Other(format!(
            "refusing URL {url:?}: only https://, ssh:// and user@host:path URLs are accepted"
        ))),
    }
}

/// Host (no port) the provider's forge answers on; `github.com` for GitHub without a URL.
pub fn provider_host(cfg: &ProviderCfg) -> String {
    let base = provider_base(cfg);
    url_host(&base).unwrap_or_default()
}

/// `https://host[:port]` of the provider's web/git endpoint (no trailing slash).
pub fn provider_base(cfg: &ProviderCfg) -> String {
    let url = cfg.url.as_deref().map(str::trim).unwrap_or_default();
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    match cfg.kind {
        ProviderKind::Github => {
            let h = authority.strip_prefix("api.").unwrap_or(authority);
            format!("https://{}", if h.is_empty() { "github.com" } else { h })
        }
        _ if authority.is_empty() => String::new(),
        _ => format!("{scheme}://{authority}"),
    }
}

/// `(owner, name)` of the path of a git URL (any accepted form), without a trailing `.git`.
pub fn owner_and_name(url: &str) -> Option<(String, String)> {
    url_host(url)?;
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => url.split_once(':')?.1,
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.rsplit_once('/')?;
    // Only the last two segments matter (a forge may live under a sub-path).
    let owner = owner.rsplit('/').next()?;
    (!owner.is_empty() && !name.is_empty()).then(|| (owner.to_string(), name.to_string()))
}

/// The canonical https clone URL of the repo a git URL points to, on the provider `cfg`.
/// `None` when the URL's host is not the provider's.
pub fn canonical_https(cfg: &ProviderCfg, url: &str) -> Option<String> {
    if !url_host(url)?.eq_ignore_ascii_case(&provider_host(cfg)) {
        return None;
    }
    let (owner, name) = owner_and_name(url)?;
    if !valid_repo_name(&name) || !valid_repo_name(&owner) {
        return None;
    }
    Some(format!("{}/{owner}/{name}.git", provider_base(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::config::Protocol;

    fn cfg(kind: ProviderKind, url: Option<&str>) -> ProviderCfg {
        ProviderCfg {
            kind,
            protocol: Protocol::Https,
            url: url.map(Into::into),
        }
    }

    #[test]
    fn repo_names_must_be_plain() {
        for ok in ["a", "my-repo", "x.y", "Synapse_2"] {
            assert!(valid_repo_name(ok), "{ok}");
        }
        for bad in [
            "", "..", ".", ".hidden", "-rf", "a/b", "a\\b", "a\nb", "a\u{7}",
        ] {
            assert!(!valid_repo_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_https_ssh_and_scp_urls_on_the_right_host() {
        let g = "git.example.es";
        for ok in [
            "https://git.example.es/o/r.git",
            "https://tok@git.example.es:3000/o/r.git",
            "ssh://gitea@git.example.es:2222/o/r.git",
            "git@git.example.es:o/r.git",
        ] {
            assert!(check_git_url(ok, g).is_ok(), "{ok}");
        }
        for bad in [
            "https://evil.example/o/r.git",
            "git@evil.example:o/r.git",
            "file:///etc/passwd",
            "/local/path",
            "ext::sh -c id",
            "--upload-pack=x",
            "http://git.example.es/o/r.git",
            "git://git.example.es/o/r.git",
        ] {
            assert!(check_git_url(bad, g).is_err(), "{bad}");
        }
    }

    #[test]
    fn canonical_urls_follow_the_provider() {
        let gitea = cfg(ProviderKind::Gitea, Some("https://git.example.es/"));
        let want = Some("https://git.example.es/dasemu/x.git".to_string());
        assert_eq!(
            canonical_https(&gitea, "ssh://gitea@git.example.es:2222/dasemu/x.git"),
            want
        );
        assert_eq!(
            canonical_https(&gitea, "https://git.example.es/dasemu/x"),
            want
        );
        assert_eq!(
            canonical_https(&gitea, "https://evil.example/dasemu/x.git"),
            None
        );
        let gh = cfg(ProviderKind::Github, None);
        assert_eq!(
            canonical_https(&gh, "git@github.com:Dasemu/Synapse.git").as_deref(),
            Some("https://github.com/Dasemu/Synapse.git")
        );
        let ghe = cfg(ProviderKind::Github, Some("https://api.ghe.example.com"));
        assert_eq!(provider_host(&ghe), "ghe.example.com");
    }
}
