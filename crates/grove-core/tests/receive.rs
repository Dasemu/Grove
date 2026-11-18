mod common;
use common::*;
use grove_core::git::Git;
use grove_core::migrate::migrate;
use grove_core::receive::*;
use grove_core::snapshot::take;
use grove_core::state::State;
use std::fs;
use std::path::PathBuf;

struct World {
    _tmp: common::tmp::TempDir,
    base: PathBuf,
    remote: PathBuf,
    g1: Git,
    d1: PathBuf,
    g2: Git,
    d2: PathBuf,
}

/// Bare remote plus two plain clones (machines "m1" and "m2") sharing `main` with one commit.
fn world() -> World {
    let tmp = tmpdir("recv");
    let base = tmp.to_path_buf();
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
    let d1 = base.join("m1");
    let g1 = init(&d1);
    g1.run(&["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    commit(&g1, "a.txt", "1\n");
    commit(&g1, ".gitignore", "*.env\n");
    g1.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    let d2 = base.join("m2");
    Git::new(&base)
        .run(&[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            d2.to_str().unwrap(),
        ])
        .unwrap();
    let g2 = Git::new(&d2);
    g2.run(&["config", "user.email", "t@t"]).unwrap();
    g2.run(&["config", "user.name", "t"]).unwrap();
    World {
        _tmp: tmp,
        base,
        remote,
        g1,
        d1,
        g2,
        d2,
    }
}

#[test]
fn fetch_works_when_remote_has_no_snapshots() {
    let w = world();
    fetch(&w.d2, REMOTE).unwrap();
}

#[test]
fn clean_behind_worktree_fast_forwards_with_backup() {
    let w = world();
    commit(&w.g1, "b.txt", "2\n");
    w.g1.run(&["push", "-q"]).unwrap();
    fetch(&w.d2, REMOTE).unwrap();
    let old = w.g2.run(&["rev-parse", "HEAD"]).unwrap();
    let r = receive(&w.d2, "m2", "t1").unwrap();
    assert!(matches!(r, Received::FastForwarded { .. }));
    assert_eq!(
        w.g2.run(&["rev-parse", "HEAD"]).unwrap(),
        w.g1.run(&["rev-parse", "HEAD"]).unwrap()
    );
    assert!(w.d2.join("b.txt").exists());
    assert_eq!(
        w.g2.run(&["rev-parse", "refs/sync-backup/m2/t1/receive/main"])
            .unwrap(),
        old
    );
}

#[test]
fn dirty_worktree_is_never_touched() {
    let w = world();
    commit(&w.g1, "b.txt", "2\n");
    w.g1.run(&["push", "-q"]).unwrap();
    fs::write(w.d2.join("a.txt"), "1\nlocal edit\n").unwrap();
    fetch(&w.d2, REMOTE).unwrap();
    let head = w.g2.run(&["rev-parse", "HEAD"]).unwrap();
    assert_eq!(
        receive(&w.d2, "m2", "t").unwrap(),
        Received::Skipped(State::BehindDirty)
    );
    assert_eq!(w.g2.run(&["rev-parse", "HEAD"]).unwrap(), head);
    assert_eq!(
        fs::read_to_string(w.d2.join("a.txt")).unwrap(),
        "1\nlocal edit\n"
    );
    assert!(!w.d2.join("b.txt").exists());
}

#[test]
fn divergence_keeps_both_sides_intact() {
    let w = world();
    commit(&w.g1, "b.txt", "remote\n");
    w.g1.run(&["push", "-q"]).unwrap();
    commit(&w.g2, "c.txt", "local\n");
    fetch(&w.d2, REMOTE).unwrap();
    let head = w.g2.run(&["rev-parse", "HEAD"]).unwrap();
    assert_eq!(
        receive(&w.d2, "m2", "t").unwrap(),
        Received::Skipped(State::Diverged)
    );
    assert_eq!(w.g2.run(&["rev-parse", "HEAD"]).unwrap(), head);
    assert!(w.d2.join("c.txt").exists() && !w.d2.join("b.txt").exists());
    // The remote side is still reachable for resolution.
    assert!(w.g2.run(&["rev-parse", "origin/main"]).is_ok());
}

#[test]
fn snapshot_roundtrip_between_machines() {
    let w = world();
    // m1 commits (pushed) and then has work in progress on top.
    commit(&w.g1, "b.txt", "2\n");
    w.g1.run(&["push", "-q"]).unwrap();
    fs::write(w.d1.join("a.txt"), "1\nwip edit\n").unwrap();
    fs::write(w.d1.join("new.txt"), "untracked\n").unwrap();
    fs::write(w.d1.join("s.env"), "secret\n").unwrap();
    fs::remove_file(w.d1.join("b.txt")).unwrap();
    take(&w.d1, "m1", "t1").unwrap();
    push_snapshot(&w.d1, "m1", REMOTE).unwrap();

    // m2 is behind by one commit and clean.
    fetch(&w.d2, REMOTE).unwrap();
    let snaps = foreign_snapshots(&w.d2, "m2").unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].machine, "m1");
    assert!(
        foreign_snapshots(&w.d2, "m1").unwrap().is_empty(),
        "own snapshots excluded"
    );

    assert_eq!(
        apply_snapshot(&w.d2, &snaps[0], "m2", "t2").unwrap(),
        Applied::Applied
    );
    assert_eq!(
        fs::read_to_string(w.d2.join("a.txt")).unwrap(),
        "1\nwip edit\n"
    );
    assert!(w.d2.join("new.txt").exists());
    assert!(!w.d2.join("b.txt").exists(), "deletion applied");
    assert!(!w.d2.join("s.env").exists(), "ignored file never travels");
    // Arrives as plain working changes, nothing staged.
    assert_eq!(
        w.g2.run(&["diff", "--cached", "--name-only"]).unwrap(),
        "",
        "nothing staged"
    );
    // HEAD fast-forwarded to the snapshot's base.
    assert_eq!(w.g2.run(&["rev-parse", "HEAD"]).unwrap(), snaps[0].based_on);
}

