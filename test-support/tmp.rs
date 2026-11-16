//! Shared RAII temp-dir helper for tests; included with `#[path]` so no extra crate is needed.
//!
//! Creates a unique directory under `std::env::temp_dir()` and removes it recursively on drop,
//! also when the test panics. `/tmp` is tmpfs and has run out of inodes before, so tests must
//! never leave directories behind.
#![allow(dead_code)]

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// `<tmp>/grove-test-<tag>-<pid>-<nanos>-<counter>`, created (with parents).
    pub fn new(tag: &str) -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "grove-test-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Remove `path` recursively, making read-only entries (git objects) writable first.
fn remove_all(path: &Path) {
    if std::fs::remove_dir_all(path).is_ok() {
        return;
    }
    fn chmod(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::symlink_metadata(p) else {
            return;
        };
        if meta.is_dir() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    chmod(&e.path());
                }
            }
        } else if !meta.file_type().is_symlink() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
        }
    }
    chmod(path);
    let _ = std::fs::remove_dir_all(path);
}

impl Drop for TempDir {
    fn drop(&mut self) {
        remove_all(&self.path);
    }
}

impl Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl From<&TempDir> for PathBuf {
    fn from(t: &TempDir) -> PathBuf {
        t.path.clone()
    }
}
