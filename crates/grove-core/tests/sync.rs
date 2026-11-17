mod common;
use common::*;
use grove_core::git::Git;
use grove_core::receive::Applied;
use grove_core::receive::Received;
use grove_core::state::State;
use grove_core::sync::{Options, sync_repo};
use std::fs;
use std::path::Path;

fn opts(machine: &str, stamp: &str) -> Options {
    Options {
        machine: machine.into(),
        stamp: stamp.into(),
        remote: "origin".into(),
        offline: false,
        materialize: true,
        push_snapshots: true,
        merged: vec![],
        disposable: vec![],
    }
}

fn clone(base: &Path, remote: &Path, name: &str) -> (Git, std::path::PathBuf) {
    let d = base.join(name);
    Git::new(base)
        .run(&["clone", "-q", remote.to_str().unwrap(), d.to_str().unwrap()])
        .unwrap();
    let g = Git::new(&d);
    g.run(&["config", "user.email", "t@t"]).unwrap();
    g.run(&["config", "user.name", "t"]).unwrap();
    (g, d)
}

#[test]
fn two_machines_converge_without_manual_git() {
    let base = tmpdir("sync");
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
    let seed = init(&base.join("seed"));
    seed.run(&["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    commit(&seed, "a.txt", "1\n");
    seed.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    let (ga, da) = clone(&base, &remote, "A");
    let (_gb, db) = clone(&base, &remote, "B");

    // A works (WIP only, nothing committed) and syncs.
    fs::write(da.join("a.txt"), "1\nwork\n").unwrap();
    fs::write(da.join("scratch.txt"), "s\n").unwrap();
    let ra = sync_repo(&da, &opts("A", "t1")).unwrap();
    assert_eq!(ra.worktrees[0].snapshot, Some("created"));
    assert!(
        ra.worktrees[0].errors.is_empty(),
        "{:?}",
        ra.worktrees[0].errors
    );

    // B syncs and ends up with A's work in progress.
    let rb = sync_repo(&db, &opts("B", "t2")).unwrap();
    assert_eq!(
        rb.worktrees[0].applied.as_ref().unwrap().1,
        Applied::Applied
    );
    assert_eq!(fs::read_to_string(db.join("a.txt")).unwrap(), "1\nwork\n");
    assert!(db.join("scratch.txt").exists());

    // A commits and pushes by hand. B's applied copy is byte-identical to that commit, so B
    // adopts it (mixed reset, no file touched) and ends clean.
    ga.run(&["add", "-A"]).unwrap();
    ga.run(&["commit", "-qm", "work"]).unwrap();
    ga.run(&["push", "-q"]).unwrap();
    let rb2 = sync_repo(&db, &opts("B", "t3")).unwrap();
    assert_eq!(rb2.worktrees[0].state, State::Synced);
    assert!(matches!(
        rb2.worktrees[0].received,
        Some(Received::FastForwarded { .. })
    ));
    assert_eq!(fs::read_to_string(db.join("a.txt")).unwrap(), "1\nwork\n");
    assert!(
        Git::new(&db)
            .run(&["status", "--porcelain"])
            .unwrap()
            .is_empty()
    );

    // If B has its own extra edit when the next upstream commit arrives, it must not be touched.
    fs::write(da.join("a.txt"), "1\nwork\nmore\n").unwrap();
    ga.run(&["commit", "-qam", "more"]).unwrap();
    ga.run(&["push", "-q"]).unwrap();
    fs::write(db.join("a.txt"), "1\nwork\nlocal\n").unwrap();
    let rb3 = sync_repo(&db, &opts("B", "t4")).unwrap();
    assert_eq!(rb3.worktrees[0].state, State::BehindDirty);
    assert_eq!(
        fs::read_to_string(db.join("a.txt")).unwrap(),
        "1\nwork\nlocal\n"
    );
}

#[test]
fn offline_pass_skips_network_and_never_fails() {
    let base = tmpdir("offline");
    let g = init(&base.join("p"));
    commit(&g, "a.txt", "1\n");
    fs::write(base.join("p/w.txt"), "w\n").unwrap();
    let mut o = opts("A", "t");
    o.offline = true;
    let r = sync_repo(&base.join("p"), &o).unwrap();
    assert_eq!(r.worktrees[0].snapshot, Some("created"));
    assert!(r.worktrees[0].errors.is_empty());
    assert!(r.fetch_error.is_none());
}

#[test]
fn unreachable_remote_is_reported_but_local_snapshot_still_taken() {
    let base = tmpdir("noremote");
    let g = init(&base.join("p"));
    commit(&g, "a.txt", "1\n");
    g.run(&["remote", "add", "origin", "/nonexistent/remote.git"])
        .unwrap();
    fs::write(base.join("p/w.txt"), "w\n").unwrap();
    let r = sync_repo(&base.join("p"), &opts("A", "t")).unwrap();
    assert!(r.fetch_error.is_some());
    assert_eq!(r.worktrees[0].snapshot, Some("created"));
    assert!(!r.worktrees[0].errors.is_empty(), "push failure surfaced");
    assert_eq!(r.worktrees[0].state, State::Dirty);
}

#[test]
fn staged_changes_block_adoption_so_the_index_is_never_reset() {
    let base = tmpdir("staged");
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
    let seed = init(&base.join("seed"));
    seed.run(&["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    commit(&seed, "a.txt", "1\n");
    seed.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    let (ga, da) = clone(&base, &remote, "A");
    let (_gb, db) = clone(&base, &remote, "B");
    fs::write(da.join("a.txt"), "1\nwork\n").unwrap();
    sync_repo(&da, &opts("A", "t1")).unwrap();
    sync_repo(&db, &opts("B", "t2")).unwrap();
    // B stages an intermediate version, then its worktree matches what A will commit.
    let gb = Git::new(&db);
    fs::write(db.join("a.txt"), "1\nstaged\n").unwrap();
    gb.run(&["add", "a.txt"]).unwrap();
    fs::write(db.join("a.txt"), "1\nwork\n").unwrap();
    ga.run(&["add", "-A"]).unwrap();
    ga.run(&["commit", "-qm", "work"]).unwrap();
    ga.run(&["push", "-q"]).unwrap();
    let r = sync_repo(&db, &opts("B", "t3")).unwrap();
    assert_eq!(r.worktrees[0].state, State::BehindDirty);
    assert_eq!(gb.run(&["show", ":a.txt"]).unwrap(), "1\nstaged");
}

#[test]
fn withheld_snapshots_are_neither_taken_nor_pushed_but_fast_forward_still_works() {
    let base = tmpdir("nosnap");
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
    let seed = init(&base.join("seed"));
    seed.run(&["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    commit(&seed, "a.txt", "1\n");
    seed.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    let (_g, d) = clone(&base, &remote, "A");
    fs::write(d.join("a.txt"), "1\nsecret\n").unwrap();
    let o = Options {
        push_snapshots: false,
        ..opts("A", "t1")
    };
    let r = sync_repo(&d, &o).unwrap();
    assert_eq!(r.worktrees[0].snapshot, None);
    let g = Git::new(&remote);
    assert!(g.run(&["for-each-ref", "refs/sync/"]).unwrap().is_empty());
    // Fast-forwarding keeps working.
    commit(&seed, "b.txt", "2\n");
    seed.run(&["push", "-q"]).unwrap();
    fs::write(d.join("a.txt"), "1\n").unwrap();
    let r = sync_repo(&d, &o).unwrap();
    assert!(matches!(
        r.worktrees[0].received,
        Some(Received::FastForwarded { .. })
    ));
    assert!(d.join("b.txt").exists());
}

#[test]
fn migrated_repo_with_linked_worktrees_syncs_only_what_it_can() {
    use grove_core::migrate::migrate;
    let base = tmpdir("synclinked");
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
    let seed = init(&base.join("seed"));
    seed.run(&["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    commit(&seed, "a.txt", "1\n");
    seed.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    seed.run(&["push", "-q", "origin", "main:feat/out"])
        .unwrap();
    let (ga, da) = clone(&base, &remote, "A");
    let nested = da.join(".claude/worktrees/wt1");
    ga.run(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        nested.to_str().unwrap(),
    ])
    .unwrap();
    let outside = base.join("A-feature");
    ga.run(&[
        "worktree",
        "add",
        "-q",
        "--track",
        "-b",
        "feat/out",
        outside.to_str().unwrap(),
        "origin/feat/out",
    ])
    .unwrap();
    migrate(&da, "A", &base.join("bk"), "1").unwrap();
    let nested = da.join("main/.claude/worktrees/wt1");
    fs::write(nested.join("a.txt"), "detached edit\n").unwrap();
    fs::write(outside.join("a.txt"), "outside edit\n").unwrap();

    let rep = sync_repo(&da, &opts("A", "t1")).unwrap();
    let find = |p: &Path| {
        rep.worktrees
            .iter()
            .find(|w| w.path == p.canonicalize().unwrap())
            .unwrap_or_else(|| panic!("{} not reported: {:?}", p.display(), rep.worktrees))
    };
    // Detached: reported, never snapshotted.
    let d = find(&nested);
    assert_eq!((d.branch.clone(), d.snapshot), (None, None));
    assert!(d.errors.is_empty(), "{:?}", d.errors);
    assert_eq!(d.state, State::Dirty);
    // Branch worktree elsewhere: synced under its own branch ref.
    let o = find(&outside);
    assert_eq!(o.snapshot, Some("created"));
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    let bare = Git::new(da.join(".bare"));
    assert!(bare.run(&["rev-parse", "refs/sync/A/feat/out"]).is_ok());
    assert!(
        bare.probe(&["rev-parse", "-q", "--verify", "refs/sync/A/HEAD"])
            .unwrap()
            .is_none()
    );

    // Gone upstream + clean: a worktree outside the layout is never archived or removed.
    Git::new(&outside)
        .run(&["checkout", "-q", "--", "."])
        .unwrap();
    seed.run(&["push", "-q", "origin", "--delete", "feat/out"])
        .unwrap();
    let rep = sync_repo(&da, &opts("A", "t2")).unwrap();
    assert!(rep.archived.is_empty(), "{:?}", rep.archived);
    assert!(outside.join("a.txt").exists());
}

#[test]
fn purge_removes_remote_only_own_refs_and_nothing_else() {
    use grove_core::receive::purge_own_snapshots;
    let base = tmpdir("purge");
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
    let seed = init(&base.join("seed"));
    seed.run(&["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    commit(&seed, "a.txt", "1\n");
    seed.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    // Own refs: one with a local copy, one only on the remote. Plus another machine's ref.
    seed.run(&["push", "-q", "origin", "main:refs/sync/A/main"])
        .unwrap();
    seed.run(&["push", "-q", "origin", "main:refs/sync/A/feat/x"])
        .unwrap();
    seed.run(&["push", "-q", "origin", "main:refs/sync/B/main"])
        .unwrap();
    seed.run(&["update-ref", "refs/sync/A/main", "HEAD"])
        .unwrap();
    let done = purge_own_snapshots(seed.dir(), "A", "origin").unwrap();
    assert_eq!(done, vec!["refs/sync/A/feat/x", "refs/sync/A/main"]);
    let refs = Git::new(&remote)
        .run(&["for-each-ref", "--format=%(refname)"])
        .unwrap();
    assert_eq!(refs, "refs/heads/main\nrefs/sync/B/main");
    assert!(
        seed.run(&["for-each-ref", "refs/sync/A/"])
            .unwrap()
            .is_empty()
    );
    // A remote that cannot be reached is a failure and keeps the local refs.
    seed.run(&["update-ref", "refs/sync/A/main", "HEAD"])
        .unwrap();
    seed.run(&[
        "remote",
        "set-url",
        "origin",
        base.join("nope.git").to_str().unwrap(),
    ])
    .unwrap();
    assert!(purge_own_snapshots(seed.dir(), "A", "origin").is_err());
    assert!(
        !seed
            .run(&["for-each-ref", "refs/sync/A/"])
            .unwrap()
            .is_empty()
    );
}
