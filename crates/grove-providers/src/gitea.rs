use crate::http::Http;
use crate::{
    Capabilities, Error, MirrorSpec, NewRepo, Provider, PullMirrorSpec, RemoteRepo, Result,
};
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
        ssh_url: s("ssh_url"),
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
            pull_mirror: true,
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

    fn get_repo(&self, owner: &str, name: &str) -> Result<RemoteRepo> {
        let v = self
            .http
            .ok("GET", &format!("/repos/{owner}/{name}"), None)?;
        // `repo` defaults a missing flag to public; for a visibility check that must be an error.
        if !v["private"].is_boolean() {
            return Err(Error::Other("response has no `private` flag".into()));
        }
        Ok(repo(&v))
    }

    fn list_repos(&self) -> Result<Vec<RemoteRepo>> {
        Ok(self
            .http
            .pages("/user/repos", 50, "limit")?
            .iter()
            .map(repo)
            .collect())
    }

    fn list_owner_repos(&self, owner: &str, login: &str) -> Result<Vec<RemoteRepo>> {
        let path = if owner.eq_ignore_ascii_case(login) {
            "/user/repos".to_string()
        } else {
            format!("/orgs/{owner}/repos")
        };
        let items = match self.http.pages(&path, 50, "limit") {
            Ok(v) => v,
            // Not an organisation (a user, or an org the token cannot see): say so, and take
            // what the token can see under that name, private repos it collaborates on included.
            Err(Error::Api { status: 404, .. }) if !owner.eq_ignore_ascii_case(login) => {
                eprintln!(
                    "grove: /orgs/{owner} not found; listing the repositories you can see owned by {owner}"
                );
                self.http.pages("/user/repos", 50, "limit")?
            }
            Err(e) => return Err(e),
        };
        Ok(items
            .iter()
            .map(repo)
            .filter(|r| r.owner.eq_ignore_ascii_case(owner))
            .collect())
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

    fn create_pull_mirror(&self, spec: &PullMirrorSpec) -> Result<RemoteRepo> {
        let mut body = json!({
            "clone_addr": spec.source_url,
            "repo_name": spec.name,
            "mirror": true,
            "mirror_interval": spec.interval,
            "private": spec.private,
            "service": "git",
            "description": "Mirror managed by grove",
        });
        if let Some(o) = &spec.owner {
            body["repo_owner"] = json!(o);
        }
        if let Some((user, token)) = &spec.source_auth {
            body["auth_username"] = json!(user);
            body["auth_password"] = json!(token);
        }
        Ok(repo(&self.http.ok(
            "POST",
            "/repos/migrate",
            Some(&body),
        )?))
    }
}
