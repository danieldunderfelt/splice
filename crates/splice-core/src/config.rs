//! Persistent configuration: settings + last-known layout. Atomic writes (tmp + rename).

use serde::{Deserialize, Serialize};
use splice_platform::BackendPrefs;
use splice_proto::LayoutDoc;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub master_enabled: bool,
    pub clipboard_sync: bool,
    /// Panic chord as evdev codes (all must be held simultaneously).
    pub panic_chord: Vec<u32>,
    /// Last adopted layout (rejoin the cluster with prior arrangement).
    pub layout: Option<LayoutDoc>,
    /// Edge-crossing dwell in ms (0 = effortless; the knob exists for accident-prone edges).
    pub edge_dwell_ms: u32,
    /// Corner dead-zone size in logical px.
    pub corner_dead_zone: u32,
    /// Linux capture/injection implementation preferences.
    pub backends: BackendPrefs,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            master_enabled: true,
            clipboard_sync: true,
            panic_chord: default_panic_chord(),
            layout: None,
            edge_dwell_ms: 0,
            corner_dead_zone: 16,
            backends: BackendPrefs::default(),
        }
    }
}

fn default_panic_chord() -> Vec<u32> {
    use splice_platform::keymap::ev;
    vec![ev::KEY_LEFTSHIFT, ev::KEY_RIGHTSHIFT, ev::KEY_ESC]
}

/// Resolve the Splice config directory (`~/.config/splice` / `~/Library/Application
/// Support/splice`), creating it if needed.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "splice")
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn load(dir: &Path) -> anyhow::Result<Config> {
    use anyhow::Context;
    let path = dir.join("config.json");
    let config: Config = match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("invalid configuration in {}", path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    anyhow::ensure!(!config.panic_chord.is_empty(), "panic chord cannot be empty");
    anyhow::ensure!(
        config.panic_chord.iter().all(|code| *code > 0 && *code <= 0x2ff),
        "panic chord contains an invalid key code"
    );
    if let Some(layout) = &config.layout {
        layout.validate()?;
    }
    Ok(config)
}

/// Atomic save: write tmp, rename over.
pub fn save(dir: &Path, cfg: &Config) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(layout) = &cfg.layout {
        layout.validate()?;
    }
    let path = dir.join("config.json");
    let tmp = dir.join("config.json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(cfg)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("splice-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = load(&dir).unwrap();
        assert!(cfg.master_enabled);
        cfg.clipboard_sync = false;
        save(&dir, &cfg).unwrap();
        let back = load(&dir).unwrap();
        assert!(!back.clipboard_sync);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_existing_configuration_is_reported_and_preserved() {
        let dir = std::env::temp_dir().join(format!("splice-config-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for bytes in [b"{broken".as_slice(), b"{}".as_slice()] {
            std::fs::write(dir.join("config.json"), bytes).unwrap();
            assert!(load(&dir).is_err());
            assert_eq!(std::fs::read(dir.join("config.json")).unwrap(), bytes);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn permission_and_file_type_errors_are_not_treated_as_first_run() {
        let dir = std::env::temp_dir().join(format!("splice-config-directory-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("config.json")).unwrap();
        assert!(load(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
