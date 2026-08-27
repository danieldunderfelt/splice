//! Persistent configuration: settings + last-known layout. Atomic writes (tmp + rename).

use serde::{Deserialize, Serialize};
use splice_proto::LayoutDoc;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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
}

impl Default for Config {
    fn default() -> Self {
        use splice_platform::keymap::ev;
        Config {
            master_enabled: true,
            clipboard_sync: true,
            panic_chord: vec![ev::KEY_LEFTCTRL, ev::KEY_LEFTALT, ev::KEY_LEFTSHIFT, ev::KEY_ESC],
            layout: None,
            edge_dwell_ms: 0,
            corner_dead_zone: 16,
        }
    }
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

pub fn load(dir: &Path) -> Config {
    let path = dir.join("config.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("config.json unreadable ({e}); using defaults");
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

/// Atomic save: write tmp, rename over.
pub fn save(dir: &Path, cfg: &Config) -> anyhow::Result<()> {
    let path = dir.join("config.json");
    let tmp = dir.join("config.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("splice-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = load(&dir);
        assert!(cfg.master_enabled);
        cfg.clipboard_sync = false;
        save(&dir, &cfg).unwrap();
        let back = load(&dir);
        assert!(!back.clipboard_sync);
        std::fs::remove_dir_all(&dir).ok();
    }
}
