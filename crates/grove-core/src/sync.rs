//! One full sync pass over a repo: snapshot, push, fetch, receive, apply, materialize, archive,
//! prune merged.
//! Failures of individual steps are reported per worktree and never abort the whole pass.

use crate::git::{Git, GitError};
use crate::inspect::{facts, state, worktrees};
use crate::prune::{MergedHead, Pruned, prune_merged, settled_branches};
use crate::receive::{
    Applied, Archived, Received, apply_snapshot, archive_gone, fetch, foreign_snapshots,
    materialize_except, push_snapshot, receive,
};
use crate::snapshot::{Outcome, ref_name, take};
use crate::state::State;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Options {
    pub machine: String,
    pub stamp: String,
    pub remote: String,
    /// Skip every network operation (fetch/push).
    pub offline: bool,
    /// Create worktrees for new remote branches and archive gone ones (bare layout only).
    pub materialize: bool,
    /// Take and push WIP snapshots. When false (private-only policy on a public or unknown
    /// repo, or `snapshots = "none"`) no `refs/sync/<machine>/*` ref is created or pushed;
    /// fetching, fast-forwarding and applying other machines' snapshots are unaffected.
    pub push_snapshots: bool,
    /// Heads of pull requests merged on the forge (empty: unknown or pruning disabled).
    /// Worktrees of those branches without local work are removed (bare layout only), and
    /// `materialize` does not bring them back.
    pub merged: Vec<MergedHead>,
    /// Names of ignored files/directories that do not count as local work when pruning.
    pub disposable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeReport {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub snapshot: Option<&'static str>,
    pub received: Option<Received>,
    pub applied: Option<(String, Applied)>,
    pub state: State,
    pub errors: Vec<String>,
    /// Large files left out of the snapshot (see `sync.max_file_mb`).
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncReport {
    pub fetch_error: Option<String>,
    pub materialized: Vec<PathBuf>,
    pub archived: Vec<Archived>,
    pub pruned: Vec<Pruned>,
    pub worktrees: Vec<WorktreeReport>,
}

/// Git dir to operate on: `<path>/.bare` for the bare layout, `path` otherwise.
pub fn git_dir_for(path: &Path) -> PathBuf {
    let bare = path.join(".bare");
    if bare.is_dir() {
        bare
    } else {
        path.to_path_buf()
    }
}

pub fn sync_repo(path: &Path, o: &Options) -> Result<SyncReport, GitError> {
    let gitdir = git_dir_for(path);
    let bare_layout = gitdir != path;
    let mut rep = SyncReport::default();

    if !o.offline
        && let Err(e) = fetch(&gitdir, &o.remote)
    {
        rep.fetch_error = Some(e.to_string());
    }
    if bare_layout && o.materialize && rep.fetch_error.is_none() {
        match archive_gone(path, &o.machine, &o.stamp) {
            Ok(a) => rep.archived = a,
            Err(e) => rep.fetch_error = Some(format!("archive: {e}")),
        }
    }
    if bare_layout && rep.fetch_error.is_none() && !o.offline {
        match prune_merged(path, &o.merged, &o.disposable, &o.machine, &o.stamp) {
            Ok(p) => rep.pruned = p,
            Err(e) => rep.fetch_error = Some(format!("prune merged: {e}")),
        }
    }
    if bare_layout && o.materialize && rep.fetch_error.is_none() {
        let skip = settled_branches(&gitdir, &o.remote, &o.merged, &o.machine);
        match skip.and_then(|skip| materialize_except(path, &o.remote, &skip)) {
            Ok(m) => rep.materialized = m,
            Err(e) => rep.fetch_error = Some(format!("materialize: {e}")),
        }
    }

    for wt in worktrees(&gitdir)? {
        let mut r = WorktreeReport {
            path: wt.path.clone(),
            branch: wt.branch.clone(),
            snapshot: None,
            received: None,
            applied: None,
            state: State::Synced,
            errors: Vec::new(),
            skipped: Vec::new(),
        };
        if let Some(branch) = &wt.branch {
            sync_worktree(&mut r, branch, o, rep.fetch_error.is_none());
        }
        r.state = state(&wt.path);
        rep.worktrees.push(r);
    }
    Ok(rep)
}

fn sync_worktree(r: &mut WorktreeReport, branch: &str, o: &Options, fetched: bool) {
    let path = r.path.clone();
    let g = Git::new(&path);
    let own_ref = ref_name(&o.machine, branch);
    let has_ref = matches!(
        g.probe(&["rev-parse", "-q", "--verify", &own_ref]),
        Ok(Some(_))
    );
    let dirty = match facts(&path) {
        Ok(f) => f.dirty,
        Err(e) => {
            r.errors.push(e.to_string());
            return;
        }
    };
    // A clean worktree still refreshes an existing snapshot so a stale WIP never lingers.
    if o.push_snapshots && (dirty || has_ref) {
        match take(&path, &o.machine, &o.stamp) {
            Ok(Outcome::Created(s)) => {
                r.snapshot = Some("created");
                r.skipped = s.skipped;
            }
            Ok(Outcome::Unchanged(_)) => r.snapshot = Some("unchanged"),
            Err(e) => {
                r.errors.push(format!("snapshot: {e}"));
                return;
            }
        }
        if !o.offline
            && let Err(e) = push_snapshot(&path, &o.machine, &o.remote)
        {
            r.errors.push(format!("push snapshot: {e}"));
        }
    }
    if !fetched {
        return;
    }
    match receive(&path, &o.machine, &o.stamp) {
        Ok(x) => r.received = Some(x),
        Err(e) => {
            r.errors.push(format!("receive: {e}"));
            return;
        }
    }
    match foreign_snapshots(&path, &o.machine) {
        Ok(snaps) => {
            for s in snaps {
                match apply_snapshot(&path, &s, &o.machine, &o.stamp) {
                    Ok(Applied::Applied) => {
                        r.applied = Some((s.machine, Applied::Applied));
                        break;
                    }
                    Ok(other) => r.applied = Some((s.machine, other)),
                    Err(e) => r.errors.push(format!("apply: {e}")),
                }
            }
        }
        Err(e) => r.errors.push(format!("snapshots: {e}")),
    }
}
