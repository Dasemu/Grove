//! Read-only inspection of worktrees: gathers `Facts` and classifies them.

use crate::git::{Git, GitError};
use crate::state::{Facts, State, classify};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// `None` for detached HEAD.
    pub branch: Option<String>,
}

/// Non-bare worktrees of the repository containing `path` (adopts foreign ones too).
pub fn worktrees(path: &Path) -> Result<Vec<Worktree>, GitError> {
    let out = Git::new(path).run(&["worktree", "list", "--porcelain"])?;
    let mut list = Vec::new();
    for block in out.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let mut wt = None;
        let mut branch = None;
        let mut bare = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                wt = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if line == "bare" {
                bare = true;
            }
        }
        if let (Some(path), false) = (wt, bare) {
            list.push(Worktree { path, branch });
        }
    }
    Ok(list)
}

/// Collect facts for one worktree. Without an upstream, ahead/behind are false.
pub fn facts(worktree: &Path) -> Result<Facts, GitError> {
    let git = Git::new(worktree);
    let dirty = !git.run(&["status", "--porcelain"])?.is_empty();
    let (mut ahead, mut behind) = (false, false);
    if git
        .probe(&["rev-parse", "--abbrev-ref", "@{upstream}"])?
        .is_some()
    {
        let counts = git.run(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
        let mut it = counts.split_whitespace();
        ahead = it.next().is_some_and(|n| n != "0");
        behind = it.next().is_some_and(|n| n != "0");
    }
    Ok(Facts {
        dirty,
        ahead,
        behind,
        error: None,
    })
}

/// Classify a worktree; git failures become `State::Error` instead of propagating.
pub fn state(worktree: &Path) -> State {
    match facts(worktree) {
        Ok(f) => classify(&f),
        Err(e) => State::Error(e.to_string()),
    }
}
