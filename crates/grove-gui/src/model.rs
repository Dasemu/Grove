//! UI-independent logic: daemon status -> aggregate state, formatting, CLI output parsing.

use serde_json::Value;

/// What the tray icon and the status pill show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TrayState {
    Ok,
    Syncing,
    Attention,
    Paused,
    #[default]
    Offline,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Aggregate {
    pub state: TrayState,
    pub text: String,
    pub repos: usize,
    pub worktrees: usize,
    pub attention: usize,
}

/// `error: <detail>` states collapse to `error`.
pub fn state_name(s: &str) -> &str {
    if s.starts_with("error") { "error" } else { s }
}

pub fn state_label(s: &str) -> &'static str {
    match state_name(s) {
        "synced" => "Synced",
        "dirty" => "Editing",
        "ahead" => "Ahead",
        "behind" => "Behind",
        "behinddirty" => "Behind + editing",
        "diverged" => "Diverged",
        "error" => "Error",
        _ => "Unknown",
    }
}

/// What the next sync pass will do for a worktree in this state.
pub fn state_plan(s: &str) -> &'static str {
    match state_name(s) {
        "synced" => "Nothing to do.",
        "dirty" => "Snapshot the working changes and push the snapshot ref.",
        "ahead" => "Nothing automatic: push your commits yourself.",
        "behind" => "Fast-forward to the remote tip.",
        "behinddirty" => "Keep your edits; adopt the remote tip only if the content is identical.",
        "diverged" => "Nothing automatic: choose how to resolve it.",
        "error" => "Retry with backoff; see the error.",
        _ => "",
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Summarises the `status` answer of the daemon. `None` means the daemon is unreachable.
pub fn aggregate(status: Option<&Value>) -> Aggregate {
    let Some(s) = status else {
        return Aggregate {
            state: TrayState::Offline,
            text: "Daemon offline".into(),
            ..Default::default()
        };
    };
    let empty = Vec::new();
    let repos = s.get("repos").and_then(Value::as_array).unwrap_or(&empty);
    let (mut attention, mut dirty, mut total) = (0, 0, 0);
    for r in repos {
        let wts = r
            .get("worktrees")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        let has_err = wts
            .iter()
            .any(|w| state_name(str_of(w, "state")) == "error");
        if r.get("last_error").is_some_and(|e| !e.is_null()) && !has_err {
            attention += 1;
        }
        for w in wts {
            total += 1;
            match state_name(str_of(w, "state")) {
                "diverged" | "error" => attention += 1,
                "synced" => {}
                _ => dirty += 1,
            }
        }
    }
    let (state, text) = if s.get("paused").and_then(Value::as_bool) == Some(true) {
        (TrayState::Paused, "Paused".to_string())
    } else if attention > 0 {
        let verb = if attention == 1 { "needs" } else { "need" };
        (
            TrayState::Attention,
            format!("{attention} {verb} attention"),
        )
    } else if dirty > 0 {
        (TrayState::Syncing, format!("{dirty} syncing"))
    } else {
        (TrayState::Ok, "All synced".to_string())
    };
    Aggregate {
        state,
        text,
        repos: repos.len(),
        worktrees: total,
        attention,
    }
}

/// Colour key of an activity event kind.
pub fn log_tone(kind: &str) -> &'static str {
    match kind {
        "error" | "watch_error" => "error",
        "state" | "snapshots_withheld" | "snapshots_purged" | "declined" => "warn",
        _ => "ok",
    }
}

/// Last two path components, e.g. `work/api`.
pub fn short(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    parts[parts.len().saturating_sub(2)..].join("/")
}

pub fn ago(ts: u64, now: u64) -> String {
    if ts == 0 {
        return "never".into();
    }
    let d = now.saturating_sub(ts);
    match d {
        0..=4 => "just now".into(),
        5..=59 => format!("{d}s ago"),
        60..=3599 => format!("{} min ago", (d + 30) / 60),
        3600..=86399 => format!("{} h ago", (d + 1800) / 3600),
        _ => format!("{} d ago", (d + 43200) / 86400),
    }
}

