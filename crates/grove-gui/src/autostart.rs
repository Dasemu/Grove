//! "Start at login": a copy of the autostart desktop file in `~/.config/autostart`.

use std::path::{Path, PathBuf};

const DESKTOP: &str = include_str!("../../../contrib/grove-autostart.desktop");

/// `$XDG_CONFIG_HOME` or `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

pub fn file(config_home: &Path) -> PathBuf {
    config_home.join("autostart").join("grove.desktop")
}

pub fn is_enabled(config_home: &Path) -> bool {
    file(config_home).exists()
}

pub fn set(config_home: &Path, on: bool) -> std::io::Result<()> {
    let f = file(config_home);
    if on {
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&f, DESKTOP)
    } else {
        match std::fs::remove_file(&f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_writes_and_removes_the_desktop_file() {
        let d = std::env::temp_dir().join(format!("gg-auto-{}", std::process::id()));
        assert!(!is_enabled(&d));
        set(&d, true).unwrap();
        assert!(is_enabled(&d));
        let text = std::fs::read_to_string(file(&d)).unwrap();
        assert!(text.contains("Exec=grove-gui --hidden"));
        set(&d, false).unwrap();
        assert!(!is_enabled(&d));
        set(&d, false).unwrap(); // already off: fine
        let _ = std::fs::remove_dir_all(&d);
    }
}
