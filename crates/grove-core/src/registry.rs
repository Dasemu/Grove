//! The config repo: a private git repo holding `repos.toml` (what to materialize) and one
//! heartbeat file per machine. A fresh machine needs nothing else to rebuild its workspace.

use crate::git::{Git, GitError};
use crate::layout::dir_name;
use crate::receive::{ensure_fetch_refspec, materialize};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct Registry {
    #[serde(default)]
    pub repos: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Entry {
    pub group: String,
    pub name: String,
    /// Clone URL on the group's primary remote.
    pub url: String,
}

impl Registry {
    pub fn parse(text: &str) -> Result<Registry, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    pub fn to_text(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Adds or updates an entry (same group + name).
    pub fn upsert(&mut self, e: Entry) {
        match self
            .repos
            .iter_mut()
            .find(|x| x.group == e.group && x.name == e.name)
        {
            Some(x) => *x = e,
            None => self.repos.push(e),
        }
        self.repos
            .sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
    }
}

/// Creates `<dir>/.bare` as a clone of `url` and a worktree per remote branch.
/// `envs` are extra git environment variables (credentials).
///
/// `dir` must not exist or be empty. If anything fails, what this call created is removed again
/// (the `.bare`, the worktree directories, `dir` itself when it created it); if that is not
/// possible a `.grove-stage` marker is left so the daemon reports the repo as stalled instead of
/// adopting a half-built one.
pub fn clone_bare_layout(
    url: &str,
    dir: &Path,
    envs: &[(String, String)],
) -> Result<Vec<PathBuf>, GitError> {
    clone_bare_layout_impl(url, dir, envs, false)
}

#[doc(hidden)]
pub fn clone_bare_layout_failing_after_clone(
    url: &str,
    dir: &Path,
    envs: &[(String, String)],
) -> Result<Vec<PathBuf>, GitError> {
    clone_bare_layout_impl(url, dir, envs, true)
}

fn clone_bare_layout_impl(
    url: &str,
    dir: &Path,
    envs: &[(String, String)],
    fail_after_clone: bool,
) -> Result<Vec<PathBuf>, GitError> {
    let created_dir = !dir.exists();
    let non_empty = !created_dir
        && fs::read_dir(dir)
            .map_err(|e| GitError::Other(format!("{}: {e}", dir.display())))?
            .next()
            .is_some();
    if non_empty {
        return Err(GitError::Other(format!(
            "{} already exists and is not empty",
            dir.display()
        )));
    }
    // Released last: until then the daemon ignores the half-built repo.
    let busy = crate::busy::BusyGuard::acquire(dir)
        .map_err(|e| GitError::Other(format!("{}: {e}", dir.display())))?;
    let res = clone_steps(url, dir, envs, fail_after_clone);
    if res.is_err() {
        rollback_clone(dir);
    }
    drop(busy);
    if res.is_err() && created_dir {
        // Only while empty: `remove_dir` refuses anything else.
        let _ = fs::remove_dir(dir);
    }
    res
}

/// Removes everything in `dir` except the busy marker (the caller guarantees `dir` was empty
/// before the clone started). On failure, leaves `.grove-stage` behind.
fn rollback_clone(dir: &Path) {
    let mut failed = false;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.filter_map(Result::ok) {
            if e.file_name() == crate::busy::BUSY {
                continue;
            }
            let p = e.path();
            let r = if e.file_type().is_ok_and(|t| t.is_dir()) {
                fs::remove_dir_all(&p)
            } else {
                fs::remove_file(&p)
            };
            failed |= r.is_err();
        }
    } else {
        failed = true;
    }
    if failed {
        let _ = fs::write(
            dir.join(".grove-stage"),
            "a failed clone could not be cleaned up; remove this directory's contents by hand\n",
        );
    }
}

fn clone_steps(
    url: &str,
    dir: &Path,
    envs: &[(String, String)],
    fail_after_clone: bool,
) -> Result<Vec<PathBuf>, GitError> {
    let bare = dir.join(".bare");
    let mut g = Git::new(dir);
    for (k, v) in envs {
        g = g.with_env(k, v);
    }
    g.run(&["clone", "-q", "--bare", "--", url, &bare.to_string_lossy()])?;
    if fail_after_clone {
        return Err(GitError::Other("injected failure".into()));
    }
    Git::new(&bare).run(&["config", "core.bare", "true"])?;
    ensure_fetch_refspec(&bare, "origin")?;
    let mut f = Git::new(&bare);
    for (k, v) in envs {
        f = f.with_env(k, v);
    }
    f.run(&["fetch", "-q", "--prune", "origin"])?;
    // `clone --bare` creates local branches without upstream config; worktrees made from them
    // would then never be classified as ahead/behind.
    let b = Git::new(&bare);
    let heads = b.run(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    for br in heads.lines() {
        let up = format!("origin/{br}");
        if b.probe(&["rev-parse", "-q", "--verify", &format!("refs/remotes/{up}")])?
            .is_some()
        {
            b.run(&["branch", "-q", &format!("--set-upstream-to={up}"), br])?;
        }
    }
    materialize(dir, "origin")
}

/// Creates a brand new local repo in the bare layout with an initial empty commit on `main`,
/// pointing at `url`. Nothing is pushed here.
pub fn init_bare_layout(dir: &Path, url: &str) -> Result<PathBuf, GitError> {
    let _busy = crate::busy::BusyGuard::acquire(dir)?;
    let bare = dir.join(".bare");
    let g = Git::new(dir);
    g.run(&[
        "init",
        "-q",
        "--bare",
        "-b",
        "main",
        &bare.to_string_lossy(),
    ])?;
    let b = Git::new(&bare);
    b.run(&["remote", "add", "origin", url])?;
    ensure_fetch_refspec(&bare, "origin")?;
    // `hash-object -t tree` on an empty file writes the empty tree object.
    let tree = b.run(&["hash-object", "-t", "tree", "-w", "/dev/null"])?;
    let commit = b
        .with_env("GIT_AUTHOR_NAME", "grove")
        .with_env("GIT_AUTHOR_EMAIL", "grove@localhost")
        .with_env("GIT_COMMITTER_NAME", "grove")
        .with_env("GIT_COMMITTER_EMAIL", "grove@localhost")
        .run(&["commit-tree", &tree, "-m", "chore: initial commit"])?;
    let b = Git::new(&bare);
    b.run(&["update-ref", "refs/heads/main", &commit])?;
    let name = dir_name("main", &[]);
    let wt = dir.join(&name);
    b.run(&["worktree", "add", "-q", &wt.to_string_lossy(), "main"])?;
    Ok(wt)
}

/// Pushes `main` of a freshly initialised layout and sets the upstream.
pub fn push_initial(dir: &Path, envs: &[(String, String)]) -> Result<(), GitError> {
    let mut g = Git::new(dir.join(".bare"));
    for (k, v) in envs {
        g = g.with_env(k, v);
    }
    g.run(&["push", "-q", "-u", "origin", "main"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rollback_that_cannot_remove_everything_leaves_a_stage_marker() {
        let d = std::env::temp_dir().join(format!("grove-rb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let locked = d.join(".bare/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("f"), "x").unwrap();
        fs::write(d.join(crate::busy::BUSY), "1 1\n").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        rollback_clone(&d);
        assert!(d.join(".grove-stage").exists());
        assert!(
            d.join(crate::busy::BUSY).exists(),
            "the marker is the guard's to remove"
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_dir_all(&d);

        // Everything removable: nothing left but the busy marker, no stage marker.
        fs::create_dir_all(d.join(".bare")).unwrap();
        fs::create_dir_all(d.join("main")).unwrap();
        fs::write(d.join(crate::busy::BUSY), "1 1\n").unwrap();
        rollback_clone(&d);
        assert!(!d.join(".grove-stage").exists() && !d.join(".bare").exists());
        let _ = fs::remove_dir_all(&d);
    }
}
