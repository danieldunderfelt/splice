//! The engine task: owns all state, consumes platform/net/discovery/UI inputs, drives
//! capture/emulation, publishes UiState. Specification: docs/DESIGN.md (“Focus & source
//! arbitration”, “Core decisions”). Implemented by the core agent; the public surface
//! below is the contract for splice-app / splice-daemon.

use crate::ui_state::UiState;
use splice_proto::{MachineId, Vec2I};
use tokio::sync::{mpsc, watch};

/// Commands from UI / tray / daemon control.
#[derive(Debug, Clone)]
pub enum Command {
    SetMasterEnabled(bool),
    SetMachineEnabled(MachineId, bool),
    SetPlacement(MachineId, Vec2I),
    SetSensitivity { link_key: String, factor: f64 },
    SetClipboardSync(bool),
    /// Local panic: end any session, release everything, broadcast Leave+ReleaseAll.
    Panic,
    /// Force a discovery refresh now.
    Refresh,
}

#[derive(Clone)]
pub struct EngineHandle {
    cmd: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<UiState>,
}

impl EngineHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.cmd.send(cmd);
    }
    pub fn state(&self) -> watch::Receiver<UiState> {
        self.state.clone()
    }
}

pub struct Engine;

impl Engine {
    /// Spawn the engine and all subsystem tasks. Returns immediately with a handle.
    ///
    /// `platform` is the OS backend (real or mock). `ts` is the LocalAPI client.
    /// `data_dir` hosts config.json / tokens.json.
    pub async fn spawn(
        _platform: splice_platform::Platform,
        _ts: splice_tailscale::Client,
        _data_dir: std::path::PathBuf,
    ) -> anyhow::Result<EngineHandle> {
        todo!("implemented by core agent — see docs/DESIGN.md")
    }
}
