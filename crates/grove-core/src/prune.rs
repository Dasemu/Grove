//! Removing worktrees whose work was merged through a pull request on the forge.
//!
//! The forge (not git ancestry) says what was merged: squash and rebase merges leave no
//! ancestry, and a branch freshly created from the default branch is an ancestor of it too.
//! A worktree is removed only when its branch is the head of a merged pull request, its tip is
//! that head (or an ancestor of it), and it holds no local work: no uncommitted or untracked
//! files, no ignored files other than regenerable ones (`sync.prune_disposable`), no git
//! operation in progress. The tip is kept under a backup ref before anything is removed.

use crate::git::{Git, GitError};
use crate::inspect::worktrees;
use crate::receive::backup_ref;
use std::path::{Path, PathBuf};

/// The head of a pull request merged on the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedHead {
    pub number: u64,
    /// Branch name of the pull request's head (in the same repo; fork PRs are left out).
    pub branch: String,
    /// Commit the pull request's head pointed at when it was merged.
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pruned {
    /// Worktree removed and local branch deleted; the tip is kept under `backup_ref`.
    Removed {
        branch: String,
        pr: u64,
        path: PathBuf,
        backup_ref: String,
    },
    /// Merged, but the worktree holds something that is not on the forge: kept.
    Kept {
        branch: String,
        pr: u64,
        why: String,
    },
}

/// Backup-ref kind of a pruned branch (`refs/sync-backup/<machine>/<stamp>/merged/<branch>`).
const KIND: &str = "merged";

/// An operation in progress in `worktree` that removing it would break.
pub fn operation_in_progress(worktree: &Path) -> Option<&'static str> {
    let gd = Git::new(worktree)
        .run(&["rev-parse", "--absolute-git-dir"])
        .ok()?;
    let gd = PathBuf::from(gd);
    [
        "index.lock",
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
    ]
    .into_iter()
    .find(|f| gd.join(f).exists())
}

/// Why `worktree` holds local work, if it does. Ignored entries whose last path component is
/// in `disposable` (build output, dependency caches) do not count.
fn local_work(worktree: &Path, disposable: &[String]) -> Result<Option<String>, GitError> {
    if let Some(op) = operation_in_progress(worktree) {
        return Ok(Some(format!("git operation in progress ({op})")));
    }
    let st = Git::new(worktree).run(&["status", "--porcelain", "--ignored", "-z"])?;
    let mut kept = Vec::new();
    for entry in st.split('\0').filter(|e| e.len() > 3) {
        let (code, path) = entry.split_at(3);
        if code != "!! " {
            return Ok(Some("uncommitted or untracked changes".into()));
        }
        let last = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path);
        if !disposable.iter().any(|d| d == last) {
            kept.push(path.to_string());
        }
    }
    if kept.is_empty() {
        return Ok(None);
    }
    kept.truncate(5);
    Ok(Some(format!("ignored files: {}", kept.join(", "))))
}

fn is_ancestor(g: &Git, a: &str, b: &str) -> bool {
    matches!(g.probe(&["merge-base", "--is-ancestor", a, b]), Ok(Some(_)))
}

/// Removes the worktrees of the bare-layout repo at `root` whose branch was merged on the forge
/// (see the module docs). Only worktrees inside `root` are touched, never the one of the
/// default branch (`.bare`'s HEAD).
pub fn prune_merged(
    root: &Path,
    merged: &[MergedHead],
    disposable: &[String],
    machine: &str,
    stamp: &str,
) -> Result<Vec<Pruned>, GitError> {
    if merged.is_empty() {
        return Ok(vec![]);
    }
    let bare = root.join(".bare");
    let g = Git::new(&bare);
    let default = g.probe(&["symbolic-ref", "--short", "-q", "HEAD"])?;
    let canonical_root = root.canonicalize()?;
    let mut out = Vec::new();
    for wt in worktrees(&bare)? {
        let Some(branch) = wt.branch.as_deref() else {
            continue;
        };
        if default.as_deref() == Some(branch) || !wt.path.starts_with(&canonical_root) {
            continue;
        }
        let Ok(tip) = g.run(&["rev-parse", &format!("refs/heads/{branch}")]) else {
            continue;
        };
        // The newest merged PR of this branch whose head contains the local tip.
        let Some(pr) = merged
            .iter()
            .filter(|m| m.branch == branch)
            .find(|m| m.sha == tip || is_ancestor(&g, &tip, &m.sha))
        else {
            continue;
        };
        if !wt.path.is_dir() {
            continue;
        }
        if let Some(why) = local_work(&wt.path, disposable)? {
            out.push(Pruned::Kept {
                branch: branch.to_string(),
                pr: pr.number,
                why,
            });
            continue;
        }
        let bref = backup_ref(machine, stamp, &format!("{KIND}/{branch}"));
        g.run(&["update-ref", &bref, &tip])?;
        // `--force`: the only files left are disposable ignored ones (checked above).
        g.run(&["worktree", "remove", "--force", &wt.path.to_string_lossy()])?;
        g.run(&["branch", "-D", branch])?;
        out.push(Pruned::Removed {
            branch: branch.to_string(),
            pr: pr.number,
            path: wt.path.clone(),
            backup_ref: bref,
        });
    }
    Ok(out)
}

/// Branches `materialize` must not bring back: the remote tip is the head of a merged PR, or
/// the tip this machine pruned (its `merged/<branch>` backup ref). New commits pushed to the
/// branch after the merge make it materialize again.
pub fn settled_branches(
    bare: &Path,
    remote: &str,
    merged: &[MergedHead],
    machine: &str,
) -> Result<Vec<String>, GitError> {
    let g = Git::new(bare);
    let backups = g.run(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        &format!("refs/sync-backup/{machine}/"),
    ])?;
    let pruned: Vec<(&str, &str)> = backups
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter_map(|(sha, r)| {
            let rest = r.strip_prefix(&format!("refs/sync-backup/{machine}/"))?;
            let (_, kind) = rest.split_once('/')?;
            Some((sha, kind.strip_prefix(&format!("{KIND}/"))?))
        })
        .collect();
    let prefix = format!("refs/remotes/{remote}/");
    let tips = g.run(&["for-each-ref", "--format=%(objectname) %(refname)", &prefix])?;
    Ok(tips
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter_map(|(sha, r)| Some((sha, r.strip_prefix(&prefix)?)))
        .filter(|(sha, b)| {
            merged.iter().any(|m| m.branch == *b && m.sha == *sha)
                || pruned.iter().any(|(s, pb)| pb == b && s == sha)
        })
        .map(|(_, b)| b.to_string())
        .collect())
}
