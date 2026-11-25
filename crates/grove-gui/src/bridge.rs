//! Everything the GUI needs from the machine: daemon IPC, allow-listed `grove` calls, config.

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `grove` subcommands the GUI may run. Everything else is refused.
const ALLOWED: &[&str] = &[
    "init",
    "login",
    "new",
    "import",
    "add",
    "config-repo-init",
    "bootstrap",
    "resolve",
    "secrets",
    "sync",
    "status",
    "group",
    "provider",
    "open",
];

pub struct Bridge {
    pub grove_bin: PathBuf,
    pub config: Option<PathBuf>,
    pub socket: PathBuf,
}

impl Bridge {
    /// Forwards one IPC request to the daemon. An unreachable daemon is a normal answer.
    pub fn ipc(&self, req: &Value) -> Value {
        grove_daemon::ipc::request(&self.socket, req).unwrap_or_else(
            |e| json!({"v": 1, "ok": false, "offline": true, "error": format!("daemon not reachable: {e}")}),
        )
    }

    /// Runs an allow-listed `grove` subcommand. `stdin` is passed through (used by `login`).
    pub fn cli(&self, args: &[String], stdin: &str) -> Value {
        let Some(sub) = args.first() else {
            return json!({"ok": false, "output": "no command"});
        };
        if !ALLOWED.contains(&sub.as_str()) {
            return json!({"ok": false, "output": format!("command {sub:?} is not allowed from the GUI")});
        }
        if let Err(e) = validate(args) {
            return json!({"ok": false, "output": format!("rejected: {e}")});
        }
        // `add` gets the canonical path the check validated, not the string it was given.
        let mut checked: Vec<String> = args.to_vec();
        if sub == "add" {
            match self.check_migrate_args(args) {
                Ok(canon) => checked[1] = canon.to_string_lossy().into_owned(),
                Err(e) => return json!({"ok": false, "output": format!("rejected: {e}")}),
            }
        }
        // `open` only for a directory under the workspace root, in canonical form.
        if sub == "open" {
            let checked_path = self
                .loaded_config()
                .ok_or_else(|| "no readable configuration".to_string())
                .and_then(|cfg| check_open_path(&cfg, Path::new(&args[1])));
            match checked_path {
                Ok(canon) => checked[1] = canon.to_string_lossy().into_owned(),
                Err(e) => return json!({"ok": false, "output": format!("rejected: {e}")}),
            }
        }
        let args = &checked[..];
        let mut cmd = Command::new(&self.grove_bin);
        if let Some(c) = &self.config {
            cmd.arg("--config").arg(c);
        }
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut run = || -> std::io::Result<std::process::Output> {
            let mut child = cmd.spawn()?;
            // Feed stdin from a thread so a chatty child cannot deadlock against a full pipe.
            let writer = child.stdin.take().map(|mut si| {
                let data = stdin.to_owned();
                std::thread::spawn(move || {
                    let _ = si.write_all(data.as_bytes());
                })
            });
            let out = child.wait_with_output();
            if let Some(w) = writer {
                let _ = w.join();
            }
            out
        };
        match run() {
            Ok(o) => {
                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&o.stderr));
                json!({"ok": o.status.success(), "output": text.trim()})
            }
            Err(e) => {
                json!({"ok": false, "output": format!("cannot run {}: {e}", self.grove_bin.display())})
            }
        }
    }

    fn loaded_config(&self) -> Option<grove_core::config::Config> {
        let p = self
            .config
            .clone()
            .or_else(grove_core::config::Config::default_path)?;
        grove_core::config::Config::load(&p).ok()
    }

    /// `add <path>` only for an existing directory directly under `<root>/<group>/`.
    fn check_migrate_args(&self, args: &[String]) -> Result<PathBuf, String> {
        let [_, path] = args else {
            return Err("add takes exactly one path".into());
        };
        let cfg = self.loaded_config().ok_or("no readable configuration")?;
        check_migrate_path(&cfg, Path::new(path))
    }

    /// Installed launchers (name, label) for "Open in" buttons, the default one first.
    pub fn launchers(&self) -> Vec<(String, String)> {
        use grove_core::launch::{System, launchers, pick};
        let Some(cfg) = self.loaded_config() else {
            return Vec::new();
        };
        let all = launchers(&cfg.open, &System);
        let default = pick(&all, &cfg.open, None).ok().map(|l| l.name.clone());
        let mut out: Vec<(String, String)> = all
            .into_iter()
            .filter(|l| l.available)
            .map(|l| (l.name, l.label))
            .collect();
        if let Some(i) = out.iter().position(|(n, _)| Some(n) == default.as_ref()) {
            let d = out.remove(i);
            out.insert(0, d);
        }
        out
    }

    /// Where `grove add` writes its safety backup.
    pub fn backup_dir() -> Option<PathBuf> {
        grove_core::config::Config::state_dir().map(|d| d.join("backups"))
    }

    /// `config.toml` as JSON (it holds no secrets), or `None` before onboarding.
    pub fn config_json(&self) -> Option<Value> {
        let p = self.config.clone().or_else(grove_core_default_config)?;
        let t: toml::Value = toml::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        serde_json::to_value(t).ok()
    }
}

