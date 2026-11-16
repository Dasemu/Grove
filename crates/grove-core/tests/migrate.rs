mod common;
use common::*;
use grove_core::git::Git;
use grove_core::migrate::{MigrateError, Step, migrate, migrate_failing_after};
use std::fs;

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn migrates_preserving_all_local_state() {
    let base = tmpdir("mig");
    let repo = base.join("proj");
    let g = messy_repo(&repo);
    let status = g.run(&["status", "--porcelain", "--ignored"]).unwrap();
    let stash = g.run(&["stash", "list", "--format=%H"]).unwrap();
    let branches = g
        .run(&["branch", "--format=%(refname) %(objectname)"])
        .unwrap();

    let r = migrate(&repo, "desktop", &base.join("backups"), "20261116").unwrap();

    assert_eq!(names(&repo), vec![".bare", "main"]);
    assert_eq!(r.worktree, repo.canonicalize().unwrap().join("main"));
    let wt = Git::new(&r.worktree);
    assert_eq!(
        wt.run(&["status", "--porcelain", "--ignored"]).unwrap(),
        status
    );
    assert_eq!(wt.run(&["stash", "list", "--format=%H"]).unwrap(), stash);
    assert_eq!(
        wt.run(&["branch", "--format=%(refname) %(objectname)"])
            .unwrap(),
        branches
    );
    assert!(
        status.contains("A  staged.txt"),
        "staged state kept: {status}"
    );
    assert!(r.worktree.join("secret.env").exists(), "ignored file kept");
    assert!(r.worktree.join("untracked.txt").exists());
    assert_eq!(
        fs::read_to_string(r.worktree.join("src/a.txt")).unwrap(),
        "one\nmodified\n"
    );

    // Backup exists and the backup ref points at the old HEAD.
    assert!(r.backup_tar.metadata().unwrap().len() > 0);
    assert_eq!(
        wt.run(&["rev-parse", &r.backup_ref]).unwrap(),
        wt.run(&["rev-parse", "HEAD"]).unwrap()
    );
    // Repo is bare now; the worktree is still fully usable.
    assert_eq!(
        Git::new(&r.bare_dir)
            .run(&["rev-parse", "--is-bare-repository"])
            .unwrap(),
        "true"
    );
    commit(&wt, "after.txt", "x\n");
}

#[test]
fn slash_branch_gets_flattened_directory() {
    let base = tmpdir("slash");
    let repo = base.join("p");
    let g = init(&repo);
    commit(&g, "a.txt", "1\n");
    g.run(&["checkout", "-q", "-b", "feat/login"]).unwrap();
    let r = migrate(&repo, "m", &base.join("bk"), "1").unwrap();
    assert!(r.worktree.ends_with("feat-login"));
    assert_eq!(names(&repo), vec![".bare", "feat-login"]);
}

#[test]
fn every_failure_point_rolls_back_to_the_original_layout() {
    for step in [
        Step::Staged,
        Step::GitMoved,
        Step::WorktreeAdded,
        Step::Filled,
        Step::IndexMoved,
        Step::Verified,
    ] {
        let base = tmpdir("rb");
        let repo = base.join("proj");
        let g = messy_repo(&repo);
        let status = g.run(&["status", "--porcelain", "--ignored"]).unwrap();
        let stash = g.run(&["stash", "list", "--format=%H"]).unwrap();
        let before_names = names(&repo);

        let err = migrate_failing_after(&repo, "m", &base.join("bk"), "1", step).unwrap_err();
        let MigrateError::Failed {
            rolled_back,
            backup,
            ..
        } = err
        else {
            panic!("{step:?}: expected Failed")
        };
        assert!(rolled_back, "{step:?}: rollback incomplete");
        assert!(backup.exists());

        assert_eq!(names(&repo), before_names, "{step:?}: layout restored");
        let g = Git::new(&repo);
        assert_eq!(
            g.run(&["rev-parse", "--is-bare-repository"]).unwrap(),
            "false",
            "{step:?}"
        );
        assert_eq!(
            g.run(&["status", "--porcelain", "--ignored"]).unwrap(),
            status,
            "{step:?}"
        );
        assert_eq!(
            g.run(&["stash", "list", "--format=%H"]).unwrap(),
            stash,
            "{step:?}"
        );
        assert_eq!(
            g.run(&["worktree", "list", "--porcelain"])
                .unwrap()
                .matches("worktree ")
                .count(),
            1
        );
        // And a retry succeeds afterwards.
        migrate(&repo, "m", &base.join("bk"), "2").unwrap();
    }
}

fn refused(r: Result<grove_core::migrate::Report, MigrateError>) -> String {
    match r {
        Err(MigrateError::Refused(m)) => m,
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn refuses_unsafe_situations_without_touching_anything() {
    let base = tmpdir("ref");
    let bk = base.join("bk");

    let detached = base.join("detached");
    let g = init(&detached);
    commit(&g, "a", "1");
    g.run(&["checkout", "-q", "--detach"]).unwrap();
    assert!(refused(migrate(&detached, "m", &bk, "1")).contains("detached"));

    let unborn = base.join("unborn");
    init(&unborn);
    assert!(refused(migrate(&unborn, "m", &bk, "1")).contains("no commits"));

    let merging = base.join("merging");
    let g = init(&merging);
    commit(&g, "a", "1");
    fs::write(merging.join(".git/MERGE_HEAD"), "x").unwrap();
    assert!(refused(migrate(&merging, "m", &bk, "1")).contains("in progress"));

    let subm = base.join("subm");
    let g = init(&subm);
    commit(&g, "a", "1");
    fs::write(subm.join(".gitmodules"), "").unwrap();
    assert!(refused(migrate(&subm, "m", &bk, "1")).contains("submodule"));

    let multi = base.join("multi");
    let g = init(&multi);
    commit(&g, "a", "1");
    g.run(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "x",
        base.join("extra").to_str().unwrap(),
    ])
    .unwrap();
    assert!(refused(migrate(&multi, "m", &bk, "1")).contains("linked worktrees"));

    let done = base.join("done");
    let g = init(&done);
    commit(&g, "a", "1");
    migrate(&done, "m", &bk, "1").unwrap();
    assert!(refused(migrate(&done, "m", &bk, "2")).contains("already migrated"));

    let inside = base.join("inside");
    let g = init(&inside);
    commit(&g, "a", "1");
    assert!(refused(migrate(&inside, "m", &inside.join("bk"), "1")).contains("outside"));
    assert!(inside.join(".git").is_dir());
    assert!(
        !inside.join("bk").exists(),
        "refusal must not create the backup dir"
    );

    let sparse = base.join("sparse");
    let g = init(&sparse);
    commit(&g, "a", "1");
    g.run(&["config", "core.sparseCheckout", "true"]).unwrap();
    assert!(refused(migrate(&sparse, "m", &bk, "1")).contains("sparse"));
    // Subdirectory is not a repo root.
    fs::create_dir(inside.join("sub")).unwrap();
    assert!(refused(migrate(&inside.join("sub"), "m", &bk, "1")).contains("plain clone"));
}
