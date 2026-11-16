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

    let locked = base.join("locked");
    let g = init(&locked);
    commit(&g, "a", "1");
    let lw = base.join("locked-wt");
    g.run(&["worktree", "add", "-q", "-b", "x", lw.to_str().unwrap()])
        .unwrap();
    g.run(&["worktree", "lock", lw.to_str().unwrap()]).unwrap();
    assert!(refused(migrate(&locked, "m", &bk, "1")).contains("locked"));
    assert!(locked.join(".git").is_dir(), "refusal touches nothing");

    let prunable = base.join("prunable");
    let g = init(&prunable);
    commit(&g, "a", "1");
    let pw = base.join("prunable-wt");
    g.run(&["worktree", "add", "-q", "-b", "x", pw.to_str().unwrap()])
        .unwrap();
    fs::remove_dir_all(&pw).unwrap();
    assert!(refused(migrate(&prunable, "m", &bk, "1")).contains("missing"));

    let busy = base.join("busy");
    let g = init(&busy);
    commit(&g, "a", "1");
    let bw = base.join("busy-wt");
    g.run(&["worktree", "add", "-q", "-b", "x", bw.to_str().unwrap()])
        .unwrap();
    let meta = fs::read_to_string(bw.join(".git")).unwrap();
    let meta = meta.trim().strip_prefix("gitdir: ").unwrap();
    fs::write(std::path::Path::new(meta).join("MERGE_HEAD"), "x").unwrap();
    let m = refused(migrate(&busy, "m", &bk, "1"));
    assert!(
        m.contains("in progress") && m.contains("linked worktree"),
        "{m}"
    );

    let subw = base.join("subw");
    let g = init(&subw);
    commit(&g, "a", "1");
    let sw = base.join("subw-wt");
    g.run(&["worktree", "add", "-q", "-b", "x", sw.to_str().unwrap()])
        .unwrap();
    fs::write(sw.join(".gitmodules"), "").unwrap();
    assert!(refused(migrate(&subw, "m", &bk, "1")).contains("submodules"));
    let subm2 = base.join("subm2");
    let g = init(&subm2);
    commit(&g, "a", "1");
    let sw2 = base.join("subm2-wt");
    g.run(&["worktree", "add", "-q", "-b", "x", sw2.to_str().unwrap()])
        .unwrap();
    let meta = fs::read_to_string(sw2.join(".git")).unwrap();
    let meta = meta.trim().strip_prefix("gitdir: ").unwrap();
    fs::create_dir_all(std::path::Path::new(meta).join("modules/m")).unwrap();
    assert!(refused(migrate(&subm2, "m", &bk, "1")).contains("submodules"));

    let rel = base.join("rel");
    let g = init(&rel);
    commit(&g, "a", "1");
    g.run(&["config", "worktree.useRelativePaths", "true"])
        .unwrap();
    assert!(refused(migrate(&rel, "m", &bk, "1")).contains("relative"));
    assert!(rel.join(".git").is_dir());

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

/// Observable state of a worktree: HEAD, branch and `status --ignored`.
fn wt_state(dir: &std::path::Path) -> String {
    let g = Git::new(dir);
    format!(
        "{}|{}|{}",
        g.run(&["rev-parse", "HEAD"]).unwrap(),
        g.probe(&["symbolic-ref", "-q", "HEAD"])
            .unwrap()
            .unwrap_or_default(),
        g.run(&["status", "--porcelain", "--ignored"]).unwrap()
    )
}

/// A repo with a detached worktree nested in it and a dirty branch worktree next to it.
fn repo_with_linked(
    base: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let repo = base.join("proj");
    let g = messy_repo(&repo);
    let nested = repo.join(".claude/worktrees/wt1");
    fs::create_dir_all(nested.parent().unwrap()).unwrap();
    g.run(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        nested.to_str().unwrap(),
    ])
    .unwrap();
    fs::write(nested.join("src/a.txt"), "nested edit\n").unwrap();
    fs::write(nested.join("nested-untracked.txt"), "n\n").unwrap();
    fs::write(nested.join("nested.env"), "ignored\n").unwrap();
    let outside = base.join("proj-feature");
    g.run(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feat/out",
        outside.to_str().unwrap(),
    ])
    .unwrap();
    fs::write(outside.join("b.txt"), "outside edit\n").unwrap();
    fs::write(outside.join("new-staged.txt"), "s\n").unwrap();
    Git::new(&outside).run(&["add", "new-staged.txt"]).unwrap();
    fs::write(outside.join("out-untracked.txt"), "u\n").unwrap();
    fs::write(outside.join("out.env"), "ignored\n").unwrap();
    (repo, nested, outside)
}

