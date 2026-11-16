mod common;
use common::*;
use grove_core::git::Git;
use grove_core::migrate::migrate;
use grove_core::prune::{MergedHead, Pruned, prune_merged};
use grove_core::sync::{Options, sync_repo};
use std::fs;
use std::path::{Path, PathBuf};

struct World {
    _tmp: tmp::TempDir,
    base: PathBuf,
    /// Another machine's plain clone (plays the forge user who merges).
    dev: Git,
    /// This machine's bare-layout repo.
    root: PathBuf,
}

fn world() -> World {
    let tmp = tmpdir("prune");
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
    let dev = init(&base.join("dev"));
    dev.run(&["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    commit(&dev, ".gitignore", "*.env\ntarget/\n");
    dev.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    let d = base.join("me");
    Git::new(&base)
        .run(&["clone", "-q", remote.to_str().unwrap(), d.to_str().unwrap()])
        .unwrap();
    migrate(&d, "me", &base.join("bk"), "1").unwrap();
    World {
        _tmp: tmp,
        base,
        dev,
        root: d.canonicalize().unwrap(),
    }
}

fn opts(stamp: &str, merged: Vec<MergedHead>) -> Options {
    Options {
        machine: "me".into(),
        stamp: stamp.into(),
        remote: "origin".into(),
        offline: false,
        materialize: true,
        push_snapshots: false,
        merged,
        disposable: vec!["target".into()],
    }
}

/// Pushes branch `b` with one commit from `dev` and returns its tip.
fn push_branch(w: &World, b: &str) -> String {
    w.dev.run(&["checkout", "-q", "-b", b, "main"]).unwrap();
    commit(&w.dev, &format!("{}.txt", b.replace('/', "-")), "x\n");
    w.dev.run(&["push", "-q", "-u", "origin", b]).unwrap();
    let tip = w.dev.run(&["rev-parse", "HEAD"]).unwrap();
    w.dev.run(&["checkout", "-q", "main"]).unwrap();
    tip
}

/// A squash merge: the branch's content lands on main as a new commit (no ancestry), and the
/// head branch stays on the remote.
fn squash_merge(w: &World, b: &str) {
    w.dev.run(&["merge", "-q", "--squash", b]).unwrap();
    w.dev.run(&["commit", "-qm", "squash"]).unwrap();
    w.dev.run(&["push", "-q", "origin", "main"]).unwrap();
}

fn head(n: u64, branch: &str, sha: &str) -> MergedHead {
    MergedHead {
        number: n,
        branch: branch.into(),
        sha: sha.into(),
    }
}

fn has_branch(root: &Path, b: &str) -> bool {
    Git::new(root.join(".bare"))
        .probe(&["rev-parse", "-q", "--verify", &format!("refs/heads/{b}")])
        .unwrap()
        .is_some()
}

#[test]
fn a_squash_merged_worktree_is_removed_and_not_brought_back() {
    let w = world();
    let tip = push_branch(&w, "feat/x");
    sync_repo(&w.root, &opts("s1", vec![])).unwrap();
    let wt = w.root.join("feat-x");
    assert!(wt.exists());
    // Build output is regenerable: it does not keep the worktree.
    fs::create_dir_all(wt.join("target/debug")).unwrap();
    fs::write(wt.join("target/debug/bin"), "b").unwrap();
    squash_merge(&w, "feat/x");

    let rep = sync_repo(&w.root, &opts("s2", vec![head(7, "feat/x", &tip)])).unwrap();
    assert!(rep.fetch_error.is_none(), "{:?}", rep.fetch_error);
    let [
        Pruned::Removed {
            branch,
            pr,
            backup_ref,
            ..
        },
    ] = rep.pruned.as_slice()
    else {
        panic!("{:?}", rep.pruned);
    };
    assert_eq!((branch.as_str(), *pr), ("feat/x", 7));
    assert!(!wt.exists() && !has_branch(&w.root, "feat/x"));
    let bare = Git::new(w.root.join(".bare"));
    assert_eq!(bare.run(&["rev-parse", backup_ref]).unwrap(), tip);
    assert!(rep.materialized.is_empty(), "{:?}", rep.materialized);

    // The remote branch is still there; later passes (even without a forge answer) leave it.
    let rep = sync_repo(&w.root, &opts("s3", vec![])).unwrap();
    assert!(rep.materialized.is_empty() && !wt.exists());

    // New commits on the branch after the merge are new work: it comes back.
    w.dev.run(&["checkout", "-q", "feat/x"]).unwrap();
    commit(&w.dev, "more.txt", "m\n");
    w.dev.run(&["push", "-q", "origin", "feat/x"]).unwrap();
    let rep = sync_repo(&w.root, &opts("s4", vec![head(7, "feat/x", &tip)])).unwrap();
    assert_eq!(rep.materialized, vec![wt.clone()]);
    assert!(rep.pruned.is_empty());
}

#[test]
fn local_work_keeps_a_merged_worktree() {
    let w = world();
    let tip = push_branch(&w, "a");
    let tip_b = push_branch(&w, "b");
    let tip_c = push_branch(&w, "c");
    sync_repo(&w.root, &opts("s1", vec![])).unwrap();
    fs::write(w.root.join("a/new.txt"), "untracked\n").unwrap();
    fs::write(w.root.join("b/secret.env"), "ignored, not regenerable\n").unwrap();
    // A local commit the merged PR does not have.
    let gc = Git::new(w.root.join("c"));
    gc.run(&["config", "user.email", "t@t"]).unwrap();
    gc.run(&["config", "user.name", "t"]).unwrap();
    commit(&gc, "local.txt", "l\n");

    let merged = vec![
        head(1, "a", &tip),
        head(2, "b", &tip_b),
        head(3, "c", &tip_c),
    ];
    let rep = sync_repo(&w.root, &opts("s2", merged)).unwrap();
    let mut kept: Vec<(String, String)> = rep
        .pruned
        .iter()
        .map(|p| match p {
            Pruned::Kept { branch, why, .. } => (branch.clone(), why.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    kept.sort();
    assert_eq!(
        kept,
        vec![
            ("a".into(), "uncommitted or untracked changes".into()),
            ("b".into(), "ignored files: secret.env".into()),
        ]
    );
    for b in ["a", "b", "c"] {
        assert!(w.root.join(b).exists() && has_branch(&w.root, b), "{b}");
    }
}

#[test]
fn an_older_tip_is_pruned_but_the_default_branch_and_outside_worktrees_never_are() {
    let w = world();
    let first = push_branch(&w, "f");
    sync_repo(&w.root, &opts("s1", vec![])).unwrap();
    // The PR got more commits (from another machine) before it was merged.
    w.dev.run(&["checkout", "-q", "f"]).unwrap();
    commit(&w.dev, "f2.txt", "2\n");
    w.dev.run(&["push", "-q", "origin", "f"]).unwrap();
    let last = w.dev.run(&["rev-parse", "HEAD"]).unwrap();
    w.dev.run(&["checkout", "-q", "main"]).unwrap();
    assert_ne!(first, last);
    // A linked worktree outside the repo directory is the user's.
    let ext = w.base.join("elsewhere");
    let bare = Git::new(w.root.join(".bare"));
    let tip_o = push_branch(&w, "o");
    bare.run(&["fetch", "-q", "origin"]).unwrap();
    bare.run(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "o",
        ext.to_str().unwrap(),
        "origin/o",
    ])
    .unwrap();
    let main_tip = bare.run(&["rev-parse", "refs/heads/main"]).unwrap();

    // `prune_merged` alone: `f` is still at `first` locally (no fetch/fast-forward).
    let merged = vec![
        head(4, "f", &last),
        head(5, "main", &main_tip),
        head(6, "o", &tip_o),
    ];
    let pruned = prune_merged(&w.root, &merged, &[], "me", "t").unwrap();
    assert!(
        matches!(pruned.as_slice(), [Pruned::Removed { branch, pr: 4, .. }] if branch == "f"),
        "{pruned:?}"
    );
    assert!(w.root.join("main").exists() && ext.exists());
}
