use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("grove-cli-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn gs(cfg: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grove"))
        .current_dir(dir)
        .arg("--config")
        .arg(cfg)
        .args(args)
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn add_then_sync_then_doctor() {
    let base = tmp("e2e");
    let cfg = base.join("config.toml");
    fs::write(
        &cfg,
        format!(
            "[machine]\nname=\"desk\"\n[root]\npath=\"{}\"\n",
            base.display()
        ),
    )
    .unwrap();

    let remote = base.join("r.git");
    git(
        &base,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    let repo = base.join("proj");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "t"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    fs::write(repo.join("a.txt"), "1\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    git(&repo, &["push", "-q", "-u", "origin", "main"]);
    fs::write(repo.join("wip.txt"), "wip\n").unwrap();

    let o = gs(
        &cfg,
        &base,
        &[
            "add",
            repo.to_str().unwrap(),
            "--backup-dir",
            base.join("bk").to_str().unwrap(),
        ],
    );
    assert!(o.status.success(), "{}", out(&o));
    assert!(repo.join("main/wip.txt").exists());
    assert!(repo.join(".bare").is_dir());

    let o = gs(&cfg, &base, &["sync", repo.to_str().unwrap()]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("snapshot created"), "{}", out(&o));
    assert!(git(&remote, &["for-each-ref", "refs/sync"]).contains("refs/sync/desk/main"));

    let o = gs(&cfg, &base, &["status", repo.to_str().unwrap()]);
    assert!(out(&o).contains("Dirty"), "{}", out(&o));

    let o = gs(&cfg, &base, &["doctor"]);
    assert!(o.status.success(), "{}", out(&o));

    // Bad config is a clear failure, not a panic.
    fs::write(&cfg, "[machine]\nname=\"a b\"\n[root]\npath=\"/x\"\n").unwrap();
    let o = gs(&cfg, &base, &["doctor"]);
    assert!(!o.status.success());
    assert!(out(&o).contains("machine.name"), "{}", out(&o));
}
