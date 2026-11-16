//! Local IPC: a Unix socket speaking line-delimited JSON, protocol version 1.
//!
//! Request:  `{"v":1,"cmd":"status"|"ping"|"pause"|"resume"|"sync"|"log"|"subscribe", ...}`
//! Response: `{"v":1,"ok":true,"data":...}` or `{"v":1,"ok":false,"error":"..."}`.
//! After `subscribe` the server keeps the connection open and streams `{"v":1,"event":{...}}`.

use crate::activity::now_secs;
use crate::daemon::{Ctx, Msg};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

pub const PROTOCOL: u32 = 1;

pub struct Server {
    pub socket: PathBuf,
    pub handle: JoinHandle<()>,
}

/// `$XDG_RUNTIME_DIR/grove/grove.sock`, falling back to the state dir.
pub fn default_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(grove_core::config::Config::state_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("grove").join("grove.sock")
}

#[derive(Deserialize)]
struct Request {
    v: u32,
    cmd: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub fn serve(socket: &Path, ctx: Ctx, stop: Arc<AtomicBool>) -> std::io::Result<Server> {
    if let Some(dir) = socket.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // The mode above only applies to a directory we create: enforce it on an existing one.
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    }
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "another daemon is already listening on {}",
                    socket.display()
                ),
            ));
        }
        std::fs::remove_file(socket)?; // stale socket from a crashed daemon
    }
    let listener = UnixListener::bind(socket)?;
    listener.set_nonblocking(true)?;
    let handle = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((conn, _)) => {
                    let ctx = ctx.clone();
                    let stop = stop.clone();
                    std::thread::spawn(move || connection(conn, ctx, stop));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    });
    Ok(Server {
        socket: socket.to_path_buf(),
        handle,
    })
}

fn reply(conn: &mut UnixStream, v: Value) -> bool {
    writeln!(conn, "{v}").is_ok()
}

fn err(msg: &str) -> Value {
    json!({"v": PROTOCOL, "ok": false, "error": msg})
}

fn ok(data: Value) -> Value {
    json!({"v": PROTOCOL, "ok": true, "data": data})
}

fn connection(mut conn: UnixStream, ctx: Ctx, stop: Arc<AtomicBool>) {
    let _ = conn.set_nonblocking(false);
    let Ok(read_half) = conn.try_clone() else {
        return;
    };
    for line in BufReader::new(read_half).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                if !reply(&mut conn, err(&format!("bad request: {e}"))) {
                    return;
                }
                continue;
            }
        };
        if req.v != PROTOCOL {
            let msg = format!(
                "unsupported protocol version {} (server speaks {PROTOCOL})",
                req.v
            );
            if !reply(&mut conn, err(&msg)) {
                return;
            }
            continue;
        }
        if req.cmd == "subscribe" {
            let rx = ctx.activity.subscribe();
            if !reply(&mut conn, ok(json!("subscribed"))) {
                return;
            }
            while !stop.load(Ordering::SeqCst) {
                if let Ok(ev) = rx.recv_timeout(Duration::from_secs(1))
                    && !reply(&mut conn, json!({"v": PROTOCOL, "event": ev}))
                {
                    return;
                }
            }
            return;
        }
        if !reply(&mut conn, handle(&req, &ctx)) {
            return;
        }
    }
}

fn handle(req: &Request, ctx: &Ctx) -> Value {
    match req.cmd.as_str() {
        "ping" => ok(json!("pong")),
        "status" => {
            let s = ctx.shared.lock().unwrap();
            let repos: Vec<Value> = s
                .repos
                .iter()
                .map(|(p, r)| {
                    json!({
                        "path": p.display().to_string(),
                        "phase": r.phase,
                        "last_sync": r.last_sync,
                        "last_error": r.last_error,
                        "failures": r.failures,
                        "worktrees": r.worktrees.iter()
                            .map(|w| json!({"branch": w.branch, "state": w.state, "path": w.path}))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();
            ok(
                json!({"machine": ctx.machine, "paused": s.paused, "now": now_secs(), "repos": repos}),
            )
        }
        "pause" | "resume" => {
            let paused = req.cmd == "pause";
            ctx.shared.lock().unwrap().paused = paused;
            ctx.activity.log(
                &req.cmd,
                None,
                if paused {
                    "sync paused"
                } else {
                    "sync resumed"
                },
            );
            if !paused {
                let _ = ctx.tx.send(Msg::SyncNow(None));
            }
            ok(json!({"paused": paused}))
        }
        "sync" => {
            let target = req.repo.as_ref().map(PathBuf::from);
            // Match by canonical path so `.`, relative and symlinked paths work.
            let target = match target {
                None => None,
                Some(t) => {
                    let want = std::fs::canonicalize(&t).unwrap_or(t);
                    let shared = ctx.shared.lock().unwrap();
                    let found = shared.repos.keys().find(|k| {
                        std::fs::canonicalize(k).unwrap_or_else(|_| (*k).clone()) == want
                    });
                    match found {
                        Some(k) => Some(k.clone()),
                        None => return err("unknown repo"),
                    }
                }
            };
            let _ = ctx.tx.send(Msg::SyncNow(target));
            ok(json!("queued"))
        }
        "log" => ok(json!(ctx.activity.recent(req.limit.unwrap_or(50)))),
        other => err(&format!("unknown command {other:?}")),
    }
}

/// Send one request and read one response. For use by the CLI and tests.
pub fn request(socket: &Path, req: &Value) -> std::io::Result<Value> {
    let mut conn = UnixStream::connect(socket)?;
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    writeln!(conn, "{req}")?;
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