/// `HH:MM:SS` (UTC offset applied by the caller) for the activity log.
pub fn clock(ts: u64, utc_offset_s: i64) -> String {
    let t = (ts as i64 + utc_offset_s).rem_euclid(86400);
    format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

/// Repository names from `grove import <group>` listing lines: `  owner/name on <provider>[ (private)]`.
pub fn parse_import_listing(output: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in output.lines() {
        if !line.starts_with(char::is_whitespace) {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        let Some((_, name)) = first.split_once('/') else {
            continue;
        };
        let rest: Vec<&str> = words.collect();
        let ok = matches!(
            rest.as_slice(),
            [] | ["(private)"] | ["on", _] | ["on", _, "(private)"]
        );
        if ok && !name.is_empty() && !name.contains('/') {
            names.push(name.to_string());
        }
    }
    names
}

fn names(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// One line per group: routes (private + public), snapshot policy, owners and layout.
pub fn group_summary(g: &Value) -> String {
    let mut s = format!("primary {}", str_of(g, "primary"));
    let mirrors = names(g.get("mirror_to"));
    if !mirrors.is_empty() {
        s += &format!(" · mirrors: {}", mirrors.join(", "));
    }
    if let Some(p) = g.get("public").filter(|p| p.is_object()) {
        s += &format!(" · public: {}", str_of(p, "primary"));
        let pm = names(p.get("mirror_to"));
        if !pm.is_empty() {
            s += &format!(" → {}", pm.join(", "));
        }
    }
    if let Some(p) = g.get("snapshots").and_then(Value::as_str) {
        s += &format!(" · snapshots: {p}");
    }
    if let Some(o) = g
        .get("owners")
        .and_then(Value::as_object)
        .filter(|o| !o.is_empty())
    {
        let list: Vec<String> = o
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
            .collect();
        s += &format!(" · owners: {}", list.join(", "));
    }
    let layout = g
        .get("layout")
        .and_then(Value::as_str)
        .unwrap_or("bare-worktrees");
    format!("{s} · {layout}")
}

pub type Section = (String, Vec<(String, String)>);

/// The settings view: sections of key/value rows, from `config.toml` as JSON.
pub fn settings_sections(cfg: &Value) -> Vec<Section> {
    let sync = cfg.get("sync");
    let num = |k: &str, d: u64| {
        sync.and_then(|s| s.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(d)
    };
    let machine = vec![
        (
            "Name".to_string(),
            str_of(&cfg["machine"], "name").to_string(),
        ),
        (
            "Workspace root".into(),
            str_of(&cfg["root"], "path").to_string(),
        ),
        (
            "Fetch interval".into(),
            format!("{} s", num("fetch_interval_s", 60)),
        ),
        (
            "Largest synced file".into(),
            format!("{} MiB", num("max_file_mb", 50)),
        ),
        (
            "Snapshots (default)".into(),
            sync.and_then(|s| s.get("snapshots"))
                .and_then(Value::as_str)
                .unwrap_or("private-only")
                .to_string(),
        ),
    ];
    let providers = cfg
        .get("providers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(id, p)| {
                    let url = str_of(p, "url");
                    let url = if url.is_empty() {
                        String::new()
                    } else {
                        format!(" · {url}")
                    };
                    (id.clone(), format!("{}{url}", str_of(p, "kind")))
                })
                .collect()
        })
        .unwrap_or_default();
    let groups = cfg
        .get("groups")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|g| (str_of(g, "name").to_string(), group_summary(g)))
                .collect()
        })
        .unwrap_or_default();
    let crepo = match cfg.get("config_repo").filter(|c| c.is_object()) {
        Some(c) => format!("{} on {}", str_of(c, "name"), str_of(c, "provider")),
        None => "not configured".into(),
    };
    vec![
        ("This machine".into(), machine),
        ("Providers".into(), providers),
        ("Groups".into(), groups),
        (
            "Config repository".into(),
            vec![("Repository".into(), crepo)],
        ),
    ]
}

