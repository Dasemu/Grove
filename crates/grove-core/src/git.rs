//! Thin wrapper over the system `git` binary.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Wall-clock limit for one git invocation in seconds; 0 (the default) means none.
static TIMEOUT_SECS: AtomicU64 = AtomicU64::new(0);

/// Sets a wall-clock limit for every git invocation of this process (0 disables it). Meant for
/// the daemon, whose single sync loop must never block forever on a stuck fetch, hook or helper.
pub fn set_timeout(secs: u64) {
    TIMEOUT_SECS.store(secs, Ordering::Relaxed);
}

/// Runs `cmd`, killing it (and its process group: ssh, helpers) when it outlives the limit.
fn output(cmd: Command, args: &[String]) -> Result<Output, GitError> {
    output_within(cmd, args, TIMEOUT_SECS.load(Ordering::Relaxed))
}

fn output_within(mut cmd: Command, args: &[String], secs: u64) -> Result<Output, GitError> {
    if secs == 0 {
        return Ok(cmd.output()?);
    }
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let reader = |mut r: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        rx
    };
    let out_rx = reader(Box::new(child.stdout.take().expect("piped")));
    let err_rx = reader(Box::new(child.stderr.take().expect("piped")));
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut nap = Duration::from_millis(1);
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .status();
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitError::Other(format!(
                "git {} timed out after {secs} s and was killed",
                args.join(" ")
            )));
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(100));
    };
    // A lingering ssh master (ControlPersist) may keep the pipes open after git exits.
    let grace = Duration::from_secs(2);
    Ok(Output {
        status,
        stdout: out_rx.recv_timeout(grace).unwrap_or_default(),
        stderr: err_rx.recv_timeout(grace).unwrap_or_default(),
    })
}

#[derive(Debug)]
pub enum GitError {
    Spawn(io::Error),
    Failed {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    Other(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Spawn(e) => write!(f, "cannot run git: {e}"),
            GitError::Failed { args, code, stderr } => {
                write!(f, "git {} failed ({code:?}): {stderr}", args.join(" "))
            }
            GitError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for GitError {}

impl From<io::Error> for GitError {
    fn from(e: io::Error) -> Self {
        GitError::Spawn(e)
    }
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// A git invocation context: a working directory plus extra environment.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    envs: Vec<(String, String)>,
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Git {
            dir: dir.into(),
            envs: Vec::new(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    fn command<S: AsRef<OsStr>>(&self, args: &[S]) -> Command {
        let mut cmd = Command::new("git");
        // Fixed locale: callers parse git's output and messages.
        cmd.current_dir(&self.dir).args(args).env("LC_ALL", "C");
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        cmd
    }

    /// Run git, returning trimmed stdout. Any non-zero exit is an error.
    pub fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let out = output(self.command(args), &owned(args))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(GitError::Failed {
                args: args.iter().map(|s| s.to_string()).collect(),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    /// Run git; a non-zero exit yields `None` (for probes like `rev-parse --verify`).
    pub fn probe(&self, args: &[&str]) -> Result<Option<String>, GitError> {
        let out = output(self.command(args), &owned(args))?;
        if out.status.success() {
            Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            ))
        } else {
            Ok(None)
        }
    }
}

/// Environment that keeps unattended git from ever waiting on a person: no terminal or askpass
/// prompts (a desktop `SSH_ASKPASS` such as ksshaskpass would otherwise open a dialog and block
/// forever), and stalled HTTP transfers abort. The ssh command is left alone so a user's (or a
/// repo's) `core.sshCommand` keeps working; wall-clock limits come from [`set_timeout`].
pub fn noninteractive_env() -> Vec<(String, String)> {
    [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ASKPASS", "true"),
        ("SSH_ASKPASS_REQUIRE", "never"),
        ("GIT_HTTP_LOW_SPEED_LIMIT", "1000"),
        ("GIT_HTTP_LOW_SPEED_TIME", "60"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Whether the user configured their own ssh command for git (env or `core.sshCommand`).
pub fn has_custom_ssh() -> bool {
    std::env::var_os("GIT_SSH_COMMAND").is_some()
        || std::env::var_os("GIT_SSH").is_some()
        || std::process::Command::new("git")
            .args(["config", "--get", "core.sshCommand"])
            .output()
            .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn unattended_git_never_prompts_and_leaves_the_ssh_command_alone() {
        let env = noninteractive_env();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
        assert_eq!(get("SSH_ASKPASS_REQUIRE"), Some("never"));
        assert_eq!(get("GIT_SSH_COMMAND"), None);
    }

    #[test]
    fn a_git_call_over_the_limit_is_killed() {
        // A hook-like stall: `git -c alias.x='!sleep 30' x`. The global limit is not touched, so
        // tests running in parallel are unaffected.
        let args = owned(&["-c", "alias.zzsleep=!sleep 30", "zzsleep"]);
        let t = Instant::now();
        let r = output_within(Git::new(".").command(&args), &args, 1);
        assert!(r.unwrap_err().to_string().contains("timed out"));
        assert!(t.elapsed() < Duration::from_secs(10));
        let v = owned(&["--version"]);
        let out = output_within(Git::new(".").command(&v), &v, 5).unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("git version"));
    }
}