/// Per-command flag table: (flags without value, flags taking one value).
fn flag_spec(sub: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match sub {
        "init" => (
            &[],
            &[
                "--machine",
                "--root",
                "--provider",
                "--kind",
                "--protocol",
                "--group",
                "--config-repo",
                "--url",
            ],
        ),
        "new" => (&["--public"], &["--open"]),
        "open" => (&[], &["--with"]),
        "group" => (
            &["--replace", "--no-materialize"],
            &[
                "--primary",
                "--mirror-to",
                "--owner",
                "--public-primary",
                "--public-mirror-to",
                "--snapshots",
            ],
        ),
        "provider" => (&["--replace"], &["--kind", "--url", "--protocol"]),
        "resolve" => (&["--take-remote", "--keep-local"], &["--save-as"]),
        _ => (&[], &[]),
    }
}

/// Strict argument shape: only known flags, no option-like positionals or values (so nothing can
/// smuggle in `--config`, `--force`, `--compare`, ...), and `secrets` limited to init/pull/rekey.
pub fn validate(args: &[String]) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let (bools, valued) = flag_spec(sub);
    let mut it = args[1..].iter().enumerate();
    while let Some((i, a)) = it.next() {
        if a.starts_with('-') {
            if valued.contains(&a.as_str()) {
                match it.next() {
                    Some((_, v)) if !v.starts_with('-') => {}
                    _ => return Err(format!("{a} needs a value")),
                }
            } else if !bools.contains(&a.as_str()) {
                return Err(format!("flag {a:?} is not allowed from the GUI"));
            }
        } else if sub == "secrets" && i == 0 && !["init", "pull", "rekey"].contains(&a.as_str()) {
            return Err(format!("secrets {a:?} is not allowed from the GUI"));
        } else if (sub == "group" || sub == "provider")
            && i == 0
            && !["add", "remove"].contains(&a.as_str())
        {
            return Err(format!("{sub} {a:?} is not allowed from the GUI"));
        }
    }
    if sub == "secrets" && args.len() != 2 {
        return Err("secrets takes exactly one action".into());
    }
    if (sub == "group" || sub == "provider") && args.len() < 3 {
        return Err(format!("{sub} needs an action and a name"));
    }
    if sub == "open" && args.get(1).is_none_or(|p| p.starts_with('-')) {
        return Err("open needs a path first".into());
    }
    Ok(())
}

/// A clone the GUI may migrate: an existing directory (not a symlink) whose parent is one of the
/// configured group directories. Returns its canonical path.
pub fn check_migrate_path(
    cfg: &grove_core::config::Config,
    path: &Path,
) -> Result<PathBuf, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let canon = path.canonicalize().map_err(|e| e.to_string())?;
    let parent = canon.parent().ok_or("path has no parent directory")?;
    let root = cfg.root_path();
    let root = root.canonicalize().unwrap_or(root);
    if cfg.groups.iter().any(|g| root.join(&g.name) == parent) {
        Ok(canon)
    } else {
        Err(format!(
            "{} is not directly inside a group directory under {}",
            path.display(),
            root.display()
        ))
    }
}

/// A directory the GUI may open in an editor: an existing directory inside the workspace root.
pub fn check_open_path(cfg: &grove_core::config::Config, path: &Path) -> Result<PathBuf, String> {
    let canon = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !canon.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let root = cfg.root_path();
    let root = root.canonicalize().unwrap_or(root);
    if canon.starts_with(&root) && canon != root {
        Ok(canon)
    } else {
        Err(format!(
            "{} is not inside {}",
            path.display(),
            root.display()
        ))
    }
}

