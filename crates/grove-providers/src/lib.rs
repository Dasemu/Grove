//! Forge providers (Gitea/Forgejo, GitHub) and token storage.
//!
//! Providers only talk to the forge's REST API. Nothing here ever deletes a remote repository.

mod gitea;
mod github;
mod http;
pub mod token;
pub mod url;

pub use gitea::Gitea;
pub use github::GitHub;

use grove_core::config::{Provider as ProviderCfg, ProviderKind};
use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// Transport problem (DNS, TLS, timeout...).
    Network(String),
    /// The forge answered with an error status.
    Api {
        status: u16,
        message: String,
    },
    /// The forge cannot do what was asked (see [`Capabilities`]).
    Unsupported(&'static str),
    /// No token stored for the provider.
    NoToken(String),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Network(m) => write!(f, "network error: {m}"),
            Error::Api { status, message } => write!(f, "forge returned {status}: {message}"),
            Error::Unsupported(m) => write!(f, "not supported by this forge: {m}"),
            Error::NoToken(id) => write!(
                f,
                "no token for provider {id:?} (log in with `tea`/`gh`, run `grove login {id}` or set GROVE_TOKEN_*)"
            ),
            Error::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepo {
    pub owner: String,
    pub name: String,
    pub private: bool,
    pub clone_url: String,
    /// SSH clone URL (empty when the forge did not report one).
    pub ssh_url: String,
    pub default_branch: String,
    /// Where the repo is mirrored from when it is a read-only pull mirror (pushes are refused).
    pub mirror_of: Option<String>,
}

impl RemoteRepo {
    /// The URL local git should use for `protocol`.
    pub fn git_url(&self, protocol: grove_core::config::Protocol) -> Result<&str> {
        use grove_core::config::Protocol;
        match protocol {
            Protocol::Https => Ok(&self.clone_url),
            Protocol::Ssh if self.ssh_url.is_empty() => Err(Error::Other(format!(
                "the forge reported no SSH URL for {}/{}",
                self.owner, self.name
            ))),
            Protocol::Ssh => Ok(&self.ssh_url),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewRepo {
    pub name: String,
    /// Create under this organisation instead of the authenticated user.
    pub owner: Option<String>,
    pub private: bool,
    pub description: String,
}

/// Push mirror from the primary forge to a secondary one.
#[derive(Debug, Clone)]
pub struct MirrorSpec {
    pub remote_url: String,
    pub remote_username: String,
    pub remote_token: String,
    /// Go-style duration (`8h0m0s`); `0` disables the periodic sync.
    pub interval: String,
    pub sync_on_commit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The forge can push-mirror a repo to another forge by itself.
    pub push_mirror: bool,
    /// The forge can create a repo that pulls (mirrors) another forge's repo on a schedule.
    pub pull_mirror: bool,
    pub orgs: bool,
}

/// A repo on this forge that mirrors `source_url` (another forge) by pulling it.
#[derive(Debug, Clone)]
pub struct PullMirrorSpec {
    pub name: String,
    /// Create under this organisation instead of the authenticated user.
    pub owner: Option<String>,
    pub private: bool,
    pub source_url: String,
    /// Credentials for a private source; `None` for a public one (nothing is stored).
    pub source_auth: Option<(String, String)>,
    /// Go-style duration (`8h0m0s`).
    pub interval: String,
}

/// A pull request merged on the forge whose head branch is in the same repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedPull {
    pub number: u64,
    pub head_ref: String,
    pub head_sha: String,
}

/// Head repo of a pull request equals its base repo (fork PRs reuse branch names freely).
/// A head repo that was deleted reports `null`: not the same repo.
pub(crate) fn same_repo(pr: &serde_json::Value) -> bool {
    let name = |side: &str| {
        pr[side]["repo"]["full_name"]
            .as_str()
            .map(str::to_ascii_lowercase)
    };
    matches!((name("head"), name("base")), (Some(h), Some(b)) if h == b)
}

pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> Capabilities;
    /// Verifies the token and returns the authenticated login.
    fn test_connection(&self) -> Result<String>;
    /// One repo's metadata (notably its visibility): `GET /repos/{owner}/{repo}`.
    fn get_repo(&self, owner: &str, name: &str) -> Result<RemoteRepo>;
    fn list_repos(&self) -> Result<Vec<RemoteRepo>>;
    /// The repos owned by `owner` (an organisation, or the authenticated user `login`), without
    /// paging through everything else the token can see.
    fn list_owner_repos(&self, owner: &str, login: &str) -> Result<Vec<RemoteRepo>>;
    fn create_repo(&self, new: &NewRepo) -> Result<RemoteRepo>;
    fn configure_mirror(&self, owner: &str, repo: &str, spec: &MirrorSpec) -> Result<()>;
    /// Creates a pull mirror (see [`Capabilities::pull_mirror`]).
    fn create_pull_mirror(&self, spec: &PullMirrorSpec) -> Result<RemoteRepo>;
    /// The most recently updated closed pull requests (one page) that were merged, newest
    /// first, leaving out those whose head is in a fork.
    fn merged_pulls(&self, owner: &str, name: &str) -> Result<Vec<MergedPull>>;
}

/// [`Provider::merged_pulls`] that also finds a repo transferred or renamed on the forge: `origin`
/// may still name the old place, which git and the repo endpoint follow by redirect but the
/// pulls endpoint does not. On failure the repo is looked up and, if its current owner/name
/// differ, its pulls are listed under those.
pub fn merged_pulls_following_moves(
    p: &dyn Provider,
    owner: &str,
    name: &str,
) -> Result<Vec<MergedPull>> {
    let err = match p.merged_pulls(owner, name) {
        Ok(pulls) => return Ok(pulls),
        Err(e) => e,
    };
    match p.get_repo(owner, name) {
        Ok(r)
            if !r.owner.is_empty()
                && !(r.owner.eq_ignore_ascii_case(owner) && r.name.eq_ignore_ascii_case(name)) =>
        {
            p.merged_pulls(&r.owner, &r.name)
        }
        _ => Err(err),
    }
}

/// Builds the provider described by `cfg`, using `token`.
pub fn build(cfg: &ProviderCfg, token: String) -> Box<dyn Provider> {
    match cfg.kind {
        ProviderKind::Gitea | ProviderKind::Forgejo => Box::new(Gitea::new(
            cfg.kind,
            cfg.url.as_deref().unwrap_or_default(),
            token,
        )),
        ProviderKind::Github => {
            Box::new(GitHub::new(cfg.url.as_deref().unwrap_or_default(), token))
        }
    }
}
