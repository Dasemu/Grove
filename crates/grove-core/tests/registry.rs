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
