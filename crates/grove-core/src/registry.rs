//! The config repo: a private git repo holding `repos.toml` (what to materialize) and one
//! heartbeat file per machine. A fresh machine needs nothing else to rebuild its workspace.

use crate::git::{Git, GitError};
use crate::layout::dir_name;
use crate::receive::{ensure_fetch_refspec, materialize};
use serde::{Deserialize, Serialize};
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
pub fn clone_bare_layout(
    url: &str,
    dir: &Path,
    envs: &[(String, String)],
) -> Result<Vec<PathBuf>, GitError> {
    // Released last: until then the daemon ignores the half-built repo.
    let _busy = crate::busy::BusyGuard::acquire(dir)?;
    let bare = dir.join(".bare");
    let mut g = Git::new(dir);
    for (k, v) in envs {
        g = g.with_env(k, v);
    }
    g.run(&["clone", "-q", "--bare", url, &bare.to_string_lossy()])?;
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
