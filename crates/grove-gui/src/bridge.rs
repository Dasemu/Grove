//! Everything the GUI needs from the machine: daemon IPC, allow-listed `grove` calls, config.

use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// `grove` subcommands the GUI may run. Everything else is refused.
const ALLOWED: &[&str] = &[
    "init",
    "login",
    "new",
    "import",
    "config-repo-init",
    "bootstrap",
    "resolve",
    "secrets",
    "sync",
    "status",
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
                "--group",
                "--config-repo",
                "--url",
            ],
        ),
        "new" => (&["--public"], &[]),
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
        }
    }
    if sub == "secrets" && args.len() != 2 {
        return Err("secrets takes exactly one action".into());
    }
    Ok(())
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