#[test]
fn snapshot_is_declined_when_target_has_local_work() {
    let w = world();
    fs::write(w.d1.join("x.txt"), "x\n").unwrap();
    take(&w.d1, "m1", "t1").unwrap();
    push_snapshot(&w.d1, "m1", REMOTE).unwrap();
    fetch(&w.d2, REMOTE).unwrap();
    let snap = foreign_snapshots(&w.d2, "m2").unwrap().remove(0);

    fs::write(w.d2.join("mine.txt"), "m\n").unwrap();
    assert!(matches!(
        apply_snapshot(&w.d2, &snap, "m2", "t").unwrap(),
        Applied::Declined(_)
    ));
    assert!(!w.d2.join("x.txt").exists());
    fs::remove_file(w.d2.join("mine.txt")).unwrap();

    commit(&w.g2, "local.txt", "l\n");
    assert!(matches!(
        apply_snapshot(&w.d2, &snap, "m2", "t").unwrap(),
        Applied::Declined(_)
    ));
}

#[test]
fn snapshot_does_not_clobber_untracked_files() {
    let w = world();
    fs::write(w.d1.join("clash.txt"), "from m1\n").unwrap();
    take(&w.d1, "m1", "t1").unwrap();
    push_snapshot(&w.d1, "m1", REMOTE).unwrap();
    fetch(&w.d2, REMOTE).unwrap();
    let snap = foreign_snapshots(&w.d2, "m2").unwrap().remove(0);
    // Dirty (untracked) worktree is declined before anything is written.
    fs::write(w.d2.join("clash.txt"), "from m2\n").unwrap();
    assert!(matches!(
        apply_snapshot(&w.d2, &snap, "m2", "t").unwrap(),
        Applied::Declined(_)
    ));
    assert_eq!(
        fs::read_to_string(w.d2.join("clash.txt")).unwrap(),
        "from m2\n"
    );
}

#[test]
fn materializes_new_remote_branches_and_archives_gone_ones() {
    let w = world();
    // m2 becomes a bare+worktree repo.
    let r = migrate(&w.d2, "m2", &w.base.join("bk"), "1").unwrap();
    let root = w.d2.canonicalize().unwrap();
    assert_eq!(materialize(&root, REMOTE).unwrap().len(), 0);

    w.g1.run(&["checkout", "-q", "-b", "feat/x"]).unwrap();
    commit(&w.g1, "x.txt", "x\n");
    w.g1.run(&["push", "-q", "-u", "origin", "feat/x"]).unwrap();
    fetch(&r.bare_dir, REMOTE).unwrap();

    let created = materialize(&root, REMOTE).unwrap();
    assert_eq!(created, vec![root.join("feat-x")]);
    assert!(root.join("feat-x/x.txt").exists());
    assert_eq!(state_of(&root.join("feat-x")), State::Synced);
    assert!(materialize(&root, REMOTE).unwrap().is_empty(), "idempotent");

    // Remote branch deleted: clean worktree is archived with a backup ref.
    w.g1.run(&["push", "-q", "origin", "--delete", "feat/x"])
        .unwrap();
    fetch(&r.bare_dir, REMOTE).unwrap();
    let out = archive_gone(&root, "m2", "t9").unwrap();
    assert_eq!(out.len(), 1);
    assert!(matches!(&out[0], Archived::Removed { branch, .. } if branch == "feat/x"));
    assert!(!root.join("feat-x").exists());
    let bg = Git::new(&r.bare_dir);
    assert!(
        bg.run(&["rev-parse", "refs/sync-backup/m2/t9/archive/feat/x"])
            .is_ok()
    );
    assert!(
        bg.probe(&["rev-parse", "-q", "--verify", "refs/heads/feat/x"])
            .unwrap()
            .is_none()
    );
}

