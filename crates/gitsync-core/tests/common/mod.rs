#![allow(dead_code)]
use gitsync_core::git::Git;
use std::fs;
use std::path::{Path, PathBuf};

pub fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "gitsync-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

pub fn init(dir: &Path) -> Git {
    fs::create_dir_all(dir).unwrap();
    let g = Git::new(dir);
    g.run(&["init", "-q", "-b", "main"]).unwrap();
    g.run(&["config", "user.email", "t@t"]).unwrap();
    g.run(&["config", "user.name", "t"]).unwrap();
    g
}

pub fn commit(g: &Git, file: &str, body: &str) {
    let path = g.dir().join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    g.run(&["add", "."]).unwrap();
    g.run(&["commit", "-qm", file]).unwrap();
}

/// A repo with history, a second branch, a stash, staged, modified, untracked and ignored files.
pub fn messy_repo(dir: &Path) -> Git {
    let g = init(dir);
    commit(&g, ".gitignore", "*.env\n");
    commit(&g, "src/a.txt", "one\n");
    commit(&g, "b.txt", "b\n");
    g.run(&["branch", "other"]).unwrap();
    fs::write(dir.join("b.txt"), "b changed for stash\n").unwrap();
    g.run(&["stash", "push", "-q"]).unwrap();
    fs::write(dir.join("src/a.txt"), "one\nmodified\n").unwrap();
    fs::write(dir.join("staged.txt"), "staged\n").unwrap();
    g.run(&["add", "staged.txt"]).unwrap();
    fs::write(dir.join("untracked.txt"), "u\n").unwrap();
    fs::write(dir.join("secret.env"), "s\n").unwrap();
    g
}
