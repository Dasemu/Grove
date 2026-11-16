mod common;
use common::*;
use grove_core::git::Git;
use grove_core::registry::*;
use std::fs;

#[test]
fn new_repo_pushes_and_a_fresh_machine_clones_it_into_the_bare_layout() {
    let base = tmpdir("reg");
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
    let url = remote.to_str().unwrap();

    let a = base.join("A/g/proj");
    let wt = init_bare_layout(&a, url).unwrap();
    assert!(wt.ends_with("main") && wt.join(".git").exists());
    push_initial(&a, &[]).unwrap();
    let g = Git::new(&wt);
    assert_eq!(g.run(&["status", "--porcelain"]).unwrap(), "");
    assert_eq!(
        g.run(&["rev-parse", "--abbrev-ref", "@{upstream}"])
            .unwrap(),
        "origin/main"
    );

    let b = base.join("B/g/proj");
    let made = clone_bare_layout(url, &b, &[]).unwrap();
    assert_eq!(made.len(), 1);
    assert_eq!(
        Git::new(b.join("main"))
            .run(&["rev-parse", "--abbrev-ref", "@{upstream}"])
            .unwrap(),
        "origin/main",
        "cloned worktrees must track their remote branch"
    );
    assert!(b.join("main/.git").exists() && b.join(".bare").is_dir());
    assert_eq!(
        Git::new(b.join("main"))
            .run(&["rev-parse", "HEAD"])
            .unwrap(),
        g.run(&["rev-parse", "HEAD"]).unwrap()
    );
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn registry_roundtrip_and_upsert() {
    let mut r = Registry::default();
    r.upsert(Entry {
        group: "z".into(),
        name: "b".into(),
        url: "u1".into(),
    });
    r.upsert(Entry {
        group: "a".into(),
        name: "x".into(),
        url: "u2".into(),
    });
    r.upsert(Entry {
        group: "z".into(),
        name: "b".into(),
        url: "u3".into(),
    });
    assert_eq!(r.repos.len(), 2);
    assert_eq!(r.repos[0].group, "a");
    assert_eq!(Registry::parse(&r.to_text()).unwrap(), r);
}

fn remote_with_main(base: &std::path::Path) -> String {
    let remote = base.join("r.git");
    Git::new(base)
        .run(&[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ])
        .unwrap();
    let a = base.join("A/g/proj");
    init_bare_layout(&a, remote.to_str().unwrap()).unwrap();
    push_initial(&a, &[]).unwrap();
    remote.to_str().unwrap().to_string()
}

#[test]
fn clone_failing_after_the_clone_step_removes_what_it_created() {
    let base = tmpdir("regfail");
    let url = remote_with_main(&base);

    // A directory the clone creates itself: gone afterwards, no marker left.
    let fresh = base.join("B/g/fresh");
    assert!(clone_bare_layout_failing_after_clone(&url, &fresh, &[]).is_err());
    assert!(!fresh.exists());

    // A pre-existing empty directory stays, empty and without markers.
    let empty = base.join("B/g/empty");
    fs::create_dir_all(&empty).unwrap();
    assert!(clone_bare_layout_failing_after_clone(&url, &empty, &[]).is_err());
    assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);

    // The retry works: nothing half-built is in the way.
    clone_bare_layout(&url, &fresh, &[]).unwrap();
    assert!(fresh.join(".bare").is_dir() && !fresh.join(".grove-busy").exists());
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn clone_refuses_a_non_empty_directory_and_touches_nothing() {
    let base = tmpdir("regkeep");
    let url = remote_with_main(&base);
    let plain = base.join("B/g/plain");
    fs::create_dir_all(plain.join(".git")).unwrap();
    fs::write(plain.join("keep.txt"), "mine").unwrap();
    let e = clone_bare_layout(&url, &plain, &[]).unwrap_err();
    assert!(e.to_string().contains("not empty"), "{e}");
    assert_eq!(fs::read_to_string(plain.join("keep.txt")).unwrap(), "mine");
    assert!(plain.join(".git").is_dir() && !plain.join(".bare").exists());
    assert!(!plain.join(".grove-busy").exists());
    fs::remove_dir_all(&base).unwrap();
}