#[test]
fn gone_branch_with_local_changes_is_kept_and_flagged() {
    let w = world();
    let r = migrate(&w.d2, "m2", &w.base.join("bk"), "1").unwrap();
    let root = w.d2.canonicalize().unwrap();
    w.g1.run(&["checkout", "-q", "-b", "feat/y"]).unwrap();
    commit(&w.g1, "y.txt", "y\n");
    w.g1.run(&["push", "-q", "-u", "origin", "feat/y"]).unwrap();
    fetch(&r.bare_dir, REMOTE).unwrap();
    materialize(&root, REMOTE).unwrap();
    fs::write(root.join("feat-y/wip.txt"), "w\n").unwrap();
    w.g1.run(&["push", "-q", "origin", "--delete", "feat/y"])
        .unwrap();
    fetch(&r.bare_dir, REMOTE).unwrap();
    assert_eq!(
        archive_gone(&root, "m2", "t").unwrap(),
        vec![Archived::KeptDirty {
            branch: "feat/y".into()
        }]
    );
    assert!(root.join("feat-y/wip.txt").exists());
    let _ = &w.remote;
}

fn state_of(p: &std::path::Path) -> State {
    grove_core::inspect::state(p)
}

// ---- regression tests for the review findings ----

#[test]
fn archive_keeps_clean_worktree_that_holds_ignored_files() {
    let w = world();
    let r = migrate(&w.d2, "m2", &w.base.join("bk"), "1").unwrap();
    let root = w.d2.canonicalize().unwrap();
    w.g1.run(&["checkout", "-q", "-b", "feat/e"]).unwrap();
    commit(&w.g1, "e.txt", "e\n");
    w.g1.run(&["push", "-q", "-u", "origin", "feat/e"]).unwrap();
    fetch(&r.bare_dir, REMOTE).unwrap();
    materialize(&root, REMOTE).unwrap();
    fs::write(root.join("feat-e/local.env"), "SECRET=1\n").unwrap(); // ignored by *.env
    assert_eq!(
        state_of(&root.join("feat-e")),
        State::Synced,
        "ignored files are invisible to status"
    );
    w.g1.run(&["push", "-q", "origin", "--delete", "feat/e"])
        .unwrap();
    fetch(&r.bare_dir, REMOTE).unwrap();
    assert_eq!(
        archive_gone(&root, "m2", "t").unwrap(),
        vec![Archived::KeptDirty {
            branch: "feat/e".into()
        }]
    );
    assert_eq!(
        fs::read_to_string(root.join("feat-e/local.env")).unwrap(),
        "SECRET=1\n"
    );
}

#[test]
fn archive_does_nothing_when_remote_shows_no_branches() {
    let w = world();
    let r = migrate(&w.d2, "m2", &w.base.join("bk"), "1").unwrap();
    let root = w.d2.canonicalize().unwrap();
    let bg = Git::new(&r.bare_dir);
    for line in bg
        .run(&["for-each-ref", "--format=%(refname)", "refs/remotes"])
        .unwrap()
        .lines()
    {
        bg.run(&["update-ref", "-d", line]).unwrap();
    }
    assert!(archive_gone(&root, "m2", "t").unwrap().is_empty());
    assert!(root.join("main").exists());
}

#[test]
fn fast_forward_declines_instead_of_overwriting_local_ignored_file() {
    let w = world();
    commit(&w.g1, "conf.local", "theirs\n");
    w.g1.run(&["push", "-q"]).unwrap();
    fs::write(w.d2.join(".git/info/exclude"), "conf.local\n").unwrap();
    fs::write(w.d2.join("conf.local"), "mine\n").unwrap();
    fetch(&w.d2, REMOTE).unwrap();
    let head = w.g2.run(&["rev-parse", "HEAD"]).unwrap();
    assert!(matches!(
        receive(&w.d2, "m2", "t").unwrap(),
        Received::Declined(_)
    ));
    assert_eq!(w.g2.run(&["rev-parse", "HEAD"]).unwrap(), head);
    assert_eq!(
        fs::read_to_string(w.d2.join("conf.local")).unwrap(),
        "mine\n"
    );
}

