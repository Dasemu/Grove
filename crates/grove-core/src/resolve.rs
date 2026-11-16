//! Resolution of a `Diverged` worktree. Always an explicit choice by the user; every action
//! writes a backup ref first so nothing is ever lost.

use crate::git::{Git, GitError};
use crate::inspect::facts;
use crate::layout::dir_name;
use crate::receive::{backup_ref, ignored_conflicts};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Discard the local commits and move to the remote tip.
    TakeRemote,
    /// Overwrite the remote branch with the local one (force-with-lease on the observed tip).
    KeepLocal,
    /// Keep the local commits on a new branch (own worktree), then move this one to the remote tip.
    SaveAsBranch(String),
}

fn fail<T>(msg: impl Into<String>) -> Result<T, GitError> {
    Err(GitError::Other(msg.into()))
}

/// Applies `choice` to the diverged worktree. Returns a one-line description of what happened.
pub fn resolve(
    worktree: &Path,
    machine: &str,
    stamp: &str,
    choice: &Choice,
) -> Result<String, GitError> {
    let g = Git::new(worktree);
    let f = facts(worktree)?;
    if !(f.ahead && f.behind) {
        return fail("worktree is not diverged: nothing to resolve");
    }
    let branch = g
        .probe(&["symbolic-ref", "--short", "-q", "HEAD"])?
        .ok_or_else(|| GitError::Other("detached HEAD".into()))?;
    let upstream = g.run(&["rev-parse", "@{upstream}"])?;
    let head = g.run(&["rev-parse", "HEAD"])?;
    match choice {
        Choice::KeepLocal => {
            g.run(&[
                "update-ref",
                &backup_ref(machine, stamp, &format!("resolve-remote/{branch}")),
                &upstream,
            ])?;
            let remote = g.run(&["config", "--get", &format!("branch.{branch}.remote")])?;
            g.run(&[
                "push",
                "-q",
                &format!("--force-with-lease={branch}:{upstream}"),
                &remote,
                &format!("HEAD:refs/heads/{branch}"),
            ])?;
            Ok(format!(
                "remote {branch} overwritten with local (old tip kept at backup ref)"
            ))
        }
        Choice::TakeRemote | Choice::SaveAsBranch(_) => {
            if f.dirty {
                return fail("worktree has uncommitted changes: commit or stash them first");
            }
            let clash = ignored_conflicts(&g, &upstream)?;
            if !clash.is_empty() {
                return fail(format!(
                    "would overwrite ignored local files: {}",
                    clash.join(", ")
                ));
            }
            g.run(&[
                "update-ref",
                &backup_ref(machine, stamp, &format!("resolve-local/{branch}")),
                &head,
            ])?;
            let mut msg =
                format!("{branch} moved to the remote tip (local tip kept at backup ref)");
            if let Choice::SaveAsBranch(name) = choice {
                if g.probe(&["rev-parse", "-q", "--verify", &format!("refs/heads/{name}")])?
                    .is_some()
                {
                    return fail(format!("branch {name:?} already exists"));
                }
                let root = worktree
                    .parent()
                    .ok_or_else(|| GitError::Other("worktree has no parent dir".into()))?;
                if !root.join(".bare").is_dir() {
                    return fail("saving as a branch needs the bare + worktrees layout");
                }
                let taken: Vec<String> = std::fs::read_dir(root)?
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                let refs: Vec<&str> = taken.iter().map(String::as_str).collect();
                let dest = root.join(dir_name(name, &refs));
                g.run(&["branch", name, &head])?;
                g.run(&["worktree", "add", "-q", &dest.to_string_lossy(), name])?;
                msg = format!(
                    "local commits saved on branch {name} ({}); {msg}",
                    dest.display()
                );
            }
            g.run(&["reset", "-q", "--hard", &upstream])?;
            Ok(msg)
        }
    }
}
