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
        git(&dir, &["clone", "-q", remote.to_str().unwrap(), "repo"]);
    }
    World { base, remote }
}

fn config(w: &World, machine: &str, debounce_ms: u64, fetch_s: u64) -> Config {
    Config::parse(&format!(
        "[machine]\nname = \"{machine}\"\n[root]\npath = \"{}\"\n[sync]\nsnapshot_debounce_ms = {debounce_ms}\nfetch_interval_s = {fetch_s}\n",
        w.base.join(machine).display()
    ))
    .unwrap()
}

fn settings(w: &World, machine: &str) -> Settings {
    Settings {
        socket: w.base.join(format!("{machine}.sock")),
        activity_log: Some(w.base.join(format!("{machine}.jsonl"))),
        remote: "origin".into(),
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
    let file_a = w.base.join("pca/repo/new.txt");
    fs::write(&file_a, "hello\n").unwrap();
    fs::write(w.base.join("pca/repo/a.txt"), "one\nedited\n").unwrap();
    let on_b = w.base.join("pcb/repo/new.txt");
    wait_until("new.txt on B", 30, || {
        fs::read_to_string(&on_b).is_ok_and(|s| s == "hello\n")
    });
    assert_eq!(
        fs::read_to_string(w.base.join("pcb/repo/a.txt")).unwrap(),
        "one\nedited\n"
    );
    // The user's branch on A was never touched by the daemon.
    assert_eq!(
        git(&w.base.join("pca/repo"), &["log", "--oneline"])
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
    let ra = w.base.join("pca/repo");
    fs::write(ra.join("c.txt"), "committed\n").unwrap();
    git(&ra, &["add", "-A"]);
    git(&ra, &["commit", "-qm", "c"]);
    git(&ra, &["push", "-q", "origin", "main"]);
    let rb = w.base.join("pcb/repo");
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

    let repo = w.base.join("pca/repo");
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
    let repo = w.base.join("pca/repo");
    let cfg_path = w.base.join("config.toml");
    fs::write(
        &cfg_path,
        format!(
            "[machine]\nname = \"pca\"\n[root]\npath = \"{}\"\n[sync]\nsnapshot_debounce_ms = 50\nfetch_interval_s = 1\n",
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
        !repo.join(".git/index.lock").exists(),
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
    let ra = w.base.join("pca/repo");
    let rb = w.base.join("pcb/repo");
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
            "[machine]\nname = \"pca\"\n[root]\npath = \"{}\"\n[sync]\nsnapshot_debounce_ms = 50\nfetch_interval_s = 1\n",
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
    let repo = w.base.join("pca/repo");
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
