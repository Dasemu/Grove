//! Keeping this machine current (section 7.2): fetch, fast-forward, foreign snapshots,
//! materializing new branches and archiving gone ones. Never touches a dirty worktree.

use crate::git::{Git, GitError};
use crate::inspect::{facts, state};
use crate::layout::dir_name;
use crate::snapshot::ref_name;
use crate::state::State;
use std::fs;
use std::path::{Path, PathBuf};

pub const REMOTE: &str = "origin";
/// Local namespace where other machines' snapshot refs are fetched to.
pub const REMOTE_SYNC: &str = "refs/sync-remote";

fn backup_ref(machine: &str, stamp: &str) -> String {
    format!("refs/sync-backup/{machine}/{stamp}")
}

/// Make sure a bare repo created with `git clone --bare` can track remote branches.
pub fn ensure_fetch_refspec(repo: &Path, remote: &str) -> Result<(), GitError> {
    let g = Git::new(repo);
    let key = format!("remote.{remote}.fetch");
    if g.probe(&["config", "--get", &key])?.is_none() {
        g.run(&[
            "config",
            &key,
            &format!("+refs/heads/*:refs/remotes/{remote}/*"),
        ])?;
    }
    Ok(())
}

/// Fetch branches (pruning) and every machine's snapshot refs.
pub fn fetch(repo: &Path, remote: &str) -> Result<(), GitError> {
    let g = Git::new(repo);
    g.run(&["fetch", "-q", "--prune", remote])?;
    g.run(&[
        "fetch",
        "-q",
        remote,
        &format!("+refs/sync/*:{REMOTE_SYNC}/*"),
    ])?;
    Ok(())
}

/// Push this machine's snapshot ref for the worktree's branch (machine-owned, so forced).
pub fn push_snapshot(worktree: &Path, machine: &str, remote: &str) -> Result<(), GitError> {
    let g = Git::new(worktree);
    let branch = g
        .probe(&["symbolic-ref", "--short", "-q", "HEAD"])?
        .ok_or_else(|| GitError::Other("detached HEAD".into()))?;
    let r = ref_name(machine, &branch);
    g.run(&["push", "-q", "--force", remote, &format!("{r}:{r}")])?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    FastForwarded { from: String, to: String },
    Skipped(State),
}

/// Fast-forward a clean, strictly-behind worktree to its upstream. Anything else is skipped.
pub fn receive(worktree: &Path, machine: &str, stamp: &str) -> Result<Received, GitError> {
    let st = state(worktree);
    if st != State::Behind {
        return Ok(Received::Skipped(st));
    }
    let g = Git::new(worktree);
    let from = g.run(&["rev-parse", "HEAD"])?;
    g.run(&["update-ref", &backup_ref(machine, stamp), &from])?;
    g.run(&["merge", "-q", "--ff-only", "@{upstream}"])?;
    let to = g.run(&["rev-parse", "HEAD"])?;
    Ok(Received::FastForwarded { from, to })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignSnapshot {
    pub machine: String,
    pub commit: String,
    /// First parent: the branch commit the snapshot was taken on.
    pub based_on: String,
}

/// Other machines' fetched snapshots for the worktree's current branch.
pub fn foreign_snapshots(worktree: &Path, machine: &str) -> Result<Vec<ForeignSnapshot>, GitError> {
    let g = Git::new(worktree);
    let Some(branch) = g.probe(&["symbolic-ref", "--short", "-q", "HEAD"])? else {
        return Ok(vec![]);
    };
    let listing = g.run(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        REMOTE_SYNC,
    ])?;
    let mut out = Vec::new();
    for line in listing.lines() {
        let Some((name, oid)) = line.split_once(' ') else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&format!("{REMOTE_SYNC}/")) else {
            continue;
        };
        let Some((m, b)) = rest.split_once('/') else {
            continue;
        };
        if m == machine || b != branch {
            continue;
        }
        let based_on = g.run(&["rev-parse", &format!("{oid}^1")])?;
        out.push(ForeignSnapshot {
            machine: m.to_string(),
            commit: oid.to_string(),
            based_on,
        });
    }
    out.sort_by(|a, b| a.machine.cmp(&b.machine));
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Applied,
    /// Snapshot tree already equals the worktree's committed tree.
    Nothing,
    Declined(String),
}

