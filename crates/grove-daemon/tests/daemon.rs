use grove_core::config::Config;
use grove_daemon::{Daemon, Settings, ipc};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

struct World {
    base: PathBuf,
    remote: PathBuf,
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("grove-dt-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A bare remote plus one seeded clone per machine under `<base>/<machine>/repo`.
fn world(tag: &str, machines: &[&str]) -> World {
    let base = tmpdir(tag);
    let remote = base.join("remote.git");
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let seed = base.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-qm", "init"]);
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main"]);
    for m in machines {
        let dir = base.join(m);
        fs::create_dir_all(&dir).unwrap();
        bare_layout(&dir.join("g/repo"), &remote);
    }
    World { base, remote }
}

/// A bare + worktrees repo (`.bare`, worktree `main`) cloned from `remote`, as `grove add` leaves it.
fn bare_layout(repo: &Path, remote: &Path) {
    fs::create_dir_all(repo).unwrap();
    git(
        repo,
        &["clone", "-q", "--bare", remote.to_str().unwrap(), ".bare"],
    );
    let bare = repo.join(".bare");
    git(
        &bare,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    git(&bare, &["fetch", "-q", "origin"]);
    git(&bare, &["worktree", "add", "-q", "../main", "main"]);
    git(
        &repo.join("main"),
        &["branch", "-q", "--set-upstream-to=origin/main", "main"],
    );
}

const GROUP: &str = "[providers.p]\nkind = \"gitea\"\nurl = \"https://git.example.invalid\"\n[[groups]]\nname = \"g\"\nprimary = \"p\"\n";

fn config(w: &World, machine: &str, debounce_ms: u64, fetch_s: u64) -> Config {
    Config::parse(&format!(
        "[machine]\nname = \"{machine}\"\n[root]\npath = \"{}\"\n{GROUP}[sync]\nsnapshots = \"all\"\nsnapshot_debounce_ms = {debounce_ms}\nfetch_interval_s = {fetch_s}\n",
        w.base.join(machine).display()
    ))
    .unwrap()
}

fn settings(w: &World, machine: &str) -> Settings {
    Settings {
        socket: w.base.join(format!("{machine}.sock")),
        activity_log: Some(w.base.join(format!("{machine}.jsonl"))),
        remote: "origin".into(),
        visibility_cache: None,
    }
}

fn start(w: &World, machine: &str) -> Daemon {
    Daemon::start(config(w, machine, 200, 2), settings(w, machine)).unwrap()
}

#[test]
fn edit_on_a_shows_up_on_b() {
    let w = world("converge", &["pca", "pcb"]);
    let a = start(&w, "pca");
    let b = start(&w, "pcb");
    let file_a = w.base.join("pca/g/repo/main/new.txt");
    fs::write(&file_a, "hello\n").unwrap();
    fs::write(w.base.join("pca/g/repo/main/a.txt"), "one\nedited\n").unwrap();
    let on_b = w.base.join("pcb/g/repo/main/new.txt");
    wait_until("new.txt on B", 30, || {
        fs::read_to_string(&on_b).is_ok_and(|s| s == "hello\n")
    });
    assert_eq!(
        fs::read_to_string(w.base.join("pcb/g/repo/main/a.txt")).unwrap(),
        "one\nedited\n"
    );
    // The user's branch on A was never touched by the daemon.
    assert_eq!(
        git(&w.base.join("pca/g/repo/main"), &["log", "--oneline"])
            .lines()
            .count(),
        1
    );
    a.shutdown();
    b.shutdown();
}

#[test]
fn committed_work_fast_forwards_the_other_machine() {
    let w = world("ff", &["pca", "pcb"]);
    let a = start(&w, "pca");
    let b = start(&w, "pcb");
    let ra = w.base.join("pca/g/repo/main");
    fs::write(ra.join("c.txt"), "committed\n").unwrap();
    git(&ra, &["add", "-A"]);
    git(&ra, &["commit", "-qm", "c"]);
    git(&ra, &["push", "-q", "origin", "main"]);
    let rb = w.base.join("pcb/g/repo/main");
    wait_until("B fast-forwarded", 30, || rb.join("c.txt").exists());
    wait_until("B synced", 30, || {
        git(&rb, &["status", "--porcelain"]).is_empty()
            && git(&rb, &["log", "--oneline"]).lines().count() == 2
    });
    a.shutdown();
    b.shutdown();
}

#[test]
fn ipc_status_pause_resume_log_and_subscribe() {
    let w = world("ipc", &["pca"]);
    let st = settings(&w, "pca");
    let sock = st.socket.clone();
    let d = Daemon::start(config(&w, "pca", 200, 2), st).unwrap();

    assert_eq!(
        ipc::request(&sock, &json!({"v":1,"cmd":"ping"})).unwrap()["data"],
        "pong"
    );
    let bad = ipc::request(&sock, &json!({"v":99,"cmd":"ping"})).unwrap();
    assert_eq!(bad["ok"], false);
    let unknown = ipc::request(&sock, &json!({"v":1,"cmd":"nope"})).unwrap();
    assert_eq!(unknown["ok"], false);

    let repo = w.base.join("pca/g/repo/main");
    wait_until("status lists the repo", 10, || {
        let s = ipc::request(&sock, &json!({"v":1,"cmd":"status"})).unwrap();
        s["data"]["repos"].as_array().is_some_and(|r| r.len() == 1)
    });

    // While paused, edits must not produce a snapshot.
    assert_eq!(
        ipc::request(&sock, &json!({"v":1,"cmd":"pause"})).unwrap()["data"]["paused"],
        true
    );
    fs::write(repo.join("p.txt"), "paused\n").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        git(&repo, &["for-each-ref", "refs/sync/"]).is_empty(),
        "snapshot while paused"
    );

    // Resume: the pending change is snapshotted; subscribers see it.
    let mut sub = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    use std::io::{BufRead, BufReader, Write};
    writeln!(sub, "{}", json!({"v":1,"cmd":"subscribe"})).unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut rd = BufReader::new(sub);
    let mut line = String::new();
    rd.read_line(&mut line).unwrap();
    assert!(line.contains("subscribed"));
    ipc::request(&sock, &json!({"v":1,"cmd":"resume"})).unwrap();
    let mut saw_snapshot = false;
    for _ in 0..20 {
        line.clear();
        if rd.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.contains("\"kind\":\"snapshot\"") {
            saw_snapshot = true;
            break;
        }
    }
    assert!(saw_snapshot, "no snapshot event streamed");
    assert!(git(&repo, &["for-each-ref", "refs/sync/"]).contains("refs/sync/pca/main"));

    let log = ipc::request(&sock, &json!({"v":1,"cmd":"log","limit":100})).unwrap();
    let kinds: Vec<&str> = log["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"pause") && kinds.contains(&"resume") && kinds.contains(&"snapshot"),
        "{kinds:?}"
    );
    assert!(
        w.base.join("pca.jsonl").exists(),
        "activity log file written"
    );
    d.shutdown();
    assert!(!sock.exists(), "socket removed on shutdown");
    let _ = &w.remote;
}

#[test]
fn second_daemon_on_same_socket_is_refused_and_stale_socket_is_reused() {
    let w = world("sock", &["pca"]);
    let st = settings(&w, "pca");
    let d = Daemon::start(config(&w, "pca", 200, 2), st.clone()).unwrap();
    assert!(Daemon::start(config(&w, "pca", 200, 2), st.clone()).is_err());
    d.shutdown();
    // A leftover socket file with nobody listening must not block startup.
    fs::write(&st.socket, "").unwrap();
    let d = Daemon::start(config(&w, "pca", 200, 2), st).unwrap();
    d.shutdown();
}

#[test]
fn killing_the_daemon_loses_nothing_and_a_restart_recovers() {
    let w = world("crash", &["pca"]);
    let repo = w.base.join("pca/g/repo/main");
    let cfg_path = w.base.join("config.toml");
    fs::write(
        &cfg_path,
        format!(
            "[machine]\nname = \"pca\"\n[root]\npath = \"{}\"\n{GROUP}[sync]\nsnapshots = \"all\"\nsnapshot_debounce_ms = 50\nfetch_interval_s = 1\n",
            w.base.join("pca").display()
        ),
    )
    .unwrap();
    let runtime = w.base.join("run");
    fs::create_dir_all(&runtime).unwrap();
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_groved"))
            .arg("--config")
            .arg(&cfg_path)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", w.base.join("state"))
            .env("GIT_AUTHOR_NAME", "t")
            .spawn()
            .unwrap()
    };
    let mut child = spawn();
    std::thread::sleep(Duration::from_millis(300));
    fs::write(repo.join("wip.txt"), "precious\n").unwrap();
    // Kill at an arbitrary moment: maybe before, during or after a pass.
    std::thread::sleep(Duration::from_millis(400));
    child.kill().unwrap();
    child.wait().unwrap();

    assert_eq!(
        fs::read_to_string(repo.join("wip.txt")).unwrap(),
        "precious\n"
    );
    assert!(
        !repo.join("../.bare/worktrees/main/index.lock").exists(),
        "stale index.lock after kill"
    );
    git(&repo, &["fsck", "--no-dangling"]);

    // Restart over the stale socket: the snapshot is (re)taken and the work is still there.
    let mut child = spawn();
    wait_until("snapshot after restart", 30, || {
        git(&repo, &["for-each-ref", "refs/sync/"]).contains("refs/sync/pca/main")
    });
    assert_eq!(
        fs::read_to_string(repo.join("wip.txt")).unwrap(),
        "precious\n"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn work_in_progress_then_committed_on_a_leaves_b_clean() {
    let w = world("wipcommit", &["pca", "pcb"]);
    let a = start(&w, "pca");
    let b = start(&w, "pcb");
    let ra = w.base.join("pca/g/repo/main");
    let rb = w.base.join("pcb/g/repo/main");
    fs::write(ra.join("w.txt"), "wip\n").unwrap();
    wait_until("WIP on B", 30, || rb.join("w.txt").exists());
    git(&ra, &["add", "-A"]);
    git(&ra, &["commit", "-qm", "w"]);
    git(&ra, &["push", "-q", "origin", "main"]);
    wait_until("B clean at the new commit", 30, || {
        git(&rb, &["status", "--porcelain"]).is_empty()
            && git(&rb, &["log", "--oneline"]).lines().count() == 2
    });
    assert_eq!(fs::read_to_string(rb.join("w.txt")).unwrap(), "wip\n");
    a.shutdown();
    b.shutdown();
}

fn cpu_ticks(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = stat.rsplit_once(')').unwrap().1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap() // utime + stime
}

#[test]
fn paused_daemon_does_not_spin() {
    let w = world("spin", &["pca"]);
    let cfg_path = w.base.join("config.toml");
    fs::write(
        &cfg_path,
        format!(
            "[machine]\nname = \"pca\"\n[root]\npath = \"{}\"\n{GROUP}[sync]\nsnapshots = \"all\"\nsnapshot_debounce_ms = 50\nfetch_interval_s = 1\n",
            w.base.join("pca").display()
        ),
    )
    .unwrap();
    let runtime = w.base.join("run");
    fs::create_dir_all(&runtime).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_groved"))
        .arg("--config")
        .arg(&cfg_path)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", w.base.join("state"))
        .spawn()
        .unwrap();
    let sock = runtime.join("grove/grove.sock");
    wait_until("daemon up", 10, || {
        ipc::request(&sock, &json!({"v":1,"cmd":"ping"})).is_ok()
    });
    ipc::request(&sock, &json!({"v":1,"cmd":"pause"})).unwrap();
    std::thread::sleep(Duration::from_millis(1500)); // let any in-flight pass finish
    let before = cpu_ticks(child.id());
    std::thread::sleep(Duration::from_secs(3)); // well past several fetch intervals
    let used = cpu_ticks(child.id()) - before;
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(used < 30, "paused daemon burned {used} ticks of CPU in 3 s");
}

#[test]
fn sync_accepts_a_non_canonical_repo_path_and_unknown_repos_are_rejected() {
    let w = world("path", &["pca"]);
    let st = settings(&w, "pca");
    let sock = st.socket.clone();
    let d = Daemon::start(config(&w, "pca", 200, 60), st).unwrap();
    let repo = w.base.join("pca/g/repo");
    wait_until("repo listed", 10, || {
        ipc::request(&sock, &json!({"v":1,"cmd":"status"})).unwrap()["data"]["repos"]
            .as_array()
            .is_some_and(|r| r.len() == 1)
    });
    let odd = format!("{}/../repo/.", repo.display());
    let ok = ipc::request(&sock, &json!({"v":1,"cmd":"sync","repo":odd})).unwrap();
    assert_eq!(ok["ok"], true, "{ok}");
    let bad = ipc::request(&sock, &json!({"v":1,"cmd":"sync","repo":"/nonexistent"})).unwrap();
    assert_eq!(bad["ok"], false);
    d.shutdown();
}

fn status(sock: &Path) -> serde_json::Value {
    ipc::request(sock, &json!({"v":1,"cmd":"status"})).unwrap()["data"].clone()
}

#[test]
fn plain_clones_and_non_group_dirs_are_ignored_and_reported() {
    let w = world("scope", &["pca"]);
    let root = w.base.join("pca");
    // A plain clone in the group, and a bare-layout repo in a directory that is no group.
    git(
        &root.join("g"),
        &["clone", "-q", w.remote.to_str().unwrap(), "plain"],
    );
    bare_layout(&root.join("nogroup/repo"), &w.remote);
    // Local work in the plain clone and the non-group repo must stay untouched and unsnapshotted.
    fs::write(root.join("g/plain/wip.txt"), "x\n").unwrap();
    fs::write(root.join("nogroup/repo/main/wip.txt"), "x\n").unwrap();
    let st = settings(&w, "pca");
    let sock = st.socket.clone();
    let d = Daemon::start(config(&w, "pca", 100, 1), st).unwrap();
    wait_until("managed repo listed", 10, || {
        status(&sock)["repos"]
            .as_array()
            .is_some_and(|r| r.len() == 1)
    });
    wait_until("plain clone reported", 10, || {
        status(&sock)["not_migrated"] == json!([root.join("g/plain").display().to_string()])
    });
    std::thread::sleep(Duration::from_secs(3));
    let s = status(&sock);
    assert_eq!(
        s["repos"][0]["path"],
        root.join("g/repo").display().to_string()
    );
    assert!(git(&root.join("g/plain"), &["for-each-ref", "refs/sync/"]).is_empty());
    assert!(
        git(
            &root.join("nogroup/repo/main"),
            &["for-each-ref", "refs/sync/"]
        )
        .is_empty()
    );
    assert!(git(&w.remote, &["for-each-ref", "refs/sync/"]).is_empty());
    d.shutdown();
}

#[test]
fn private_only_withholds_snapshots_when_visibility_is_unknown() {
    let w = world("private", &["pca"]);
    // The provider host does not resolve and the origin is a local path: visibility is unknown.
    let text = format!(
        "[machine]\nname = \"pca\"\n[root]\npath = \"{}\"\n{GROUP}[sync]\nsnapshot_debounce_ms = 100\nfetch_interval_s = 1\n",
        w.base.join("pca").display()
    );
    let cfg = Config::parse(&text).unwrap();
    assert_eq!(
        cfg.sync.snapshots,
        grove_core::config::SnapshotPolicy::PrivateOnly
    );
    let st = settings(&w, "pca");
    let sock = st.socket.clone();
    let d = Daemon::start(cfg, st).unwrap();
    let repo = w.base.join("pca/g/repo/main");
    fs::write(repo.join("secret.txt"), "wip\n").unwrap();
    wait_until("withheld reported", 15, || {
        status(&sock)["repos"][0]["snapshots_withheld"]
            .as_str()
            .is_some_and(|m| m.contains("treated as public"))
    });
    std::thread::sleep(Duration::from_secs(2));
    assert!(git(&repo, &["for-each-ref", "refs/sync/"]).is_empty());
    assert!(git(&w.remote, &["for-each-ref", "refs/sync/"]).is_empty());
    let log = ipc::request(&sock, &json!({"v":1,"cmd":"log","limit":100})).unwrap();
    assert!(log["data"].to_string().contains("snapshots_withheld"));
    d.shutdown();
}

#[test]
fn auto_materialize_is_decided_per_group() {
    let w = world("nomat", &["pca"]);
    let text = format!(
        "[machine]\nname = \"pca\"\n[root]\npath = \"{}\"\n[providers.p]\nkind = \"gitea\"\n[[groups]]\nname = \"g\"\nprimary = \"p\"\n[[groups]]\nname = \"h\"\nprimary = \"p\"\nauto_materialize = false\n[sync]\nsnapshots = \"all\"\nfetch_interval_s = 1\n",
        w.base.join("pca").display()
    );
    let seed = w.base.join("seed");
    git(&seed, &["checkout", "-q", "-b", "feat/new"]);
    git(&seed, &["push", "-q", "origin", "feat/new"]);
    let st = settings(&w, "pca");
    let d = Daemon::start(Config::parse(&text).unwrap(), st).unwrap();
    let bare = w.base.join("pca/g/repo/.bare");
    // Another group turns materializing off; this repo's group keeps the default.
    wait_until("worktree for the new branch", 15, || {
        git(&bare, &["branch", "-r"]).contains("origin/feat/new")
            && w.base.join("pca/g/repo/feat-new").exists()
    });
    d.shutdown();
}

#[test]
fn snapshots_already_pushed_are_deleted_when_they_become_withheld() {
    let w = world("purge", &["pca"]);
    let repo = w.base.join("pca/g/repo/main");
    // This machine pushed a snapshot earlier; another machine has one too.
    git(&repo, &["update-ref", "refs/sync/pca/main", "HEAD"]);
    git(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            "refs/sync/pca/main:refs/sync/pca/main",
        ],
    );
    git(&repo, &["push", "-q", "origin", "HEAD:refs/sync/pcb/main"]);
    let text = format!(
        "[machine]\nname = \"pca\"\n[root]\npath = \"{}\"\n{GROUP}[sync]\nfetch_interval_s = 1\n",
        w.base.join("pca").display()
    );
    let d = Daemon::start(Config::parse(&text).unwrap(), settings(&w, "pca")).unwrap();
    wait_until("own snapshot deleted remotely", 15, || {
        !git(&w.remote, &["for-each-ref", "refs/sync/pca/"]).contains("refs/sync/pca/main")
    });
    d.shutdown();
    let refs = git(&w.remote, &["for-each-ref", "--format=%(refname)"]);
    assert!(refs.contains("refs/sync/pcb/main"), "{refs}");
    assert!(refs.contains("refs/heads/main"), "{refs}");
    assert!(git(&repo, &["for-each-ref", "refs/sync/pca/"]).is_empty());
    let log = fs::read_to_string(w.base.join("pca.jsonl")).unwrap();
    assert!(log.contains("snapshots_purged"), "{log}");
}

/// A daemon whose periodic pass is far away, so only events (not the safety net) can notice changes.
fn start_event_only(w: &World, machine: &str) -> (Daemon, PathBuf) {
    let st = settings(w, machine);
    let sock = st.socket.clone();
    (
        Daemon::start(config(w, machine, 200, 3600), st).unwrap(),
        sock,
    )
}

fn repo_paths(sock: &Path) -> Vec<String> {
    let mut v: Vec<String> = status(sock)["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn repo_migrated_after_start_is_picked_up_and_synced() {
    let w = world("rd-migrate", &["pca"]);
    let root = w.base.join("pca");
    let plain = root.join("g/plain");
    git(
        &root.join("g"),
        &["clone", "-q", w.remote.to_str().unwrap(), "plain"],
    );
    let (d, sock) = start_event_only(&w, "pca");
    wait_until("plain clone reported", 10, || {
        status(&sock)["not_migrated"] == json!([plain.display().to_string()])
    });

    // Migrate in place, as `grove add` does: the clone goes away, the bare layout takes its place.
    fs::remove_dir_all(&plain).unwrap();
    bare_layout(&plain, &w.remote);
    wait_until("migrated repo managed", 10, || {
        repo_paths(&sock).contains(&plain.display().to_string())
            && status(&sock)["not_migrated"] == json!([])
    });
    fs::write(plain.join("main/wip.txt"), "wip\n").unwrap();
    wait_until("snapshot of the new repo on the remote", 30, || {
        git(&w.remote, &["for-each-ref", "refs/sync/pca/"]).contains("refs/sync/pca/main")
    });
    d.shutdown();
}

#[test]
fn repo_cloned_with_bare_layout_after_start_is_picked_up() {
    let w = world("rd-clone", &["pca"]);
    let root = w.base.join("pca");
    let (d, sock) = start_event_only(&w, "pca");
    wait_until("initial repo listed", 10, || repo_paths(&sock).len() == 1);

    // Like `grove import`: the directory appears first, `.bare` a moment later.
    let new = root.join("g/imported");
    fs::create_dir_all(&new).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(repo_paths(&sock).len(), 1);
    bare_layout(&new, &w.remote);
    wait_until("imported repo managed", 10, || {
        repo_paths(&sock).contains(&new.display().to_string())
    });
    fs::write(new.join("main/wip.txt"), "wip\n").unwrap();
    wait_until("snapshot of the imported repo", 30, || {
        git(&w.remote, &["for-each-ref", "refs/sync/pca/"]).contains("refs/sync/pca/main")
    });
    d.shutdown();
}

#[test]
fn managed_repo_removed_after_start_disappears_without_crash() {
    let w = world("rd-remove", &["pca"]);
    let root = w.base.join("pca");
    bare_layout(&root.join("g/second"), &w.remote);
    let (d, sock) = start_event_only(&w, "pca");
    wait_until("both repos listed", 10, || repo_paths(&sock).len() == 2);

    // Keep the first repo busy so a pass may be running when the other one vanishes.
    fs::write(root.join("g/repo/main/busy.txt"), "x\n").unwrap();
    // The daemon may create files in the repo while it is being deleted (ENOTEMPTY): retry.
    wait_until("repo directory removed", 5, || {
        let _ = fs::remove_dir_all(root.join("g/second"));
        !root.join("g/second").exists()
    });
    wait_until("removed repo dropped", 10, || {
        repo_paths(&sock) == vec![root.join("g/repo").display().to_string()]
    });
    // The daemon is alive, the other repo still syncs, and the removal was not logged as an error.
    assert_eq!(
        ipc::request(&sock, &json!({"v":1,"cmd":"ping"})).unwrap()["data"],
        "pong"
    );
    wait_until("remaining repo still synced", 30, || {
        git(&w.remote, &["for-each-ref", "refs/sync/pca/"]).contains("refs/sync/pca/main")
    });
    let log = ipc::request(&sock, &json!({"v":1,"cmd":"log","limit":500})).unwrap();
    let errors: Vec<_> = log["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "error")
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    d.shutdown();
}

#[test]
fn not_migrated_list_follows_clones_added_and_removed() {
    let w = world("rd-notmig", &["pca"]);
    let root = w.base.join("pca");
    let (d, sock) = start_event_only(&w, "pca");
    wait_until("initial repo listed", 10, || repo_paths(&sock).len() == 1);
    assert_eq!(status(&sock)["not_migrated"], json!([]));

    let plain = root.join("g/plain");
    git(
        &root.join("g"),
        &["clone", "-q", w.remote.to_str().unwrap(), "plain"],
    );
    wait_until("new clone reported", 10, || {
        status(&sock)["not_migrated"] == json!([plain.display().to_string()])
    });
    // Deleted (e.g. moved to the trash): it must leave the list again.
    fs::remove_dir_all(&plain).unwrap();
    wait_until("deleted clone dropped", 10, || {
        status(&sock)["not_migrated"] == json!([])
    });
    d.shutdown();
}

fn events(sock: &Path, kind: &str) -> Vec<serde_json::Value> {
    let log = ipc::request(sock, &json!({"v":1,"cmd":"log","limit":500})).unwrap();
    log["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .cloned()
        .collect()
}

#[test]
fn repo_with_busy_marker_is_ignored_until_the_marker_is_removed() {
    let w = world("rd-busy", &["pca"]);
    let root = w.base.join("pca");
    let (d, sock) = start_event_only(&w, "pca");
    wait_until("initial repo listed", 10, || repo_paths(&sock).len() == 1);

    let new = root.join("g/halfway");
    fs::create_dir_all(&new).unwrap();
    fs::write(
        new.join(".grove-busy"),
        format!("{} 1\n", std::process::id()),
    )
    .unwrap();
    bare_layout(&new, &w.remote);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(repo_paths(&sock).len(), 1, "busy repo must not be managed");
    assert!(git(&w.remote, &["for-each-ref", "refs/sync/"]).is_empty());

    fs::remove_file(new.join(".grove-busy")).unwrap();
    wait_until("repo managed once the marker is gone", 10, || {
        repo_paths(&sock).contains(&new.display().to_string())
    });
    d.shutdown();
}

#[test]
fn stale_busy_marker_keeps_the_repo_out_and_warns_once() {
    let w = world("rd-stale", &["pca"]);
    let root = w.base.join("pca");
    let stale = root.join("g/crashed");
    fs::create_dir_all(&stale).unwrap();
    bare_layout(&stale, &w.remote);
    fs::write(stale.join(".grove-busy"), "999999999 1\n").unwrap();
    let (d, sock) = start_event_only(&w, "pca");
    wait_until("warning logged", 10, || {
        !events(&sock, "repo_stalled").is_empty()
    });
    // Another rescan must not repeat the warning, and the marker is never deleted.
    fs::create_dir_all(root.join("g/other")).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(events(&sock, "repo_stalled").len(), 1);
    assert_eq!(repo_paths(&sock).len(), 1);
    assert!(stale.join(".grove-busy").exists());
    assert_eq!(
        status(&sock)["stalled"],
        json!([stale.display().to_string()])
    );
    d.shutdown();
}

#[test]
fn clone_appears_only_when_complete() {
    let w = world("rd-clonecomplete", &["pca"]);
    let root = w.base.join("pca");
    let (d, sock) = start_event_only(&w, "pca");
    wait_until("initial repo listed", 10, || repo_paths(&sock).len() == 1);

    let new = root.join("g/cloned");
    let url = w.remote.display().to_string();
    let target = new.clone();
    let h = std::thread::spawn(move || {
        grove_core::registry::clone_bare_layout(&url, &target, &[]).unwrap()
    });
    let mut seen = false;
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end && !seen {
        if repo_paths(&sock).contains(&new.display().to_string()) {
            // Listed: the clone must be finished, never in the middle.
            assert!(!new.join(".grove-busy").exists());
            assert!(new.join(".bare/HEAD").exists());
            assert!(new.join("main").is_dir());
            seen = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    h.join().unwrap();
    assert!(seen, "cloned repo never managed");
    d.shutdown();
}
