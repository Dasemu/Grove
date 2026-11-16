//! Thin wrapper over the system `git` binary.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        let out = self.command(args).output()?;
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
        let out = self.command(args).output()?;
        if out.status.success() {
            Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            ))
        } else {
            Ok(None)
        }
    }
}
