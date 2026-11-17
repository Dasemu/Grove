//! Snapshots: capture the full worktree state into `refs/sync/<machine>/<branch>`
//! without touching the user's branch, index or history (section 7.1).

use crate::git::{Git, GitError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    pub refname: String,
    /// Changed files left out because they exceed the size limit.
    pub skipped: Vec<String>,
}

static MAX_FILE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Process-wide limit: changed files larger than this are left out of snapshots (0 = no limit).
/// Set once at startup from `sync.max_file_mb`.
pub fn set_max_file_bytes(n: u64) {
    MAX_FILE_BYTES.store(n, Ordering::Relaxed);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Created(Snapshot),
    /// The worktree tree equals the last snapshot's tree; nothing was written.
    Unchanged(String),
}

pub fn ref_name(machine: &str, branch: &str) -> String {
    format!("refs/sync/{machine}/{branch}")
}

struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn temp_index_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // A counter too: daemon workers snapshot several repos at once.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("grove-index-{}-{nanos}-{n}", std::process::id()))
}

/// Tree of everything in the worktree (tracked changes and untracked files, not ignored ones),
/// built in a temporary copy of the index so the real index is never modified. The returned
/// guard keeps the temporary index alive; `Git` has it set as `GIT_INDEX_FILE` and the author
/// identity forced.
fn stage(git: &Git) -> Result<(TempIndex, Git, String, Vec<String>), GitError> {
    let gitdir = PathBuf::from(git.run(&["rev-parse", "--absolute-git-dir"])?);
    let tmp = TempIndex(temp_index_path());
    let real_index = gitdir.join("index");
    if real_index.exists() {
        std::fs::copy(&real_index, &tmp.0)?;
    }
    let snap = git
        .clone()
        .with_env("GIT_INDEX_FILE", tmp.0.to_string_lossy())
        .with_env("GIT_AUTHOR_NAME", "grove")
        .with_env("GIT_AUTHOR_EMAIL", "grove@localhost")
        .with_env("GIT_COMMITTER_NAME", "grove")
        .with_env("GIT_COMMITTER_EMAIL", "grove@localhost");
    snap.run(&["add", "-A"])?;
    let skipped = drop_oversized(&snap, git.dir())?;
    let tree = snap.run(&["write-tree"])?;
    Ok((tmp, snap, tree, skipped))
}

/// Reverts oversized changed files in the temporary index to their HEAD version (or removes them
/// when HEAD has none). Returns their paths.
fn drop_oversized(snap: &Git, worktree: &Path) -> Result<Vec<String>, GitError> {
    let limit = MAX_FILE_BYTES.load(Ordering::Relaxed);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let has_head = snap
        .probe(&["rev-parse", "-q", "--verify", "HEAD"])?
        .is_some();
    let changed = if has_head {
        snap.run(&[
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            "HEAD",
        ])?
    } else {
        snap.run(&["ls-files", "-z"])?
    };
    let mut skipped = Vec::new();
    for rel in changed.split('\0').filter(|p| !p.is_empty()) {
        let big = std::fs::symlink_metadata(worktree.join(rel))
            .map(|m| m.is_file() && m.len() > limit)
            .unwrap_or(false);
        if !big {
            continue;
        }
        if has_head {
            snap.run(&["reset", "-q", "HEAD", "--", rel])?;
        } else {
            snap.run(&["rm", "--cached", "-q", "--ignore-unmatch", "--", rel])?;
        }
        skipped.push(rel.to_string());
    }
    Ok(skipped)
}

/// The tree a snapshot of `worktree` would have, without creating any object besides the tree.
pub fn worktree_tree(worktree: &Path) -> Result<String, GitError> {
    Ok(stage(&Git::new(worktree))?.2)
}

/// Take a snapshot of `worktree` for `machine`. `message_stamp` is embedded in the
/// commit message (callers pass a timestamp; keeping it a parameter keeps this testable).
pub fn take(worktree: &Path, machine: &str, message_stamp: &str) -> Result<Outcome, GitError> {
    let git = Git::new(worktree);
    let branch = git
        .probe(&["symbolic-ref", "--short", "-q", "HEAD"])?
        .ok_or_else(|| GitError::Other("detached HEAD: cannot snapshot".into()))?;
    let refname = ref_name(machine, &branch);
    let (_tmp, snap, tree, skipped) = stage(&git)?;

    let prev = snap.probe(&["rev-parse", "-q", "--verify", &refname])?;
    if let Some(p) = &prev {
        let prev_tree = snap.run(&["rev-parse", &format!("{p}^{{tree}}")])?;
        if prev_tree == tree {
            return Ok(Outcome::Unchanged(p.clone()));
        }
    }

    let head = snap.probe(&["rev-parse", "-q", "--verify", "HEAD"])?;
    let message = format!("grove snapshot {machine} {message_stamp}");
    let mut args: Vec<&str> = vec!["commit-tree", &tree];
    if let Some(h) = &head {
        args.extend(["-p", h]);
    }
    if let Some(p) = &prev {
        args.extend(["-p", p]);
    }
    args.extend(["-m", &message]);
    let commit = snap.run(&args)?;
    snap.run(&["update-ref", &refname, &commit])?;
    Ok(Outcome::Created(Snapshot {
        commit,
        tree,
        refname,
        skipped,
    }))
}
