use grove_core::config::ProviderKind;
use grove_providers::{Error, GitHub, Gitea, MirrorSpec, NewRepo, Provider, PullMirrorSpec, token};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

/// Serves the scripted `(status, body)` replies in order, one connection each, and reports
/// every raw request it received.
fn mock(replies: Vec<(u16, &'static str)>) -> (String, mpsc::Receiver<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for (status, body) in replies {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = s.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(h) = text.find("\r\n\r\n") {
                    let len = text[..h]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= h + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            tx.send(String::from_utf8_lossy(&buf).to_string()).unwrap();
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
        }
    });
    (url, rx)
}

/// Serialises the tests that change process-wide env vars.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Restores an env var on drop, also when an assertion panics.
struct EnvGuard(&'static str, Option<std::ffi::OsString>);

impl EnvGuard {
    fn set(k: &'static str, v: impl AsRef<std::ffi::OsStr>) -> Self {
        let g = EnvGuard(k, std::env::var_os(k));
        // SAFETY: callers hold ENV_LOCK; no other test reads these variables.
        unsafe { std::env::set_var(k, v) };
        g
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: as in `set`.
        unsafe {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }
}

fn body(req: &str) -> serde_json::Value {
    serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

#[test]
fn gitea_connection_list_create_and_mirror() {
    let (url, rx) = mock(vec![
        (200, r#"{"login":"dasemu"}"#),
        (
            200,
            r#"[{"name":"a","private":true,"clone_url":"u/a.git","default_branch":"main","owner":{"login":"dasemu"}}]"#,
        ),
        (200, "[]"),
        (
            201,
            r#"{"name":"n","private":true,"clone_url":"u/n.git","default_branch":"main","owner":{"login":"dasemu"}}"#,
        ),
        (200, "{}"),
    ]);
    let g = Gitea::new(ProviderKind::Gitea, &url, "secret".into());
    assert_eq!(g.test_connection().unwrap(), "dasemu");
    let first = rx.recv().unwrap();
    assert!(first.starts_with("GET /api/v1/user "), "{first}");
    assert!(first.to_lowercase().contains("authorization: token secret"));

    let repos = g.list_repos().unwrap();
    assert_eq!(
        (repos.len(), repos[0].name.as_str(), repos[0].private),
        (1, "a", true)
    );

    let r = g
        .create_repo(&NewRepo {
            name: "n".into(),
            owner: None,
            private: true,
            description: "d".into(),
        })
        .unwrap();
    assert_eq!(r.clone_url, "u/n.git");
    rx.recv().unwrap();
    rx.recv().unwrap(); // the empty second page
    let create = rx.recv().unwrap();
    assert!(create.starts_with("POST /api/v1/user/repos "), "{create}");
    assert_eq!(body(&create)["private"], true);

    g.configure_mirror(
        "dasemu",
        "n",
        &MirrorSpec {
            remote_url: "https://github.com/x/n.git".into(),
            remote_username: "x".into(),
            remote_token: "t".into(),
            interval: "8h0m0s".into(),
            sync_on_commit: true,
        },
    )
    .unwrap();
    let m = rx.recv().unwrap();
    assert!(
        m.starts_with("POST /api/v1/repos/dasemu/n/push_mirrors "),
        "{m}"
    );
    assert_eq!(body(&m)["sync_on_commit"], true);
}

#[test]
fn gitea_org_create_uses_org_endpoint_and_api_errors_carry_status() {
    let (url, rx) = mock(vec![
        (409, r#"{"message":"repository already exists"}"#),
        (401, r#"{"message":"bad token"}"#),
    ]);
    let g = Gitea::new(ProviderKind::Forgejo, &url, "t".into());
    let e = g
        .create_repo(&NewRepo {
            name: "n".into(),
            owner: Some("org".into()),
            private: false,
            description: String::new(),
        })
        .unwrap_err();
    assert!(matches!(e, Error::Api { status: 409, .. }), "{e}");
    assert!(
        rx.recv()
            .unwrap()
            .starts_with("POST /api/v1/orgs/org/repos ")
    );
    assert!(matches!(
        g.test_connection().unwrap_err(),
        Error::Api { status: 401, .. }
    ));
}

#[test]
fn github_uses_bearer_and_has_no_push_mirror() {
    let (url, rx) = mock(vec![(200, r#"{"login":"me"}"#)]);
    let g = GitHub::new(&url, "tok".into());
    assert_eq!(g.test_connection().unwrap(), "me");
    assert!(
        rx.recv()
            .unwrap()
            .to_lowercase()
            .contains("authorization: bearer tok")
    );
    assert!(!g.capabilities().push_mirror);
    let e = g
        .configure_mirror(
            "o",
            "r",
            &MirrorSpec {
                remote_url: String::new(),
                remote_username: String::new(),
                remote_token: String::new(),
                interval: String::new(),
                sync_on_commit: false,
            },
        )
        .unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)));
}

#[test]
fn unreachable_forge_is_a_network_error() {
    let g = Gitea::new(ProviderKind::Gitea, "http://127.0.0.1:1", "t".into());
    assert!(matches!(
        g.test_connection().unwrap_err(),
        Error::Network(_)
    ));
}

#[test]
fn env_var_overrides_the_keyring() {
    assert_eq!(token::env_name("gitea-main"), "GROVE_TOKEN_GITEA_MAIN");
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = EnvGuard::set("GROVE_TOKEN_ENV_TEST", " abc ");
    assert_eq!(token::get("env-test").unwrap(), "abc");
}

#[test]
fn gitea_pull_mirror_sends_credentials_only_for_private_sources() {
    let reply = r#"{"name":"m","private":false,"clone_url":"u/m.git","owner":{"login":"Org"}}"#;
    let (url, rx) = mock(vec![(201, reply), (201, reply)]);
    let g = Gitea::new(ProviderKind::Gitea, &url, "secret".into());
    let mut spec = PullMirrorSpec {
        name: "m".into(),
        owner: Some("Org".into()),
        private: false,
        source_url: "https://github.com/o/m.git".into(),
        source_auth: None,
        interval: "8h0m0s".into(),
    };
    assert_eq!(g.create_pull_mirror(&spec).unwrap().owner, "Org");
    let req = rx.recv().unwrap();
    assert!(req.starts_with("POST /api/v1/repos/migrate "), "{req}");
    let b = body(&req);
    assert_eq!(b["clone_addr"], "https://github.com/o/m.git");
    assert_eq!(
        (b["mirror"].as_bool(), b["repo_owner"].as_str()),
        (Some(true), Some("Org"))
    );
    assert!(b.get("auth_password").is_none(), "{b}");

    spec.private = true;
    spec.owner = None;
    spec.source_auth = Some(("me".into(), "ghtok".into()));
    g.create_pull_mirror(&spec).unwrap();
    let b = body(&rx.recv().unwrap());
    assert_eq!(
        (b["auth_username"].as_str(), b["auth_password"].as_str()),
        (Some("me"), Some("ghtok"))
    );
    assert!(b.get("repo_owner").is_none(), "{b}");
    assert_eq!(b["private"], true);
}

#[test]
fn github_cannot_pull_mirror() {
    let g = GitHub::new("http://127.0.0.1:9", "t".into());
    assert!(!g.capabilities().pull_mirror);
    let spec = PullMirrorSpec {
        name: "m".into(),
        owner: None,
        private: false,
        source_url: "x".into(),
        source_auth: None,
        interval: "8h0m0s".into(),
    };
    assert!(matches!(
        g.create_pull_mirror(&spec),
        Err(Error::Unsupported(_))
    ));
}

/// Changes PATH and GROVE_NO_CLI_TOKENS under ENV_LOCK; both are restored on drop.
#[test]
fn falls_back_to_tea_and_gh_logins() {
    use grove_core::config::Provider as ProviderCfg;
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "grove-clitok-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // tea's credential helper answers only for its host; gh echoes the host it was asked for.
    let scripts = [
        (
            "tea",
            "#!/bin/sh\nin=\"\"; while IFS= read -r l; do in=\"$in $l\"; done\ncase \"$in\" in *host=git.example.com*) printf 'protocol=https\\nhost=git.example.com\\nusername=u\\npassword=teatok\\n';; *) exit 1;; esac\n",
        ),
        (
            "gh",
            "#!/bin/sh\n[ \"$4\" = github.com ] && echo ' ghtok ' || exit 1\n",
        ),
    ];
    for (name, body) in scripts {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _path = EnvGuard::set("PATH", &dir);
    let off = EnvGuard::set("GROVE_NO_CLI_TOKENS", "0");
    let gitea = |url: &str| ProviderCfg {
        kind: ProviderKind::Gitea,
        protocol: Default::default(),
        url: Some(url.into()),
    };
    let github = ProviderCfg {
        kind: ProviderKind::Github,
        protocol: Default::default(),
        url: None,
    };
    let id = format!("clitok-{}", std::process::id());
    let got = token::resolve(&id, &gitea("https://git.example.com/")).unwrap();
    assert_eq!(got, ("teatok".to_string(), token::Source::Tea));
    assert!(token::resolve(&id, &gitea("https://other.example.com")).is_err());
    let got = token::resolve(&id, &github).unwrap();
    assert_eq!(got, ("ghtok".to_string(), token::Source::Gh));
    drop(off);
    let _off = EnvGuard::set("GROVE_NO_CLI_TOKENS", "1");
    assert!(token::resolve(&id, &github).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn get_repo_reports_visibility_on_both_forges() {
    let (url, rx) = mock(vec![
        (
            200,
            r#"{"name":"r","private":true,"clone_url":"u","default_branch":"main","owner":{"login":"o"}}"#,
        ),
        (404, r#"{"message":"not found"}"#),
        (200, r#"{"name":"r","owner":{"login":"o"}}"#),
    ]);
    let g = Gitea::new(ProviderKind::Gitea, &url, "t".into());
    let r = g.get_repo("o", "r").unwrap();
    assert!(r.private);
    assert!(rx.recv().unwrap().starts_with("GET /api/v1/repos/o/r "));
    assert!(matches!(
        g.get_repo("o", "gone").unwrap_err(),
        Error::Api { status: 404, .. }
    ));
    rx.recv().unwrap();
    // A reply without the flag is an error, never "public by default".
    assert!(g.get_repo("o", "r").is_err());

    let (url, rx) = mock(vec![(
        200,
        r#"{"name":"r","private":false,"visibility":"public","clone_url":"u","default_branch":"main","owner":{"login":"o"}}"#,
    )]);
    let gh = GitHub::new(&url, "t".into());
    assert!(!gh.get_repo("o", "r").unwrap().private);
    assert!(rx.recv().unwrap().starts_with("GET /repos/o/r "));

    // No `visibility` field: unknown, never guessed from `private`.
    let (url, _rx) = mock(vec![(
        200,
        r#"{"name":"r","private":true,"owner":{"login":"o"}}"#,
    )]);
    assert!(GitHub::new(&url, "t".into()).get_repo("o", "r").is_err());

    // An internal repo reports `private: true` but is not private.
    let (url, _rx) = mock(vec![(
        200,
        r#"{"name":"r","private":true,"visibility":"internal","owner":{"login":"o"}}"#,
    )]);
    assert!(
        !GitHub::new(&url, "t".into())
            .get_repo("o", "r")
            .unwrap()
            .private
    );
}

const REPO_A: &str = r#"{"name":"a","private":false,"clone_url":"https://h/o/a.git","ssh_url":"git@h:o/a.git","default_branch":"main","owner":{"login":"Org"}}"#;
const REPO_B: &str = r#"{"name":"b","private":true,"clone_url":"https://h/me/b.git","ssh_url":"ssh://gitea@h:2222/me/b.git","default_branch":"main","owner":{"login":"me"}}"#;
const REPO_C: &str = r#"{"name":"c","private":true,"clone_url":"https://h/x/c.git","ssh_url":"","default_branch":"main","owner":{"login":"x"}}"#;

fn lines(req: &str) -> &str {
    req.lines().next().unwrap()
}

#[test]
fn gitea_owner_listing_uses_the_org_endpoint_or_the_users_own() {
    let org = format!("[{REPO_A}]");
    let user = format!("[{REPO_B},{REPO_C}]");
    let (url, rx) = mock(vec![
        (200, Box::leak(org.into_boxed_str())),
        (200, "[]"),
        (200, Box::leak(user.into_boxed_str())),
        (200, "[]"),
    ]);
    let g = Gitea::new(ProviderKind::Gitea, &url, "t".into());
    let r = g.list_owner_repos("Org", "me").unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].ssh_url, "git@h:o/a.git");
    assert!(lines(&rx.recv().unwrap()).starts_with("GET /api/v1/orgs/Org/repos?limit=50&page=1 "));
    assert!(lines(&rx.recv().unwrap()).contains("page=2 "));
    // The login itself: `/user/repos`, then only its own repos (org repos it can see are dropped).
    let r = g.list_owner_repos("ME", "me").unwrap();
    assert_eq!(r.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["b"]);
    assert!(lines(&rx.recv().unwrap()).starts_with("GET /api/v1/user/repos?limit=50&page=1 "));
}

#[test]
fn github_owner_listing_scopes_the_query_and_sees_collaborator_repos_of_users() {
    let org = format!("[{REPO_A}]");
    let mine = format!("[{REPO_B}]");
    let collab = format!("[{REPO_C},{REPO_B}]");
    let (url, rx) = mock(vec![
        (200, Box::leak(org.into_boxed_str())),
        (200, "[]"),
        (200, Box::leak(mine.into_boxed_str())),
        (200, "[]"),
        (404, r#"{"message":"Not Found"}"#),
        (200, Box::leak(collab.into_boxed_str())),
        (200, "[]"),
    ]);
    let g = GitHub::new(&url, "t".into());
    assert_eq!(g.list_owner_repos("Org", "me").unwrap().len(), 1);
    assert!(
        lines(&rx.recv().unwrap()).starts_with("GET /orgs/Org/repos?type=all&per_page=100&page=1 ")
    );
    rx.recv().unwrap();
    assert_eq!(g.list_owner_repos("me", "me").unwrap().len(), 1);
    assert!(
        lines(&rx.recv().unwrap())
            .starts_with("GET /user/repos?affiliation=owner&per_page=100&page=1 ")
    );
    rx.recv().unwrap();
    // Not an organisation (404): what the token sees under that name, private ones included.
    let r = g.list_owner_repos("x", "me").unwrap();
    assert_eq!(r.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["c"]);
    rx.recv().unwrap();
    assert!(lines(&rx.recv().unwrap()).starts_with(
        "GET /user/repos?affiliation=owner,collaborator,organization_member&per_page=100&page=1 "
    ));
}

#[test]
fn a_short_page_is_not_the_end_of_a_listing() {
    // A server that caps the page size below the request: page 1 has 2 items, page 2 has 1.
    let p1 = format!("[{REPO_A},{REPO_B}]");
    let p2 = format!("[{REPO_C}]");
    let (url, _rx) = mock(vec![
        (200, Box::leak(p1.into_boxed_str())),
        (200, Box::leak(p2.into_boxed_str())),
        (200, "[]"),
    ]);
    let g = GitHub::new(&url, "t".into());
    assert_eq!(g.list_repos().unwrap().len(), 3);
}

#[test]
fn git_url_follows_the_protocol_and_needs_an_ssh_url_for_ssh() {
    use grove_core::config::Protocol;
    let (url, _rx) = mock(vec![
        (
            200,
            Box::leak(format!("[{REPO_B},{REPO_C}]").into_boxed_str()),
        ),
        (200, "[]"),
    ]);
    let g = Gitea::new(ProviderKind::Gitea, &url, "t".into());
    let r = g.list_repos().unwrap();
    assert_eq!(r[0].git_url(Protocol::Https).unwrap(), "https://h/me/b.git");
    assert_eq!(
        r[0].git_url(Protocol::Ssh).unwrap(),
        "ssh://gitea@h:2222/me/b.git"
    );
    assert!(r[1].git_url(Protocol::Ssh).is_err());
}
