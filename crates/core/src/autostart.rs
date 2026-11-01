//! "Start at login": `~/.config/autostart/sls.desktop`, which GNOME (and
//! every XDG desktop) runs at login.  It starts sls without a window.

use std::fs;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "sls.desktop";

pub fn default_path() -> PathBuf {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"),
    };
    config.join("autostart").join(FILE_NAME)
}

/// The desktop entry; `config_dir` is passed on when it is not the default.
pub fn entry(exec: &str, config_dir: Option<&Path>) -> String {
    let mut cmd = format!("{exec} --background");
    if let Some(dir) = config_dir {
        cmd.push_str(&format!(" --config-dir \"{}\"", dir.display()));
    }
    format!(
        "[Desktop Entry]\nType=Application\nName=sls\n\
         Comment=Share folders on the local network (started in the background)\n\
         Exec={cmd}\nIcon=folder-remote\nTerminal=false\nNoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

pub fn is_enabled(path: &Path) -> bool {
    fs::read_to_string(path)
        .is_ok_and(|t| !t.contains("X-GNOME-Autostart-enabled=false") && !t.contains("Hidden=true"))
}

pub fn set_enabled(
    path: &Path,
    on: bool,
    exec: &str,
    config_dir: Option<&Path>,
) -> std::io::Result<()> {
    if on {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, entry(exec, config_dir))
    } else {
        match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_disable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("autostart/sls.desktop");
        assert!(!is_enabled(&path));
        set_enabled(&path, true, "sls", None).unwrap();
        assert!(is_enabled(&path));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\nExec=sls --background\n"), "{text}");
        set_enabled(&path, true, "/usr/bin/sls", Some(Path::new("/tmp/c d"))).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("Exec=/usr/bin/sls --background --config-dir \"/tmp/c d\"")
        );
        set_enabled(&path, false, "sls", None).unwrap();
        assert!(!is_enabled(&path));
        set_enabled(&path, false, "sls", None).unwrap();

        // Turned off in GNOME Tweaks / Startup Applications.
        fs::write(&path, "[Desktop Entry]\nX-GNOME-Autostart-enabled=false\n").unwrap();
        assert!(!is_enabled(&path));
    }
}
