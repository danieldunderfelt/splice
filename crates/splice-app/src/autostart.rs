//! "Start at login" via an XDG autostart entry for `splice service`. Works on every
//! desktop and alongside the systemd user unit: a second service start finds the
//! socket in use and exits.

use std::io;
use std::path::PathBuf;

const ENTRY: &str = "io.github.danieldunderfelt.Splice.desktop";

fn path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("autostart").join(ENTRY))
}

pub fn is_enabled() -> bool {
    path().is_some_and(|path| path.exists())
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let path = path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config directory"))?;
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
    }
    let exec = match std::env::var("FLATPAK_ID") {
        Ok(id) => format!("flatpak run --command=splice {id} service"),
        Err(_) => format!("{} service", std::env::current_exe()?.display()),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=Splice\nComment=Splice software KVM service\n\
             Exec={exec}\nIcon=input-mouse\nTerminal=false\nNoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n"
        ),
    )
}
