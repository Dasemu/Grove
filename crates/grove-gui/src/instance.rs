//! Single instance: the first process listens on a Unix socket; a second launch asks it to show
//! its window and exits.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `$XDG_RUNTIME_DIR/grove/gui.sock` (same private directory as the daemon socket); without a
/// runtime dir, a private per-user directory under the temp dir.
pub fn default_socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(base) => PathBuf::from(base).join("grove").join("gui.sock"),
        None => std::env::temp_dir()
            .join(format!("grove-gui-{}", own_uid()))
            .join("gui.sock"),
    }
}

fn own_uid() -> u32 {
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

/// Creates `dir` with mode 0700, or checks that an existing one is ours and not accessible to
/// others (a pre-created directory in a shared /tmp must not be trusted).
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = std::fs::symlink_metadata(dir)?;
    if !m.is_dir() || m.uid() != own_uid() || m.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!(
            "{} is not a private directory owned by you",
            dir.display()
        )));
    }
    Ok(())
}

pub enum Role {
    /// Another instance is running and has been told to show (or ignore, when `show` is false).
    Secondary,
    /// This process owns the socket.
    Primary(UnixListener),
}

/// Tries to hand over to a running instance; otherwise becomes the primary one.
pub fn acquire(path: &Path, show: bool) -> std::io::Result<Role> {
    acquire_with(path, show, Duration::from_secs(2))
}

pub fn acquire_with(path: &Path, show: bool, timeout: Duration) -> std::io::Result<Role> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    for _ in 0..3 {
        match notify(path, if show { "show" } else { "ping" }, timeout) {
            Ok(()) => return Ok(Role::Secondary),
            // Nobody listens: a stale socket file from a crash is replaced.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                if e.kind() == std::io::ErrorKind::ConnectionRefused {
                    let _ = std::fs::remove_file(path);
                }
            }
            // A live instance that does not answer in time is never evicted.
            Err(e) => return Err(e),
        }
        match UnixListener::bind(path) {
            Ok(l) => return Ok(Role::Primary(l)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("cannot acquire the instance socket"))
}

fn notify(path: &Path, msg: &str, timeout: Duration) -> std::io::Result<()> {
    let mut c = UnixStream::connect(path)?;
    c.set_read_timeout(Some(timeout))?;
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
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    fn tmp_sock(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d); // leftovers of an earlier run with the same pid
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
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
    fn unresponsive_instance_is_not_evicted() {
        let p = tmp_sock("busy");
        let l = UnixListener::bind(&p).unwrap();
        // Accepts but never answers (a primary stuck in startup).
        let t = std::thread::spawn(move || {
            let c = l.accept().unwrap();
            std::thread::sleep(Duration::from_millis(600));
            drop(c);
        });
        let r = acquire_with(&p, true, Duration::from_millis(200));
        assert!(r.is_err(), "must not become primary");
        assert!(p.exists(), "socket file must stay");
        t.join().unwrap();
    }

    #[test]
    fn private_dir_is_enforced() {
        let base = std::env::temp_dir().join(format!("gg-priv-{}", std::process::id()));
        let d = base.join("x");
        ensure_private_dir(&d).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().mode() & 0o777, 0o700);
        ensure_private_dir(&d).unwrap(); // idempotent
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&d).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stale_socket_is_replaced() {
        let p = tmp_sock("stale");
        drop(UnixListener::bind(&p).unwrap()); // leaves the socket file behind
        assert!(p.exists());
        assert!(matches!(acquire(&p, true).unwrap(), Role::Primary(_)));
    }
}
