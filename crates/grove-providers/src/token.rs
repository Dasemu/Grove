//! Token storage: `GROVE_TOKEN_<ID>` env var first (headless machines, tests), then the
//! desktop Secret Service, then the login of the forge's own CLI (`tea`, `gh`) if present.
//! Tokens are never written to the config file.

use crate::{Error, Result};
use grove_core::config::{Provider as ProviderCfg, ProviderKind};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SERVICE: &str = "grove";

pub fn env_name(provider_id: &str) -> String {
    let id: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("GROVE_TOKEN_{id}")
}

pub fn get(provider_id: &str) -> Result<String> {
    if let Ok(t) = std::env::var(env_name(provider_id))
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let e = keyring::Entry::new(SERVICE, provider_id).map_err(|e| Error::Other(e.to_string()))?;
    match e.get_password() {
        Ok(t) => Ok(t),
        Err(keyring::Error::NoEntry) => Err(Error::NoToken(provider_id.into())),
        Err(e) => Err(Error::Other(format!("secret service: {e}"))),
    }
}

/// Where a token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Grove,
    Tea,
    Gh,
}

/// [`get`], falling back to the forge CLI's own login (`tea` for Gitea/Forgejo, `gh` for
/// GitHub) when grove has no token of its own. Set `GROVE_NO_CLI_TOKENS=1` to disable it.
pub fn resolve(provider_id: &str, cfg: &ProviderCfg) -> Result<(String, Source)> {
    let own = match get(provider_id) {
        Ok(t) => return Ok((t, Source::Grove)),
        Err(e) => e,
    };
    if std::env::var_os("GROVE_NO_CLI_TOKENS").is_some_and(|v| !v.is_empty() && v != "0") {
        return Err(own);
    }
    let host = host(cfg);
    let found = match cfg.kind {
        ProviderKind::Gitea | ProviderKind::Forgejo => host
            .as_deref()
            .and_then(tea_token)
            .map(|t| (t, Source::Tea)),
        ProviderKind::Github => {
            gh_token(host.as_deref().unwrap_or("github.com")).map(|t| (t, Source::Gh))
        }
    };
    // Report grove's own error (no token / Secret Service down) when the CLIs have nothing.
    found.ok_or(own)
}

/// Host (with port) of the provider URL, without user info. For GitHub a leading `api.` is
/// dropped so Enterprise API URLs map to the host `gh` knows. `None` when no URL is configured.
fn host(cfg: &ProviderCfg) -> Option<String> {
    let url = cfg.url.as_deref()?.trim();
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let h = rest.split(['/', '?', '#']).next()?.trim();
    let h = h.rsplit_once('@').map_or(h, |(_, h)| h);
    let h = match cfg.kind {
        ProviderKind::Github => h.strip_prefix("api.").unwrap_or(h),
        _ => h,
    };
    (!h.is_empty()).then(|| h.to_string())
}

/// How long a forge CLI may take to answer (it may be waiting on a keyring unlock prompt).
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

fn run(cmd: &str, args: &[&str], stdin: Option<&str>) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let wrote = match (stdin, child.stdin.take()) {
        (Some(input), Some(mut w)) => w.write_all(input.as_bytes()).is_ok(),
        _ => true, // dropping the handle closes stdin
    };
    // Outputs are a few bytes, so the pipe cannot fill up while we poll.
    let deadline = Instant::now() + CLI_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if wrote && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    }?;
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    status.success().then_some(out)
}

/// Asks tea's git credential helper, which matches the host against tea's logins.
fn tea_token(host: &str) -> Option<String> {
    let out = run(
        "tea",
        &["login", "helper", "get"],
        Some(&format!("protocol=https\nhost={host}\n\n")),
    )?;
    out.lines()
        .find_map(|l| l.strip_prefix("password="))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn gh_token(host: &str) -> Option<String> {
    let t = run("gh", &["auth", "token", "--hostname", host], None)?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

pub fn set(provider_id: &str, token: &str) -> Result<()> {
    keyring::Entry::new(SERVICE, provider_id)
        .and_then(|e| e.set_password(token))
        .map_err(|e| Error::Other(format!("secret service: {e}")))
}

pub fn delete(provider_id: &str) -> Result<()> {
    match keyring::Entry::new(SERVICE, provider_id).and_then(|e| e.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Other(format!("secret service: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_strips_scheme_path_userinfo_and_github_api_prefix() {
        let h = |kind, url: &str| {
            host(&ProviderCfg {
                kind,
                protocol: Default::default(),
                url: Some(url.into()),
            })
        };
        let g = ProviderKind::Gitea;
        assert_eq!(
            h(g, "https://git.example.com/").as_deref(),
            Some("git.example.com")
        );
        assert_eq!(
            h(g, "https://u@git.example.com:3000/x").as_deref(),
            Some("git.example.com:3000")
        );
        assert_eq!(
            h(g, "https://api.example.com").as_deref(),
            Some("api.example.com")
        );
        let gh = ProviderKind::Github;
        assert_eq!(
            h(gh, "https://api.ghe.corp/api/v3").as_deref(),
            Some("ghe.corp")
        );
        assert_eq!(h(gh, "").as_deref(), None);
    }
}