fn grove_core_default_config() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("grove/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_blocks_smuggled_flags() {
        let v = |a: &[&str]| validate(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(v(&["sync", "--config", "/x"]).is_err());
        assert!(v(&["init", "--force"]).is_err());
        assert!(v(&["resolve", "/p", "--compare"]).is_err());
        assert!(v(&["resolve", "/p", "--save-as", "--take-remote"]).is_err());
        assert!(v(&["secrets", "push", "f"]).is_err());
        assert!(v(&["new", "g", "-x"]).is_err());
        assert!(v(&["resolve", "/p", "--save-as", "b", "--take-remote"]).is_ok());
        assert!(v(&["new", "g", "n", "--public"]).is_ok());
        assert!(v(&["secrets", "pull"]).is_ok());
        assert!(v(&["group", "list"]).is_err());
        assert!(v(&["group", "add"]).is_err());
        assert!(v(&["group", "add", "g", "--primary", "p", "--force"]).is_err());
        assert!(
            v(&[
                "group",
                "add",
                "g",
                "--primary",
                "p",
                "--mirror-to",
                "q",
                "--replace"
            ])
            .is_ok()
        );
        assert!(v(&["provider", "remove", "p"]).is_ok());
        assert!(v(&["provider", "add", "p", "--url", "--kind"]).is_err());
        assert!(v(&["open", "/p", "--with", "zed"]).is_ok());
        assert!(v(&["open", "/p", "--print"]).is_err());
        assert!(v(&["open", "--with", "zed"]).is_err());
        assert!(v(&["new", "g", "n", "--open", "zed"]).is_ok());
    }

    #[test]
    fn open_path_must_be_inside_the_root() {
        let tmp = crate::test_tmp::TempDir::new("grove-open");
        let base = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("ws/work/app/main")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        let cfg = grove_core::config::Config::parse(&format!(
            "[machine]\nname=\"m\"\n[root]\npath=\"{}/ws\"\n",
            base.display()
        ))
        .unwrap();
        assert_eq!(
            check_open_path(&cfg, &base.join("ws/work/app/../app/main")).unwrap(),
            base.join("ws/work/app/main")
        );
        for bad in ["ws", "elsewhere", "ws/work/../../elsewhere", "ws/none"] {
            assert!(check_open_path(&cfg, &base.join(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn migrate_path_must_be_a_clone_directly_under_a_group() {
        let _tmp = crate::test_tmp::TempDir::new("grove-mig");
        let base = _tmp.path().to_path_buf();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("work/proj/sub")).unwrap();
        std::fs::create_dir_all(base.join("other/proj")).unwrap();
        std::fs::write(base.join("work/file"), "x").unwrap();
        std::os::unix::fs::symlink(base.join("other/proj"), base.join("work/link")).unwrap();
        let cfg = grove_core::config::Config::parse(&format!(
            "[machine]\nname=\"m\"\n[root]\npath=\"{}\"\n[providers.p]\nkind=\"gitea\"\n[[groups]]\nname=\"work\"\nprimary=\"p\"\n",
            base.display()
        ))
        .unwrap();
        assert!(check_migrate_path(&cfg, &base.join("work/proj")).is_ok());
        for bad in [
            "work/proj/sub",
            "other/proj",
            "work/file",
            "work/link",
            "work/none",
            "work",
            "work/../other/proj",
        ] {
            assert!(check_migrate_path(&cfg, &base.join(bad)).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn add_runs_only_for_valid_clone_paths() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = crate::test_tmp::TempDir::new("grove-add");
        let base = _tmp.path().to_path_buf();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("work/proj")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        let cfg = base.join("config.toml");
        std::fs::write(&cfg, format!(
            "[machine]\nname=\"m\"\n[root]\npath=\"{}\"\n[providers.p]\nkind=\"gitea\"\n[[groups]]\nname=\"work\"\nprimary=\"p\"\n",
            base.display()
        )).unwrap();
        // A stand-in `grove` that reports what it was asked to do.
        let fake = base.join("grove");
        std::fs::write(&fake, "#!/bin/sh\necho \"migrated: $4/wt\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let b = Bridge {
            grove_bin: fake,
            config: Some(cfg),
            socket: "/nonexistent".into(),
        };
        let ok = b.cli(
            &["add".into(), base.join("work/proj").display().to_string()],
            "",
        );
        assert_eq!(ok["ok"], true, "{ok}");
        assert!(ok["output"].as_str().unwrap().starts_with("migrated:"));
        // A path with `..`/symlink parts reaches `grove add` in canonical form.
        let sneaky = format!("{}/work/../work/proj", base.display());
        let canon = base.join("work/proj").canonicalize().unwrap();
        let r = b.cli(&["add".into(), sneaky], "");
        assert_eq!(r["output"], format!("migrated: {}/wt", canon.display()));
        let bad = b.cli(
            &["add".into(), base.join("elsewhere").display().to_string()],
            "",
        );
        assert_eq!(bad["ok"], false);
        assert!(bad["output"].as_str().unwrap().contains("rejected"));
        let flag = b.cli(&["add".into(), "--backup-dir".into(), "/x".into()], "");
        assert_eq!(flag["ok"], false);
        let _ = std::fs::remove_dir_all(&base);
    }

    fn bridge() -> Bridge {
        Bridge {
            grove_bin: "/nonexistent/grove".into(),
            config: None,
            socket: "/nonexistent/sock".into(),
        }
    }

    #[test]
    fn only_allow_listed_subcommands_run() {
        let r = bridge().cli(&["daemon".into(), "pause".into()], "");
        assert_eq!(r["ok"], false);
        assert!(r["output"].as_str().unwrap().contains("not allowed"));
        let r = bridge().cli(&["sync".into()], "");
        assert!(r["output"].as_str().unwrap().contains("cannot run"));
    }

    #[test]
    fn offline_daemon_is_a_regular_answer() {
        let r = bridge().ipc(&json!({"v": 1, "cmd": "status"}));
        assert_eq!(r["ok"], false);
        assert_eq!(r["offline"], true);
    }
}
