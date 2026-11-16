//! A small log file (`<state dir>/grove-gui.log`) for things that would otherwise vanish when the
//! GUI runs from autostart with stdout/stderr closed: panics, fatal errors and why it exited.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
/// Set when the process leaves on purpose (tray "Quit", window closed without a tray).
pub static EXPECTED_EXIT: AtomicBool = AtomicBool::new(false);

const MAX_BYTES: u64 = 256 * 1024;

/// Opens the log (truncating a file past 256 KiB) and installs the panic hook.
pub fn init() {
    let Some(dir) = grove_core::config::Config::state_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("grove-gui.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, dir.join("grove-gui.log.old"));
    }
    let _ = PATH.set(path);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let t = std::thread::current();
        let bt = std::backtrace::Backtrace::force_capture();
        line(&format!(
            "PANIC in thread {:?}: {info}\n{bt}",
            t.name().unwrap_or("<unnamed>")
        ));
        default(info);
    }));
}

/// Appends one timestamped line; never fails and never panics.
pub fn line(msg: &str) {
    let Some(p) = PATH.get() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
    {
        let _ = writeln!(f, "{} [{}] {msg}", stamp(), std::process::id());
    }
}

pub fn expected_exit() {
    EXPECTED_EXIT.store(true, Ordering::SeqCst);
}

/// `YYYY-MM-DD HH:MM:SSZ` (UTC) from the system clock.
fn stamp() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (s / 86400, s % 86400);
    // Civil-from-days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
