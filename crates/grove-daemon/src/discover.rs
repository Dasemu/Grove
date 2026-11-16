//! Finds the repos managed under the workspace root: `root/<group>/<repo>` for the configured
//! groups, where the repo is in the bare + worktrees layout (has a `.bare` directory). Plain
//! clones are never managed (they were not migrated with `grove add`); they are only reported.

use std::path::{Path, PathBuf};

fn is_bare_layout(p: &Path) -> bool {
    p.join(".bare").is_dir()
}

fn is_plain_clone(p: &Path) -> bool {
    !is_bare_layout(p) && p.join(".git").exists()
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
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
            if is_bare_layout(&repo) {
                d.managed.push(repo);
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
            "grp/plain/.git",
            "grp/.hidden/.bare",
            "grp/notarepo/x",
            "other/b/.bare",
            "other/c/.git",
            "rootclone/.git",
            "rootbare/.bare",
            ".dot/b/.bare",
            "grp2/nested/deeper/.bare",
        ] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let found = discover(
            &base,
            &["grp".to_string(), "grp2".to_string(), "absent".into()],
        );
        assert_eq!(found.managed, vec![base.join("grp/migrated")]);
        assert_eq!(found.not_migrated, vec![base.join("grp/plain")]);
        assert_eq!(group_of(&base.join("grp/migrated")).as_deref(), Some("grp"));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