/// Apply another machine's snapshot as working changes. Only when the worktree is clean
/// with no local commits ahead, and HEAD can fast-forward to the snapshot's base.
pub fn apply_snapshot(
    worktree: &Path,
    snap: &ForeignSnapshot,
    machine: &str,
    stamp: &str,
) -> Result<Applied, GitError> {
    let f = facts(worktree)?;
    if f.dirty {
        return Ok(Applied::Declined("worktree has local changes".into()));
    }
    if f.ahead {
        return Ok(Applied::Declined("worktree has unpushed commits".into()));
    }
    let g = Git::new(worktree);
    if g.probe(&["merge-base", "--is-ancestor", "HEAD", &snap.based_on])?
        .is_none()
    {
        return Ok(Applied::Declined(
            "snapshot is not based on a descendant of HEAD".into(),
        ));
    }
    let head = g.run(&["rev-parse", "HEAD"])?;
    let snap_tree = g.run(&["rev-parse", &format!("{}^{{tree}}", snap.commit)])?;
    if snap_tree == g.run(&["rev-parse", "HEAD^{tree}"])? {
        return Ok(Applied::Nothing);
    }
    g.run(&["update-ref", &backup_ref(machine, stamp), &head])?;
    if head != snap.based_on {
        g.run(&["merge", "-q", "--ff-only", &snap.based_on])?;
    }
    // Two-way merge: refuses (without changing anything) if it would clobber untracked files.
    g.run(&["read-tree", "-m", "-u", "HEAD", &snap.commit])?;
    // Present the changes as working changes, like on the machine that made them.
    g.run(&["reset", "-q"])?;
    Ok(Applied::Applied)
}

/// Create worktrees under `root` for remote branches that have none. Returns new paths.
pub fn materialize(root: &Path, remote: &str) -> Result<Vec<PathBuf>, GitError> {
    let bare = root.join(".bare");
    let g = Git::new(&bare);
    let prefix = format!("refs/remotes/{remote}/");
    let remotes = g.run(&["for-each-ref", "--format=%(refname)", &prefix])?;
    let existing_branches: Vec<String> = crate::inspect::worktrees(&bare)?
        .into_iter()
        .filter_map(|w| w.branch)
        .collect();
    let mut taken: Vec<String> = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let mut created = Vec::new();
    for r in remotes.lines() {
        let Some(branch) = r.strip_prefix(&prefix) else {
            continue;
        };
        if branch == "HEAD" || existing_branches.iter().any(|b| b == branch) {
            continue;
        }
        let refs: Vec<&str> = taken.iter().map(String::as_str).collect();
        let dir = dir_name(branch, &refs);
        let path = root.join(&dir);
        let path_str = path.to_string_lossy().into_owned();
        let local_exists = g
            .probe(&[
                "rev-parse",
                "-q",
                "--verify",
                &format!("refs/heads/{branch}"),
            ])?
            .is_some();
        if local_exists {
            g.run(&["worktree", "add", "-q", &path_str, branch])?;
        } else {
            g.run(&[
                "worktree",
                "add",
                "-q",
                "--track",
                "-b",
                branch,
                &path_str,
                &format!("{remote}/{branch}"),
            ])?;
        }
        taken.push(dir);
        created.push(path);
    }
    Ok(created)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Archived {
    /// Worktree removed and branch deleted; the tip is kept under `backup_ref`.
    Removed { branch: String, backup_ref: String },
    /// Worktree has local changes: kept and flagged.
    KeptDirty { branch: String },
}

/// Archive worktrees whose upstream branch is gone (merged/deleted on the remote).
pub fn archive_gone(root: &Path, machine: &str, stamp: &str) -> Result<Vec<Archived>, GitError> {
    let bare = root.join(".bare");
    let g = Git::new(&bare);
    let listing = g.run(&[
        "for-each-ref",
        "--format=%(refname:short)|%(upstream:track)",
        "refs/heads",
    ])?;
    let gone: Vec<&str> = listing
        .lines()
        .filter_map(|l| l.split_once('|'))
        .filter(|(_, t)| t.contains("gone"))
        .map(|(b, _)| b)
        .collect();
    let wts = crate::inspect::worktrees(&bare)?;
    let mut out = Vec::new();
    for branch in gone {
        let Some(wt) = wts.iter().find(|w| w.branch.as_deref() == Some(branch)) else {
            continue;
        };
        if facts(&wt.path)?.dirty {
            out.push(Archived::KeptDirty {
                branch: branch.to_string(),
            });
            continue;
        }
        let tip = g.run(&["rev-parse", &format!("refs/heads/{branch}")])?;
        let bref = format!("{}/{}", backup_ref(machine, stamp), branch);
        g.run(&["update-ref", &bref, &tip])?;
        g.run(&["worktree", "remove", &wt.path.to_string_lossy()])?;
        g.run(&["branch", "-D", branch])?;
        out.push(Archived::Removed {
            branch: branch.to_string(),
            backup_ref: bref,
        });
    }
    Ok(out)
}
