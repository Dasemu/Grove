//! Finds the repos managed under the workspace root: `root/<group>/<repo>` for the configured
//! groups, where the repo is in the bare + worktrees layout (has a `.bare` directory). Plain
//! clones are never managed (they were not migrated with `grove add`); they are only reported.

use grove_core::busy::{BusyState, busy_state};
use std::path::{Path, PathBuf};

pub fn is_bare_layout(p: &Path) -> bool {
    p.join(".bare").is_dir()
}

/// A linked worktree (`.git` file pointing into `<repo>/.git/worktrees/<id>`) belongs to the repo
/// it was added from; treating it as a repo of its own would sync the same worktrees twice.
/// A repo the daemon may touch: complete bare layout (`.bare/HEAD`), no migration/clone in
/// progress (`.grove-busy`) and no interrupted migration (`.grove-stage`).
pub fn is_ready(p: &Path) -> bool {
    is_bare_layout(p)
        && p.join(".bare/HEAD").exists()
        && !p.join(".grove-stage").exists()
        && busy_state(p) == BusyState::Free
}

fn is_linked_worktree(p: &Path) -> bool {
    std::fs::read_to_string(p.join(".git")).is_ok_and(|s| {
        s.trim()
            .strip_prefix("gitdir:")
            .is_some_and(|g| g.trim().contains("/worktrees/"))
    })
}

fn is_plain_clone(p: &Path) -> bool {
    !is_bare_layout(p) && p.join(".git").exists() && !is_linked_worktree(p)
}

pub fn subdirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// What lives under the root, as far as the daemon is concerned.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Bare-layout repos in configured groups: these are synced.
    pub managed: Vec<PathBuf>,
    /// Plain clones in configured groups: ignored, reported as "not migrated".
    pub not_migrated: Vec<PathBuf>,
    /// Bare-layout repos left alone because an interrupted `grove add`/clone left a stale
    /// `.grove-busy` or a `.grove-stage` behind.
    pub stalled: Vec<PathBuf>,
}

/// Looks only at `root/<group>/<repo>` for each name in `groups` (cheap: one `read_dir` per
/// group). Directories that are not configured groups are never entered.
pub fn discover(root: &Path, groups: &[String]) -> Discovery {
    let mut d = Discovery::default();
    let mut groups: Vec<&String> = groups.iter().collect();
    groups.sort();
    groups.dedup();
    for g in groups {
        for repo in subdirs(&root.join(g)) {
            if is_ready(&repo) {
                d.managed.push(repo);
            } else if is_bare_layout(&repo) {
                if busy_state(&repo) == BusyState::Stale || repo.join(".grove-stage").exists() {
                    d.stalled.push(repo);
                }
            } else if is_plain_clone(&repo) {
                d.not_migrated.push(repo);
            }
        }
    }
    d
}

/// The group a managed repo belongs to: the name of its parent directory.
pub fn group_of(repo: &Path) -> Option<String> {
    Some(repo.parent()?.file_name()?.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bare_layout_repos_in_configured_groups_are_managed() {
        let base = std::env::temp_dir().join(format!("grove-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in [
            "grp/migrated/.bare",
            "grp/halfway/.bare",
            "grp/busy/.bare",
            "grp/staged/.bare",
            "grp/staged/.grove-stage",
            "grp/plain/.git",
            "grp/.hidden/.bare",
            "grp/notarepo/x",
            "other/b/.bare",
            "other/c/.git",
            "rootclone/.git",
            "rootbare/.bare",
            ".dot/b/.bare",
            "grp2/nested/deeper/.bare",
            "grp/migrated/main",
        ] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        for r in ["migrated", "busy", "staged"] {
            std::fs::write(
                base.join("grp").join(r).join(".bare/HEAD"),
                "ref: refs/heads/main\n",
            )
            .unwrap();
        }
        // An active marker (this process) hides the repo; a stale one also gets it reported.
        std::fs::write(
            base.join("grp/busy/.grove-busy"),
            format!("{} 1\n", std::process::id()),
        )
        .unwrap();
        // A linked worktree sitting next to its repo is neither managed nor "not migrated".
        std::fs::create_dir_all(base.join("grp/migrated-feature")).unwrap();
        std::fs::write(
            base.join("grp/migrated-feature/.git"),
            "gitdir: /x/grp/migrated/.bare/worktrees/migrated-feature\n",
        )
        .unwrap();
        let found = discover(
            &base,
            &["grp".to_string(), "grp2".to_string(), "absent".into()],
        );
        assert_eq!(found.managed, vec![base.join("grp/migrated")]);
        assert_eq!(found.stalled, vec![base.join("grp/staged")]);
        assert_eq!(found.not_migrated, vec![base.join("grp/plain")]);
        assert_eq!(group_of(&base.join("grp/migrated")).as_deref(), Some("grp"));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
