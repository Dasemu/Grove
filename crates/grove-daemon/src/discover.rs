//! Finds the repos managed under the workspace root: `root/<repo>` or `root/<group>/<repo>`,
//! where a repo has a `.bare` directory (bare + worktrees layout) or a `.git`.

use std::path::{Path, PathBuf};

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
        if is_repo(&child) {
            out.push(child);
        } else {
            out.extend(subdirs(&child).into_iter().filter(|g| is_repo(g)));
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
        ] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let found = discover(&base);
        assert_eq!(
            found,
            vec![base.join("a"), base.join("c/inner"), base.join("grp/b")]
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