#[test]
fn files_new_in_an_ignored_directory_do_not_block_a_fast_forward() {
    let w = world();
    commit(&w.g1, "tool/settings.json", "{}\n");
    w.g1.run(&["push", "-q"]).unwrap();
    // The whole directory is ignored and exists locally, but not the incoming file.
    fs::write(w.d2.join(".git/info/exclude"), "tool/\n").unwrap();
    fs::create_dir_all(w.d2.join("tool/cache")).unwrap();
    fs::write(w.d2.join("tool/cache/x"), "local\n").unwrap();
    fetch(&w.d2, REMOTE).unwrap();
    assert!(
        matches!(
            receive(&w.d2, "m2", "t").unwrap(),
            Received::FastForwarded { .. }
        ),
        "nothing local would be overwritten"
    );
    assert_eq!(
        fs::read_to_string(w.d2.join("tool/settings.json")).unwrap(),
        "{}\n"
    );
    assert_eq!(
        fs::read_to_string(w.d2.join("tool/cache/x")).unwrap(),
        "local\n"
    );
}

#[test]
fn an_existing_file_in_an_ignored_directory_still_blocks_a_fast_forward() {
    let w = world();
    commit(&w.g1, "tool/settings.json", "theirs\n");
    w.g1.run(&["push", "-q"]).unwrap();
    fs::write(w.d2.join(".git/info/exclude"), "tool/\n").unwrap();
    fs::create_dir_all(w.d2.join("tool")).unwrap();
    fs::write(w.d2.join("tool/settings.json"), "mine\n").unwrap();
    fetch(&w.d2, REMOTE).unwrap();
    assert!(matches!(
        receive(&w.d2, "m2", "t").unwrap(),
        Received::Declined(_)
    ));
    assert_eq!(
        fs::read_to_string(w.d2.join("tool/settings.json")).unwrap(),
        "mine\n"
    );
}

#[test]
fn snapshot_apply_declines_instead_of_overwriting_local_ignored_file() {
    let w = world();
    fs::write(w.d1.join("conf.local"), "theirs\n").unwrap();
    take(&w.d1, "m1", "t1").unwrap();
    push_snapshot(&w.d1, "m1", REMOTE).unwrap();
    fs::write(w.d2.join(".git/info/exclude"), "conf.local\n").unwrap();
    fs::write(w.d2.join("conf.local"), "mine\n").unwrap();
    fetch(&w.d2, REMOTE).unwrap();
    let snap = foreign_snapshots(&w.d2, "m2").unwrap().remove(0);
    assert!(matches!(
        apply_snapshot(&w.d2, &snap, "m2", "t").unwrap(),
        Applied::Declined(_)
    ));
    assert_eq!(
        fs::read_to_string(w.d2.join("conf.local")).unwrap(),
        "mine\n"
    );
}

#[test]
fn archive_and_receive_in_one_pass_do_not_collide_on_backup_refs() {
    use grove_core::sync::{Options, sync_repo};
    let w = world();
    migrate(&w.d2, "m2", &w.base.join("bk"), "1").unwrap();
    let root = w.d2.canonicalize().unwrap();
    let o = |stamp: &str| Options {
        machine: "m2".into(),
        stamp: stamp.into(),
        remote: "origin".into(),
        offline: false,
        materialize: true,
        push_snapshots: true,
        merged: vec![],
        disposable: vec![],
        prune: false,
        protected: vec![],
    };
    w.g1.run(&["checkout", "-q", "-b", "feat/z"]).unwrap();
    commit(&w.g1, "z.txt", "z\n");
    w.g1.run(&["push", "-q", "-u", "origin", "feat/z"]).unwrap();
    sync_repo(&root, &o("s1")).unwrap();
    assert!(root.join("feat-z").exists());

    // Same pass: feat/z disappears upstream AND main moves.
    w.g1.run(&["push", "-q", "origin", "--delete", "feat/z"])
        .unwrap();
    w.g1.run(&["checkout", "-q", "main"]).unwrap();
    commit(&w.g1, "m.txt", "m\n");
    w.g1.run(&["push", "-q", "origin", "main"]).unwrap();
    let rep = sync_repo(&root, &o("s2")).unwrap();
    assert_eq!(rep.archived.len(), 1);
    for wt in &rep.worktrees {
        assert!(wt.errors.is_empty(), "{:?}", wt.errors);
    }
    assert!(
        root.join("main/m.txt").exists(),
        "main fast-forwarded in the same pass"
    );
}
