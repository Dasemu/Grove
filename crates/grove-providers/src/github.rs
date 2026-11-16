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
        ssh_url: v["ssh_url"].as_str().unwrap_or_default().to_string(),
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
        Ok(self
            .http
            .pages("/user/repos", 100, "per_page")?
            .iter()
            .map(repo)
            .collect())
    }

    fn list_owner_repos(&self, owner: &str, login: &str) -> Result<Vec<RemoteRepo>> {
        let path = if owner.eq_ignore_ascii_case(login) {
            "/user/repos?affiliation=owner".to_string()
        } else {
            format!("/orgs/{owner}/repos?type=all")
        };
        let items = match self.http.pages(&path, 100, "per_page") {
            Ok(v) => v,
            // Not an organisation (a user, or an org the token cannot see): say so, and take
            // what the token can see under that name, private repos it collaborates on included.
            Err(Error::Api { status: 404, .. }) if !owner.eq_ignore_ascii_case(login) => {
                eprintln!(
                    "grove: /orgs/{owner} not found; listing the repositories you can see owned by {owner}"
                );
                self.http.pages(
                    "/user/repos?affiliation=owner,collaborator,organization_member",
                    100,
                    "per_page",
                )?
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
