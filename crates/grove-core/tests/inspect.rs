use grove_core::git::Git;
use grove_core::inspect::{state, worktrees};
use grove_core::state::State;
use std::fs;
use std::path::{Path, PathBuf};

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "grove-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

fn commit(g: &Git, dir: &Path, file: &str, body: &str) {
    fs::write(dir.join(file), body).unwrap();
    g.run(&["add", "."]).unwrap();
    g.run(&["commit", "-qm", file]).unwrap();
}

/// A bare "remote" plus two clones (two machines) tracking its `main`.
fn setup() -> (Git, PathBuf, Git, PathBuf) {
    let base = tmpdir("inspect");
    let remote = base.join("remote.git");
    Git::new(&base)
        .run(&[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ])
        .unwrap();
    let mut out = Vec::new();
    for name in ["m1", "m2"] {
        let dir = base.join(name);
        Git::new(&base)
            .run(&[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                dir.to_str().unwrap(),
            ])
            .unwrap();
        let g = Git::new(&dir);
        g.run(&["config", "user.email", "t@t"]).unwrap();
        g.run(&["config", "user.name", "t"]).unwrap();
        out.push((g, dir));
    }
    let (g2, d2) = out.pop().unwrap();
    let (g1, d1) = out.pop().unwrap();
    commit(&g1, &d1, "a.txt", "1\n");
    g1.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    g2.run(&["pull", "-q", "origin", "main"]).unwrap();
    g2.run(&["branch", "-q", "--set-upstream-to=origin/main", "main"])
        .unwrap();
    (g1, d1, g2, d2)
}

#[test]
fn classifies_real_repos() {
    let (g1, d1, g2, d2) = setup();
    assert_eq!(state(&d1), State::Synced);

    fs::write(d1.join("wip.txt"), "x").unwrap();
    assert_eq!(state(&d1), State::Dirty);
    fs::remove_file(d1.join("wip.txt")).unwrap();

    commit(&g1, &d1, "b.txt", "2\n");
    assert_eq!(state(&d1), State::Ahead);
    g1.run(&["push", "-q"]).unwrap();

    g2.run(&["fetch", "-q"]).unwrap();
    assert_eq!(state(&d2), State::Behind);
    fs::write(d2.join("wip2.txt"), "x").unwrap();
    assert_eq!(state(&d2), State::BehindDirty);
    fs::remove_file(d2.join("wip2.txt")).unwrap();

    commit(&g2, &d2, "c.txt", "3\n");
    assert_eq!(state(&d2), State::Diverged);
}

#[test]
fn lists_worktrees_skipping_bare() {
    let base = tmpdir("wtlist");
    let src = base.join("src");
    fs::create_dir_all(&src).unwrap();
    let g = Git::new(&src);
    g.run(&["init", "-q", "-b", "main"]).unwrap();
    g.run(&["config", "user.email", "t@t"]).unwrap();
    g.run(&["config", "user.name", "t"]).unwrap();
    commit(&g, &src, "a.txt", "1\n");
    let bare = base.join("r/.bare");
    fs::create_dir_all(bare.parent().unwrap()).unwrap();
    Git::new(&base)
        .run(&[
            "clone",
            "-q",
            "--bare",
            src.to_str().unwrap(),
            bare.to_str().unwrap(),
        ])
        .unwrap();
    let bg = Git::new(&bare);
    let wt = base.join("r/main");
    bg.run(&["worktree", "add", "-q", wt.to_str().unwrap(), "main"])
        .unwrap();
    let list = worktrees(&bare).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].branch.as_deref(), Some("main"));
}
