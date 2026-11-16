use grove_core::config::ProviderKind;
use grove_providers::{Error, GitHub, Gitea, MirrorSpec, NewRepo, Provider, token};
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
    // SAFETY: tests in this binary that touch this variable run in this single test.
    unsafe { std::env::set_var("GROVE_TOKEN_ENV_TEST", " abc ") };
    assert_eq!(token::get("env-test").unwrap(), "abc");
}
