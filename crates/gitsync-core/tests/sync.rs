mod common;
use common::*;
use gitsync_core::git::Git;
use gitsync_core::receive::Applied;
use gitsync_core::state::State;
use gitsync_core::sync::{Options, sync_repo};
use std::fs;
use std::path::Path;

fn opts(machine: &str, stamp: &str) -> Options {
    Options {
        machine: machine.into(),
        stamp: stamp.into(),
        remote: "origin".into(),
        offline: false,
        materialize: true,
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

    // A commits and pushes by hand; B (now dirty from the applied WIP) must not be touched.
    ga.run(&["add", "-A"]).unwrap();
    ga.run(&["commit", "-qm", "work"]).unwrap();
    ga.run(&["push", "-q"]).unwrap();
    let rb2 = sync_repo(&db, &opts("B", "t3")).unwrap();
    assert_eq!(rb2.worktrees[0].state, State::BehindDirty);
    assert_eq!(fs::read_to_string(db.join("a.txt")).unwrap(), "1\nwork\n");

    // Discarding B's applied copy makes it clean; the next sync fast-forwards it.
    Git::new(&db).run(&["checkout", "-q", "--", "."]).unwrap();
    fs::remove_file(db.join("scratch.txt")).unwrap();
    let rb3 = sync_repo(&db, &opts("B", "t4")).unwrap();
    assert_eq!(rb3.worktrees[0].state, State::Synced);
    assert!(db.join("scratch.txt").exists());
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
