use crate::http::Http;
use crate::{Capabilities, Error, MirrorSpec, NewRepo, Provider, RemoteRepo, Result};
use grove_core::config::ProviderKind;
use serde_json::{Value, json};

/// Gitea and Forgejo share the same `/api/v1`.
pub struct Gitea {
    kind: ProviderKind,
    http: Http,
}

impl Gitea {
    pub fn new(kind: ProviderKind, url: &str, token: String) -> Self {
        Gitea {
            kind,
            http: Http::new(
                &format!("{}/api/v1", url.trim_end_matches('/')),
                format!("token {token}"),
            ),
        }
    }
}

fn repo(v: &Value) -> RemoteRepo {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    RemoteRepo {
        owner: v["owner"]["login"].as_str().unwrap_or_default().to_string(),
        name: s("name"),
        private: v["private"].as_bool().unwrap_or(false),
        clone_url: s("clone_url"),
        default_branch: v["default_branch"].as_str().unwrap_or("main").to_string(),
    }
}

impl Provider for Gitea {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            push_mirror: true,
            orgs: true,
        }
    }

    fn test_connection(&self) -> Result<String> {
        let u = self.http.ok("GET", "/user", None)?;
        u["login"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Other("response has no login".into()))
    }

    fn list_repos(&self) -> Result<Vec<RemoteRepo>> {
        let mut out = Vec::new();
        for page in 1..=200 {
            let v = self
                .http
                .ok("GET", &format!("/user/repos?limit=50&page={page}"), None)?;
            let items = v.as_array().cloned().unwrap_or_default();
            let n = items.len();
            out.extend(items.iter().map(repo));
            if n < 50 {
                break;
            }
        }
        Ok(out)
    }

    fn create_repo(&self, new: &NewRepo) -> Result<RemoteRepo> {
        let path = match &new.owner {
            Some(o) => format!("/orgs/{o}/repos"),
            None => "/user/repos".to_string(),
        };
        let body = json!({"name": new.name, "private": new.private,
                          "description": new.description, "default_branch": "main"});
        Ok(repo(&self.http.ok("POST", &path, Some(&body))?))
    }

    fn configure_mirror(&self, owner: &str, name: &str, spec: &MirrorSpec) -> Result<()> {
        let body = json!({
            "remote_address": spec.remote_url,
            "remote_username": spec.remote_username,
            "remote_password": spec.remote_token,
            "interval": spec.interval,
            "sync_on_commit": spec.sync_on_commit,
        });
        self.http
            .ok(
                "POST",
                &format!("/repos/{owner}/{name}/push_mirrors"),
                Some(&body),
            )
            .map(|_| ())
    }
}
