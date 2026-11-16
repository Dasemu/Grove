mod common;
use common::*;
use grove_core::git::Git;
use grove_core::inspect::state;
use grove_core::registry::{clone_bare_layout, init_bare_layout, push_initial};
use grove_core::resolve::{Choice, resolve};
use grove_core::state::State;
use std::fs;
use std::path::PathBuf;

/// Two machines on the bare layout, both committed on main: diverged on B.
fn diverged() -> (PathBuf, PathBuf, PathBuf) {
    let base = tmpdir("resolve");
    let remote = base.join("r.git");
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
    let url = remote.to_str().unwrap().to_string();
    let a = base.join("A");
    init_bare_layout(&a, &url).unwrap();
    push_initial(&a, &[]).unwrap();
    let b = base.join("B");
    clone_bare_layout(&url, &b, &[]).unwrap();
    for (root, who) in [(&a, "a"), (&b, "b")] {
        let g = Git::new(root.join("main"));
        g.run(&["config", "user.email", "t@t"]).unwrap();
        g.run(&["config", "user.name", "t"]).unwrap();
        let _ = who;
    }
    // A commits and pushes; B commits locally and fetches.
    let ga = Git::new(a.join("main"));
    fs::write(a.join("main/f.txt"), "from A\n").unwrap();
    ga.run(&["add", "-A"]).unwrap();
    ga.run(&["commit", "-qm", "a"]).unwrap();
    ga.run(&["push", "-q"]).unwrap();
    let gb = Git::new(b.join("main"));
    fs::write(b.join("main/g.txt"), "from B\n").unwrap();
    gb.run(&["add", "-A"]).unwrap();
    gb.run(&["commit", "-qm", "b"]).unwrap();
    gb.run(&["fetch", "-q"]).unwrap();
    assert_eq!(state(&b.join("main")), State::Diverged);
    (base, a, b)
}

#[test]
fn take_remote_discards_local_commits_but_keeps_a_backup_ref() {
    let (_base, _a, b) = diverged();
    let wt = b.join("main");
    let msg = resolve(&wt, "B", "s1", &Choice::TakeRemote).unwrap();
    assert!(msg.contains("backup"));
    let g = Git::new(&wt);
    assert_eq!(state(&wt), State::Synced);
    assert!(wt.join("f.txt").exists() && !wt.join("g.txt").exists());
    let backup = g
        .run(&["rev-parse", "refs/sync-backup/B/s1/resolve-local/main"])
        .unwrap();
    assert!(g.run(&["show", &format!("{backup}:g.txt")]).is_ok());
}

#[test]
fn save_as_branch_keeps_the_local_work_in_its_own_worktree() {
    let (_base, _a, b) = diverged();
    let wt = b.join("main");
    resolve(&wt, "B", "s1", &Choice::SaveAsBranch("b-work".into())).unwrap();
    assert_eq!(state(&wt), State::Synced);
    assert!(!wt.join("g.txt").exists());
    assert_eq!(
        fs::read_to_string(b.join("b-work/g.txt")).unwrap(),
        "from B\n"
    );
}

#[test]
fn keep_local_force_pushes_with_lease_and_backs_up_the_remote_tip() {
    let (_base, a, b) = diverged();
    let wt = b.join("main");
    resolve(&wt, "B", "s1", &Choice::KeepLocal).unwrap();
    assert_eq!(state(&wt), State::Synced);
    let ga = Git::new(a.join("main"));
    ga.run(&["fetch", "-q"]).unwrap();
    let remote_tree = ga.run(&["ls-tree", "--name-only", "origin/main"]).unwrap();
    assert!(remote_tree.contains("g.txt") && !remote_tree.contains("f.txt"));
    let g = Git::new(&wt);
    assert!(
        g.run(&["rev-parse", "refs/sync-backup/B/s1/resolve-remote/main"])
            .is_ok()
    );
}

#[test]
fn refuses_when_not_diverged_or_when_the_worktree_is_dirty() {
    let (_base, _a, b) = diverged();
    let wt = b.join("main");
    fs::write(wt.join("wip.txt"), "x\n").unwrap();
    let e = resolve(&wt, "B", "s", &Choice::TakeRemote)
        .unwrap_err()
        .to_string();
    assert!(e.contains("uncommitted"), "{e}");
    assert!(wt.join("wip.txt").exists() && wt.join("g.txt").exists());
}
