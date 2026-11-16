//! The "busy" marker: `<repo>/.grove-busy` exists while grove restructures a repo (`grove add`,
//! clone/import, init). The daemon treats such a repo as not ready and leaves it alone until the
//! marker is gone, so it never fetches, materializes or snapshots a half-built layout.
//!
//! The marker holds `<pid> <start unix seconds>`. A marker whose process is gone is *stale*: the
//! operation was interrupted, the repo may be half-migrated, and nothing removes the marker
//! automatically.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const BUSY: &str = ".grove-busy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyState {
    Free,
    /// The marker's process is still running.
    Active,
    /// The marker's process is gone: an interrupted operation.
    Stale,
}

/// Holds the marker; removes it on drop, so also on error paths and rollback.
#[derive(Debug)]
pub struct BusyGuard(PathBuf);

impl BusyGuard {
    /// Creates `<dir>/.grove-busy` (and `dir` if needed). Fails if the marker already exists.
    pub fn acquire(dir: &Path) -> io::Result<BusyGuard> {
        fs::create_dir_all(dir)?;
        let path = dir.join(BUSY);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        io::Write::write_all(&mut f, format!("{} {now}\n", std::process::id()).as_bytes())?;
        Ok(BusyGuard(path))
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// State of the marker in `dir`.
pub fn busy_state(dir: &Path) -> BusyState {
    let Ok(text) = fs::read_to_string(dir.join(BUSY)) else {
        return if dir.join(BUSY).exists() {
            BusyState::Stale
        } else {
            BusyState::Free
        };
    };
    match text
        .split_whitespace()
        .next()
        .and_then(|p| p.parse::<u32>().ok())
    {
        Some(pid) if Path::new(&format!("/proc/{pid}")).exists() => BusyState::Active,
        _ => BusyState::Stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_lifecycle() {
        let tmp = crate::test_tmp::TempDir::new("busy");
        let d = tmp.path().join("d");
        assert_eq!(busy_state(&d), BusyState::Free);
        let g = BusyGuard::acquire(&d).unwrap();
        assert_eq!(busy_state(&d), BusyState::Active);
        assert!(BusyGuard::acquire(&d).is_err());
        drop(g);
        assert_eq!(busy_state(&d), BusyState::Free);
        fs::write(d.join(BUSY), "999999999 1\n").unwrap();
        assert_eq!(busy_state(&d), BusyState::Stale);
        fs::remove_dir_all(&d).unwrap();
    }
}
