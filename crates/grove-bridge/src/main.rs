//! Localhost dev server for the GUI frontend: static files plus the `Bridge` calls over HTTP.
//! Binds to 127.0.0.1 only. POSTs need the `X-Grove: 1` header (not sendable cross-origin
//! without a preflight, which is never answered); the Host must be exactly 127.0.0.1:<port> or
//! localhost:<port> and any Origin must match too (DNS rebinding).

use anyhow::{Context, Result};
use grove_bridge::Bridge;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == name)
        .and_then(|i| a.get(i + 1))
        .cloned()
}

fn main() -> Result<()> {
    let ui = PathBuf::from(arg("--ui").unwrap_or_else(|| "gui/ui".into()));
    let port: u16 = arg("--port")
        .map_or(Ok(4780), |p| p.parse())
        .context("bad --port")?;
    let bridge = Arc::new(Bridge {
        grove_bin: arg("--grove-bin").map_or_else(|| "grove".into(), PathBuf::from),
        config: arg("--config").map(PathBuf::from),
        socket: arg("--socket")
            .map(PathBuf::from)
            .unwrap_or_else(grove_daemon::ipc::default_socket_path),
    });
    let l = TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("port {port}"))?;
    eprintln!(
        "grove GUI dev server on http://127.0.0.1:{port}/ (ui: {})",
        ui.display()
    );
    for s in l.incoming().filter_map(Result::ok) {
        let (b, ui) = (bridge.clone(), ui.clone());
        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        std::thread::spawn(move || {
            let _ = handle(s, &b, &ui, port);
        });
    }
    Ok(())
}

fn respond(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body)
}

fn handle(mut s: TcpStream, b: &Bridge, ui: &std::path::Path, port: u16) -> std::io::Result<()> {
    // Cap what a client can make us buffer while reading the request head.
    let mut r = BufReader::new(s.try_clone()?.take(64 * 1024));
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut it = line.split_whitespace();
    let (method, path) = (
        it.next().unwrap_or("").to_string(),
        it.next().unwrap_or("/").to_string(),
    );
    let (mut len, mut host, mut marker, mut origin) = (0usize, String::new(), false, None);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = lower.strip_prefix("host:") {
            host = v.trim().to_string();
        } else if let Some(v) = lower.strip_prefix("origin:") {
            origin = Some(v.trim().to_string());
        } else if lower.starts_with("x-grove:") {
            marker = true;
        }
    }
    // Exact match: a prefix check would accept `localhost.evil.com` (DNS rebinding).
    let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !local.contains(&host) {
        return respond(&mut s, "403 Forbidden", "text/plain", b"bad host");
    }
    if let Some(o) = &origin
        && !local.iter().any(|h| *o == format!("http://{h}"))
    {
        return respond(&mut s, "403 Forbidden", "text/plain", b"bad origin");
    }
    if method == "POST" {
        if !marker || len > 1 << 20 {
            return respond(&mut s, "403 Forbidden", "text/plain", b"forbidden");
        }
        r.get_mut().set_limit(1 << 20); // head parsed; allow the (capped) body
        let mut body = vec![0u8; len];
        r.read_exact(&mut body)?;
        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let out = match path.as_str() {
            "/api/ipc" => b.ipc(&req),
            "/api/cli" => {
                let args: Vec<String> = req["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                b.cli(&args, req["stdin"].as_str().unwrap_or(""))
            }
            _ => return respond(&mut s, "404 Not Found", "text/plain", b"not found"),
        };
        return respond(
            &mut s,
            "200 OK",
            "application/json",
            out.to_string().as_bytes(),
        );
    }
    if path == "/api/config" {
        let body = match b.config_json() {
            Some(c) => json!({"exists": true, "config": c}),
            None => json!({"exists": false}),
        };
        return respond(
            &mut s,
            "200 OK",
            "application/json",
            body.to_string().as_bytes(),
        );
    }
    let name = match path.split('?').next().unwrap_or("/") {
        "/" => "index.html",
        p => p.trim_start_matches('/'),
    };
    if name.contains("..") {
        return respond(&mut s, "403 Forbidden", "text/plain", b"forbidden");
    }
    let ctype = match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    match std::fs::read(ui.join(name)) {
        Ok(data) => respond(&mut s, "200 OK", ctype, &data),
        Err(_) => respond(&mut s, "404 Not Found", "text/plain", b"not found"),
    }
}
