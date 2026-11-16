//! Own test binary: the limit is process-wide.
mod common;
use common::*;
use grove_core::git::Git;
use grove_core::snapshot::{Outcome, set_max_file_bytes, take};
use std::fs;

#[test]
fn oversized_changes_are_left_out_but_small_ones_and_head_content_stay() {
    let base = tmpdir("size");
    let g = init(&base.join("p"));
    commit(&g, "tracked.bin", "old\n");
    set_max_file_bytes(1000);
    let p = base.join("p");
    fs::write(p.join("small.txt"), "ok\n").unwrap();
    fs::write(p.join("big.bin"), vec![b'x'; 5000]).unwrap(); // untracked, too big
    fs::write(p.join("tracked.bin"), vec![b'y'; 5000]).unwrap(); // tracked, modified, too big
    let Outcome::Created(s) = take(&p, "A", "t").unwrap() else {
        panic!("expected a snapshot")
    };
    let mut skipped = s.skipped.clone();
    skipped.sort();
    assert_eq!(skipped, ["big.bin", "tracked.bin"]);
    let show = |f: &str| Git::new(&p).run(&["show", &format!("{}:{f}", s.commit)]);
    assert_eq!(show("small.txt").unwrap(), "ok");
    assert!(
        show("big.bin").is_err(),
        "big untracked file must not be in the snapshot"
    );
    assert_eq!(
        show("tracked.bin").unwrap(),
        "old",
        "tracked file keeps its HEAD content"
    );
    // The user's real index and files are untouched.
    assert_eq!(fs::read(p.join("big.bin")).unwrap().len(), 5000);
    assert!(
        Git::new(&p)
            .run(&["status", "--porcelain"])
            .unwrap()
            .contains("big.bin")
    );
}
