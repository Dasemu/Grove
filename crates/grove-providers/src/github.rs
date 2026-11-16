use crate::http::Http;
use crate::{
    Capabilities, Error, MirrorSpec, NewRepo, Provider, PullMirrorSpec, RemoteRepo, Result,
};
use grove_core::config::ProviderKind;
use serde_json::{Value, json};

pub struct GitHub {
    http: Http,
}

impl GitHub {
    /// `url` empty means github.com; set it for GitHub Enterprise (`https://host/api/v3`) or tests.
    pub fn new(url: &str, token: String) -> Self {
        let base = if url.is_empty() || url == "https://github.com" {
            "https://api.github.com"
        } else {
            url
        };
        GitHub {
            http: Http::new(base, format!("Bearer {token}")),
        }
    }
}

fn repo(v: &Value) -> RemoteRepo {
    RemoteRepo {
        owner: v["owner"]["login"].as_str().unwrap_or_default().to_string(),
        name: v["name"].as_str().unwrap_or_default().to_string(),
        private: v["private"].as_bool().unwrap_or(false),
        clone_url: v["clone_url"].as_str().unwrap_or_default().to_string(),
        default_branch: v["default_branch"].as_str().unwrap_or("main").to_string(),
    }
}

impl Provider for GitHub {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Github
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            push_mirror: false,
            pull_mirror: false,
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
        let mut r = repo(&v);
        // `internal` repos (GitHub Enterprise) have `private: true` but are readable by the
        // whole enterprise: only an explicit "private" counts as private.
        // Without the field we cannot tell private from internal: refuse to guess.
        let vis = v["visibility"]
            .as_str()
            .ok_or_else(|| Error::Other("response has no `visibility` field".into()))?;
        r.private = vis == "private";
        Ok(r)
    }

    fn list_repos(&self) -> Result<Vec<RemoteRepo>> {
        let mut out = Vec::new();
        for page in 1..=200 {
            let v = self.http.ok(
                "GET",
                &format!("/user/repos?per_page=100&page={page}"),
                None,
            )?;
            let items = v.as_array().cloned().unwrap_or_default();
            let n = items.len();
            out.extend(items.iter().map(repo));
            if n < 100 {
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
        let body =
            json!({"name": new.name, "private": new.private, "description": new.description});
        Ok(repo(&self.http.ok("POST", &path, Some(&body))?))
    }

    fn configure_mirror(&self, _: &str, _: &str, _: &MirrorSpec) -> Result<()> {
        Err(Error::Unsupported(
            "GitHub has no push mirrors; use a Gitea pull mirror or the daemon's branch push",
        ))
    }

    fn create_pull_mirror(&self, _: &PullMirrorSpec) -> Result<RemoteRepo> {
        Err(Error::Unsupported(
            "GitHub cannot pull-mirror another forge",
        ))
    }
}
