//! Removing worktrees whose work is already merged.
//!
//! A branch counts as merged when either
//! - it is the head of a pull request merged on the forge and its tip is that head (or an
//!   ancestor of it): squash and rebase merges leave no ancestry, so the forge has to say so; or
//! - it was merged without a pull request: its tip was pushed (`<remote>/<branch>` points at it),
//!   is a strict ancestor of `<remote>/<default>` and is not on its first-parent history (a
//!   merge commit brought it in). A branch created from the default branch with no commits of
//!   its own is on that history, so it never counts; neither does a fast-forward merge.
//!
//! Branches named in `sync.prune_protected` (long-lived ones such as `development`) and the
//! default branch are never pruned. A worktree is removed only when it holds no local work: no
//! uncommitted or untracked files, no ignored files other than regenerable ones
//! (`sync.prune_disposable`), no git operation in progress. The tip is kept under a backup ref
//! before anything is removed.

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
        /// The merged pull request; `None` when merged into the default branch without one.
        pr: Option<u64>,
        path: PathBuf,
        backup_ref: String,
    },
    /// Merged, but the worktree holds something that is not on the forge: kept.
    Kept {
        branch: String,
        pr: Option<u64>,
        why: String,
    },
}

/// How a pruned branch was merged, for messages ("merged in PR #3").
pub fn merged_how(pr: Option<u64>) -> String {
    match pr {
        Some(n) => format!("merged in PR #{n}"),
        None => "merged into the default branch".into(),
    }
}

/// Long-lived branches that are never pruned; `sync.prune_protected` adds to them.
pub fn default_protected() -> Vec<String> {
    [
        "main",
        "master",
        "development",
        "develop",
        "dev",
        "staging",
        "production",
    ]
    .map(String::from)
    .to_vec()
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

fn resolve(g: &Git, r: &str) -> Option<String> {
    g.probe(&["rev-parse", "-q", "--verify", &format!("{r}^{{commit}}")])
        .ok()
        .flatten()
}

/// Merged without a pull request: `tip` was pushed as `<remote>/<branch>`, is a strict ancestor
/// of `<remote>/<default>` and is not on its first-parent history (a merge commit brought it).
fn merged_by_ancestry(g: &Git, remote: &str, default: &str, branch: &str, tip: &str) -> bool {
    let Some(base) = resolve(g, &format!("refs/remotes/{remote}/{default}")) else {
        return false;
    };
    resolve(g, &format!("refs/remotes/{remote}/{branch}")).as_deref() == Some(tip)
        && base != tip
        && is_ancestor(g, tip, &base)
        && !on_first_parent_chain(g, tip, &base)
}

/// True when `tip` is on the first-parent history of `base`: the default branch passed through
/// it. That is a branch created from the default branch with no commits of its own (or merged
/// by fast-forward, which the PR path still covers), so ancestry says nothing about a merge.
fn on_first_parent_chain(g: &Git, tip: &str, base: &str) -> bool {
    // On error, assume it is (never prune on doubt).
    g.run(&["rev-list", "--first-parent", base])
        .map_or(true, |l| l.lines().any(|c| c == tip))
}

/// What to prune and what to keep.
pub struct Rules<'a> {
    /// Remote whose default branch and branches the ancestry rule compares against.
    pub remote: &'a str,
    /// Heads of pull requests merged on the forge (may be empty: unknown).
    pub merged: &'a [MergedHead],
    /// Ignored file/directory names that do not count as local work.
    pub disposable: &'a [String],
    /// Branch names never pruned.
    pub protected: &'a [String],
}

/// Removes the worktrees of the bare-layout repo at `root` whose branch is already merged (see
/// the module docs). Only worktrees inside `root` are touched, never the one of the default
/// branch (`.bare`'s HEAD) nor protected branches.
pub fn prune_merged(
    root: &Path,
    rules: &Rules,
    machine: &str,
    stamp: &str,
) -> Result<Vec<Pruned>, GitError> {
    let bare = root.join(".bare");
    let g = Git::new(&bare);
    let default = g.probe(&["symbolic-ref", "--short", "-q", "HEAD"])?;
    let canonical_root = root.canonicalize()?;
    let mut out = Vec::new();
    for wt in worktrees(&bare)? {
        let Some(branch) = wt.branch.as_deref() else {
            continue;
        };
        if default.as_deref() == Some(branch)
            || rules.protected.iter().any(|p| p == branch)
            || !wt.path.starts_with(&canonical_root)
        {
            continue;
        }
        let Ok(tip) = g.run(&["rev-parse", &format!("refs/heads/{branch}")]) else {
            continue;
        };
        // The newest merged PR of this branch whose head contains the local tip, else a merge
        // without a PR.
        let pr = rules
            .merged
            .iter()
            .filter(|m| m.branch == branch)
            .find(|m| m.sha == tip || is_ancestor(&g, &tip, &m.sha))
            .map(|m| m.number);
        if pr.is_none()
            && !default
                .as_deref()
                .is_some_and(|d| merged_by_ancestry(&g, rules.remote, d, branch, &tip))
        {
            continue;
        }
        if !wt.path.is_dir() {
            continue;
        }
        if let Some(why) = local_work(&wt.path, rules.disposable)? {
            out.push(Pruned::Kept {
                branch: branch.to_string(),
                pr,
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
            pr,
            path: wt.path.clone(),
            backup_ref: bref,
        });
    }
    Ok(out)
}

/// Branches `materialize` must not bring back: the remote tip is the head of a merged PR, or
/// the tip this machine pruned (its `merged/<branch>` backup ref). New commits pushed to the
/// branch after the merge make it materialize again. Protected branches are never settled.
pub fn settled_branches(
    bare: &Path,
    remote: &str,
    merged: &[MergedHead],
    protected: &[String],
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
        .filter(|(_, b)| !protected.iter().any(|p| p == b))
        .filter(|(sha, b)| {
            merged.iter().any(|m| m.branch == *b && m.sha == *sha)
                || pruned.iter().any(|(s, pb)| pb == b && s == sha)
        })
        .map(|(_, b)| b.to_string())
        .collect())
}
