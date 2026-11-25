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

/// The autostart file, its `Exec` pointing at `exe` (this binary) by absolute path: desktop
/// sessions often lack `~/.local/bin` (where `cargo install --root ~/.local` puts it) in `PATH`,
/// and then a bare `grove-gui` never starts. Without a usable path the bare name is kept.
fn desktop_text(exe: Option<&Path>) -> String {
    let Some(exe) = exe.filter(|e| e.is_absolute()).and_then(|e| e.to_str()) else {
        return DESKTOP.to_string();
    };
    // Desktop Entry spec: quote arguments with reserved characters, escaping `"`, `` ` ``, `$`, `\`.
    let exe = if exe.contains(|c: char| " \t\n\"'\\><~|&;$*?#()`".contains(c)) {
        let esc: String = exe
            .chars()
            .flat_map(|c| match c {
                '"' | '`' | '$' | '\\' => vec!['\\', c],
                c => vec![c],
            })
            .collect();
        format!("\"{esc}\"")
    } else {
        exe.to_string()
    };
    DESKTOP.replace("Exec=grove-gui", &format!("Exec={exe}"))
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
        let exe = std::env::current_exe().ok();
        std::fs::write(&f, desktop_text(exe.as_deref()))
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
        let _tmp = crate::test_tmp::TempDir::new("gg-auto");
        let d = _tmp.path().to_path_buf();
        assert!(!is_enabled(&d));
        set(&d, true).unwrap();
        assert!(is_enabled(&d));
        let text = std::fs::read_to_string(file(&d)).unwrap();
        let exe = std::env::current_exe().unwrap();
        assert!(
            text.contains(&format!("Exec={} --hidden", exe.display())) || text.contains("Exec=\"")
        );
        set(&d, false).unwrap();
        assert!(!is_enabled(&d));
        set(&d, false).unwrap(); // already off: fine
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn exec_uses_the_absolute_binary_path_quoted_when_needed() {
        let t = desktop_text(Some(Path::new("/home/u/.local/bin/grove-gui")));
        assert!(
            t.contains("Exec=/home/u/.local/bin/grove-gui --hidden"),
            "{t}"
        );
        let t = desktop_text(Some(Path::new("/opt/my apps/grove-gui")));
        assert!(
            t.contains("Exec=\"/opt/my apps/grove-gui\" --hidden"),
            "{t}"
        );
        let t = desktop_text(None);
        assert!(t.contains("Exec=grove-gui --hidden"), "{t}");
    }
}
