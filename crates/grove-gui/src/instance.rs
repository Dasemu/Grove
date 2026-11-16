//! Single instance: the first process listens on a Unix socket; a second launch asks it to show
//! its window and exits.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `$XDG_RUNTIME_DIR/grove/gui.sock` (same private directory as the daemon socket).
pub fn default_socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("grove").join("gui.sock")
}

pub enum Role {
    /// Another instance is running and has been told to show (or ignore, when `show` is false).
    Secondary,
    /// This process owns the socket.
    Primary(UnixListener),
}

/// Tries to hand over to a running instance; otherwise becomes the primary one.
pub fn acquire(path: &Path, show: bool) -> std::io::Result<Role> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    for _ in 0..3 {
        if notify(path, if show { "show" } else { "ping" }).is_ok() {
            return Ok(Role::Secondary);
        }
        // Nobody answers: a stale socket from a crash is replaced.
        let _ = std::fs::remove_file(path);
        match UnixListener::bind(path) {
            Ok(l) => return Ok(Role::Primary(l)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("cannot acquire the instance socket"))
}

fn notify(path: &Path, msg: &str) -> std::io::Result<()> {
    let mut c = UnixStream::connect(path)?;
    c.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(c, "{msg}")?;
    let mut line = String::new();
    BufReader::new(c).read_line(&mut line)?;
    if line.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other("unexpected answer"))
    }
}

/// Serves the primary socket forever; calls `on_show` for every `show` request.
pub fn serve(listener: UnixListener, on_show: impl Fn() + Send + 'static) {
    for conn in listener.incoming().flatten() {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        let mut conn = BufReader::new(conn);
        if conn.read_line(&mut line).is_err() {
            continue;
        }
        if line.trim() == "show" {
            on_show();
        }
        let _ = writeln!(conn.get_mut(), "ok");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn tmp_sock(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("gui.sock")
    }

    #[test]
    fn second_launch_asks_first_to_show() {
        let p = tmp_sock("show");
        let Role::Primary(l) = acquire(&p, true).unwrap() else {
            panic!("first must be primary");
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || serve(l, move || tx.send(()).unwrap()));
        assert!(matches!(acquire(&p, true).unwrap(), Role::Secondary));
        rx.recv_timeout(Duration::from_secs(2))
            .expect("show callback");
        // A hidden (autostart) relaunch does not pop the window.
        assert!(matches!(acquire(&p, false).unwrap(), Role::Secondary));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn stale_socket_is_replaced() {
        let p = tmp_sock("stale");
        drop(UnixListener::bind(&p).unwrap()); // leaves the socket file behind
        assert!(p.exists());
        assert!(matches!(acquire(&p, true).unwrap(), Role::Primary(_)));
    }
}