/// Group name from a combo entry (`name → primary`).
pub fn group_name(entry: &str) -> &str {
    entry.split(" → ").next().unwrap_or(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(paused: bool, states: &[&str], last_error: Option<&str>) -> Value {
        let wts: Vec<Value> = states
            .iter()
            .map(|s| json!({"branch": "main", "state": s, "path": "/w/a/main"}))
            .collect();
        json!({"machine": "m", "paused": paused, "now": 100,
               "repos": [{"path": "/w/a", "phase": "idle", "last_sync": 90,
                          "last_error": last_error, "worktrees": wts}]})
    }

    #[test]
    fn offline_when_no_status() {
        let a = aggregate(None);
        assert_eq!(a.state, TrayState::Offline);
        assert_eq!(a.text, "Daemon offline");
    }

    #[test]
    fn state_to_tray_mapping() {
        let agg = |s: Value| aggregate(Some(&s));
        assert_eq!(agg(status(false, &["synced"], None)).state, TrayState::Ok);
        assert_eq!(
            agg(json!({"paused": false, "repos": []})).state,
            TrayState::Ok
        );
        assert_eq!(
            agg(status(false, &["synced", "dirty"], None)).state,
            TrayState::Syncing
        );
        assert_eq!(
            agg(status(false, &["behind"], None)).state,
            TrayState::Syncing
        );
        assert_eq!(
            agg(status(false, &["diverged"], None)).state,
            TrayState::Attention
        );
        assert_eq!(
            agg(status(false, &["error: boom"], None)).state,
            TrayState::Attention
        );
        assert_eq!(
            agg(status(false, &["synced"], Some("fetch failed"))).state,
            TrayState::Attention
        );
        // Paused wins over everything else.
        assert_eq!(
            agg(status(true, &["diverged"], None)).state,
            TrayState::Paused
        );
    }

    #[test]
    fn attention_counts_and_text() {
        let a = aggregate(Some(&status(
            false,
            &["diverged", "error: x", "dirty"],
            None,
        )));
        assert_eq!((a.attention, a.worktrees, a.repos), (2, 3, 1));
        assert_eq!(a.text, "2 need attention");
        let a = aggregate(Some(&status(false, &["diverged"], None)));
        assert_eq!(a.text, "1 needs attention");
        // An error on the repo that no worktree shows counts once.
        let a = aggregate(Some(&status(false, &["error: x"], Some("x"))));
        assert_eq!(a.attention, 1);
    }

    #[test]
    fn import_listing_parses_every_line_shape() {
        let out = "remote repositories in group work not on this machine:\n  me/alpha on main\n  me/beta on main (private)\n  me/gamma\n  me/delta (private)\nnot indented/line on x\n  nonsense line here\n";
        assert_eq!(
            parse_import_listing(out),
            ["alpha", "beta", "gamma", "delta"]
        );
        assert!(parse_import_listing("nothing to import").is_empty());
    }

    #[test]
    fn log_tones() {
        assert_eq!(log_tone("snapshots_withheld"), "warn");
        assert_eq!(log_tone("snapshots_purged"), "warn");
        assert_eq!(log_tone("snapshots_enabled"), "ok");
        assert_eq!(log_tone("watch_error"), "error");
    }

    #[test]
    fn formatting() {
        assert_eq!(short("/home/u/Workspace/work/api"), "work/api");
        assert_eq!(short("api"), "api");
        assert_eq!(ago(0, 100), "never");
        assert_eq!(ago(98, 100), "just now");
        assert_eq!(ago(70, 100), "30s ago");
        assert_eq!(ago(100, 400), "5 min ago");
        assert_eq!(ago(1, 7201), "2 h ago");
        assert_eq!(clock(3661, 0), "01:01:01");
        assert_eq!(clock(3600 * 23, 7200), "01:00:00");
        assert_eq!(state_label("error: x"), "Error");
        assert!(state_plan("diverged").contains("resolve"));
    }

    #[test]
    fn settings_show_public_route_and_owners() {
        let cfg = json!({
            "machine": {"name": "pca"}, "root": {"path": "/w"},
            "providers": {"gitea": {"kind": "gitea", "url": "https://g.example"}, "gh": {"kind": "github"}},
            "groups": [{"name": "work", "primary": "gitea", "mirror_to": ["gh"],
                        "owners": {"gitea": "Org", "gh": "org-gh"},
                        "snapshots": "none",
                        "public": {"primary": "gh", "mirror_to": ["gitea"]}}],
            "config_repo": {"name": "grove-config", "provider": "gitea"}
        });
        let s = settings_sections(&cfg);
        assert_eq!(s[0].1[0], ("Name".into(), "pca".into()));
        assert_eq!(s[0].1[2].1, "60 s");
        assert_eq!(
            s[0].1[4],
            ("Snapshots (default)".into(), "private-only".into())
        );
        // Providers come out sorted by id.
        assert_eq!(s[1].1[0].1, "github");
        assert_eq!(s[1].1[1].1, "gitea · https://g.example");
        assert_eq!(
            s[2].1[0].1,
            "primary gitea · mirrors: gh · public: gh → gitea · snapshots: none · owners: gh=org-gh, gitea=Org · bare-worktrees"
        );
        assert_eq!(s[3].1[0].1, "grove-config on gitea");
        assert_eq!(group_name("work → gitea"), "work");
    }
}
