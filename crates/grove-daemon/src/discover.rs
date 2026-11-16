//! Finds the repos managed under the workspace root: `root/<repo>` or `root/<group>/<repo>`,
//! where a repo has a `.bare` directory (bare + worktrees layout) or a `.git`.

use std::path::{Path, PathBuf};

/// A linked worktree (`.git` file pointing into `<repo>/.git/worktrees/<id>`) belongs to the repo
/// it was added from; treating it as a repo of its own would sync the same worktrees twice.
fn is_linked_worktree(p: &Path) -> bool {
    std::fs::read_to_string(p.join(".git")).is_ok_and(|s| {
        s.trim()
            .strip_prefix("gitdir:")
            .is_some_and(|g| g.trim().contains("/worktrees/"))
    })
}

fn is_repo(p: &Path) -> bool {
    p.join(".bare").is_dir() || p.join(".git").exists()
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

pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for child in subdirs(root) {
        if is_linked_worktree(&child) {
            continue;
        }
        if is_repo(&child) {
            out.push(child);
        } else {
            out.extend(
                subdirs(&child)
                    .into_iter()
                    .filter(|g| is_repo(g) && !is_linked_worktree(g)),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_repos_at_depth_one_and_two_and_skips_hidden() {
        let base = std::env::temp_dir().join(format!("grove-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in [
            "a/.git",
            "grp/b/.bare",
            "grp/plain",
            ".hidden/.git",
            "c/inner/.git",
            "grp/b/main",
        ] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        // Linked worktrees (siblings of their repo) are not repos of their own.
        let link = "gitdir: /x/grp/b/.bare/worktrees/b-feature\n";
        std::fs::create_dir_all(base.join("grp/b-feature")).unwrap();
        std::fs::write(base.join("grp/b-feature/.git"), link).unwrap();
        std::fs::create_dir_all(base.join("top-wt/sub/.git")).unwrap();
        std::fs::write(base.join("top-wt/.git"), link).unwrap();
        let found = discover(&base);
        assert_eq!(
            found,
            vec![base.join("a"), base.join("c/inner"), base.join("grp/b")]
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