fn same_links(wt: &std::path::Path, bare: &std::path::Path) {
    let pointer = fs::read_to_string(wt.join(".git")).unwrap();
    let meta = std::path::PathBuf::from(pointer.trim().strip_prefix("gitdir: ").unwrap());
    assert!(meta.starts_with(bare.join("worktrees")), "{pointer}");
    let back = fs::read_to_string(meta.join("gitdir")).unwrap();
    assert_eq!(back.trim(), wt.join(".git").to_str().unwrap());
}

#[test]
fn migrates_with_nested_detached_and_outside_branch_worktrees() {
    let base = tmpdir("linked");
    let (repo, nested, outside) = repo_with_linked(&base);
    let (nested_before, outside_before) = (wt_state(&nested), wt_state(&outside));
    let main_before = wt_state(&repo);
    assert!(nested_before.contains("nested-untracked.txt"));

    let r = migrate(&repo, "m", &base.join("bk"), "1").unwrap();

    let root = repo.canonicalize().unwrap();
    let new_nested = root.join("main/.claude/worktrees/wt1");
    assert_eq!(names(&root), vec![".bare", "main"]);
    assert_eq!(r.linked.len(), 2);
    let find = |p: &std::path::Path| r.linked.iter().find(|l| l.path == p).unwrap();
    assert_eq!(find(&new_nested).branch, None);
    assert_eq!(
        find(&new_nested).old_path,
        root.join(".claude/worktrees/wt1")
    );
    let out_canon = outside.canonicalize().unwrap();
    assert_eq!(find(&out_canon).branch.as_deref(), Some("feat/out"));
    assert_eq!(find(&out_canon).old_path, out_canon);
    assert_eq!(
        r.linked_backups.len(),
        1,
        "outside worktree gets its own tar"
    );
    assert!(r.linked_backups[0].metadata().unwrap().len() > 0);

    // Same observable state in all three worktrees, and links point both ways.
    assert_eq!(wt_state(&r.worktree), main_before);
    assert_eq!(wt_state(&new_nested), nested_before);
    assert_eq!(wt_state(&outside), outside_before);
    same_links(&new_nested, &r.bare_dir);
    same_links(&outside.canonicalize().unwrap(), &r.bare_dir);
    assert!(!root.join(".claude").exists());
    assert_eq!(
        fs::read_to_string(outside.join("b.txt")).unwrap(),
        "outside edit\n"
    );
    assert!(outside.join("out.env").exists());
    assert!(new_nested.join("nested.env").exists());

    let list = Git::new(&r.worktree)
        .run(&["worktree", "list", "--porcelain"])
        .unwrap();
    assert!(list.contains(&format!("worktree {}", new_nested.display())));
    assert!(list.contains(&format!(
        "worktree {}",
        outside.canonicalize().unwrap().display()
    )));
    assert!(!list.contains("prunable"), "{list}");

    // They are still usable: commit on the outside branch, status in the nested one.
    let og = Git::new(&outside);
    og.run(&["commit", "-qam", "more"]).unwrap();
    assert!(Git::new(&new_nested).run(&["status"]).is_ok());
}

#[test]
fn rollback_restores_linked_worktrees_at_every_step() {
    for step in [
        Step::Staged,
        Step::GitMoved,
        Step::WorktreeAdded,
        Step::Filled,
        Step::Relinked,
        Step::IndexMoved,
        Step::Verified,
    ] {
        let base = tmpdir("rblinked");
        let (repo, nested, outside) = repo_with_linked(&base);
        let before = (wt_state(&repo), wt_state(&nested), wt_state(&outside));
        let files = (
            fs::read_to_string(nested.join(".git")).unwrap(),
            fs::read_to_string(outside.join(".git")).unwrap(),
        );
        let names_before = names(&repo);

        let err = migrate_failing_after(&repo, "m", &base.join("bk"), "1", step).unwrap_err();
        let MigrateError::Failed { rolled_back, .. } = err else {
            panic!("{step:?}: expected Failed")
        };
        assert!(rolled_back, "{step:?}");

        assert_eq!(names(&repo), names_before, "{step:?}");
        assert_eq!(
            (wt_state(&repo), wt_state(&nested), wt_state(&outside)),
            before,
            "{step:?}"
        );
        assert_eq!(fs::read_to_string(nested.join(".git")).unwrap(), files.0);
        assert_eq!(fs::read_to_string(outside.join(".git")).unwrap(), files.1);
        let g = Git::new(&repo);
        let list = g.run(&["worktree", "list", "--porcelain"]).unwrap();
        assert_eq!(list.matches("worktree ").count(), 3, "{step:?}: {list}");
        assert!(!list.contains("prunable"), "{step:?}: {list}");
        assert_eq!(
            fs::read_dir(repo.join(".git/worktrees")).unwrap().count(),
            2,
            "{step:?}: only the original worktree metadata remains"
        );
        // And a retry succeeds afterwards.
        migrate(&repo, "m", &base.join("bk"), "2").unwrap();
    }
}
