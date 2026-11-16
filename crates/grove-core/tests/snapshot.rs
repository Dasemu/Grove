use grove_core::git::Git;
use grove_core::snapshot::{Outcome, take};
mod common;
use common::tmpdir;
use std::fs;
use std::path::Path;

fn repo(dir: &Path) -> Git {
    let g = Git::new(dir);
    g.run(&["init", "-q", "-b", "main"]).unwrap();
    g.run(&["config", "user.email", "t@t"]).unwrap();
    g.run(&["config", "user.name", "t"]).unwrap();
    fs::write(dir.join("a.txt"), "hi\n").unwrap();
    fs::write(dir.join(".gitignore"), "*.env\n").unwrap();
    g.run(&["add", "."]).unwrap();
    g.run(&["commit", "-qm", "init"]).unwrap();
    g
}

fn files(g: &Git, rev: &str) -> Vec<String> {
    g.run(&["ls-tree", "-r", "--name-only", rev])
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn snapshot_captures_wip_without_touching_user_state() {
    let dir = tmpdir("snap");
    let g = repo(&dir);
    fs::write(dir.join("a.txt"), "hi\nedit\n").unwrap();
    fs::write(dir.join("new.txt"), "untracked\n").unwrap();
    fs::write(dir.join("secret.env"), "x\n").unwrap();
    g.run(&["add", "a.txt"]).unwrap(); // staged edit

    let head_before = g.run(&["rev-parse", "HEAD"]).unwrap();
    let status_before = g.run(&["status", "--porcelain"]).unwrap();

    let out = take(&dir, "desktop", "t1").unwrap();
    let Outcome::Created(s) = out else {
        panic!("expected Created")
    };
    assert_eq!(s.refname, "refs/sync/desktop/main");
    let f = files(&g, &s.commit);
    assert!(f.contains(&"new.txt".to_string()), "untracked captured");
    assert!(!f.contains(&"secret.env".to_string()), "ignored excluded");
    assert_eq!(
        g.run(&["show", &format!("{}:a.txt", s.commit)]).unwrap(),
        "hi\nedit"
    );

    // User's branch, index and worktree are untouched.
    assert_eq!(g.run(&["rev-parse", "HEAD"]).unwrap(), head_before);
    assert_eq!(g.run(&["status", "--porcelain"]).unwrap(), status_before);
    assert_eq!(
        g.run(&["rev-parse", "refs/heads/main"]).unwrap(),
        head_before
    );
    // First parent is HEAD.
    assert_eq!(
        g.run(&["rev-parse", &format!("{}^1", s.commit)]).unwrap(),
        head_before
    );
}

#[test]
fn unchanged_tree_is_a_noop_and_changes_chain_snapshots() {
    let dir = tmpdir("chain");
    let g = repo(&dir);
    fs::write(dir.join("w.txt"), "1\n").unwrap();
    let Outcome::Created(first) = take(&dir, "m", "t1").unwrap() else {
        panic!()
    };
    assert_eq!(
        take(&dir, "m", "t2").unwrap(),
        Outcome::Unchanged(first.commit.clone())
    );

    fs::write(dir.join("w.txt"), "2\n").unwrap();
    let Outcome::Created(second) = take(&dir, "m", "t3").unwrap() else {
        panic!()
    };
    assert_eq!(
        g.run(&["rev-parse", &format!("{}^2", second.commit)])
            .unwrap(),
        first.commit
    );
}

#[test]
fn works_in_a_linked_worktree_of_a_bare_repo() {
    let base = tmpdir("bare");
    let src = base.join("src");
    fs::create_dir_all(&src).unwrap();
    repo(&src);
    let bare = base.join("repo/.bare");
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
    let wt = base.join("repo/main");
    bg.run(&["worktree", "add", "-q", wt.to_str().unwrap(), "main"])
        .unwrap();
    fs::write(wt.join("wip.txt"), "wip\n").unwrap();

    let Outcome::Created(s) = take(&wt, "laptop", "t").unwrap() else {
        panic!()
    };
    assert!(files(&bg, &s.commit).contains(&"wip.txt".to_string()));
}

#[test]
fn detached_head_is_an_error() {
    let dir = tmpdir("detached");
    let g = repo(&dir);
    g.run(&["checkout", "-q", "--detach"]).unwrap();
    assert!(take(&dir, "m", "t").is_err());
}
