//! The engine task: owns all state, consumes platform/net/discovery/UI inputs, drives
//! capture/emulation, publishes UiState. Specification: docs/DESIGN.md (“Focus & source
//! arbitration”, “Core decisions”). Implemented by the core agent; the public surface
//! below is the contract for splice-app / splice-daemon.

mod inner;

use crate::ui_state::UiState;
use splice_proto::{MachineId, Vec2I};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Commands from UI / tray / daemon control.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Command {
    SetMasterEnabled(bool),
    SetMachineEnabled(MachineId, bool),
    /// Commit a whole arrangement at once (the UI's constrained drag moves several cards).
    SetArrangement(Vec<(MachineId, Vec2I)>),
    SetSensitivity { link_key: String, factor: f64 },
    SetClipboardSync(bool),
    /// Linux: choose capture/injection implementations (hot-swapped by the backend).
    SetBackends(splice_platform::BackendPrefs),
    /// Local panic: end any session, release everything, broadcast Leave+ReleaseAll.
    Panic,
    /// Force a discovery refresh now.
    Refresh,
}

#[derive(Clone)]
pub struct EngineHandle {
    cmd: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<UiState>,
    ready: watch::Receiver<Option<SocketAddr>>,
}

impl EngineHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.cmd.send(cmd);
    }
    pub fn state(&self) -> watch::Receiver<UiState> {
        self.state.clone()
    }

    /// Bound address of the engine's listener, once bootstrap completes. Tests use this
    /// to wire `NetOpts::dial_ports` between in-process engines on loopback.
    #[doc(hidden)]
    pub async fn bound_addr(&self) -> Option<SocketAddr> {
        let mut rx = self.ready.clone();
        loop {
            if let Some(addr) = *rx.borrow() {
                return Some(addr);
            }
            if rx.changed().await.is_err() {
                return None;
            }
        }
    }
}

pub struct Engine;

impl Engine {
    /// Spawn the engine and all subsystem tasks. Returns immediately with a handle.
    ///
    /// `platform` is the OS backend (real or mock). `ts` is the LocalAPI client.
    /// `data_dir` hosts config.json / tokens.json.
    pub async fn spawn(
        platform: splice_platform::Platform,
        ts: splice_tailscale::Client,
        data_dir: std::path::PathBuf,
    ) -> anyhow::Result<EngineHandle> {
        Self::spawn_with(
            platform,
            Arc::new(ts),
            data_dir,
            crate::net::NetOpts::default(),
            Duration::from_secs(15),
        )
        .await
    }

    /// Test/harness entry point: inject a fake LocalAPI, net tunables, and the
    /// discovery poll cadence. Bootstrap (tailscale status, bind) runs inside the
    /// engine task; this returns immediately like [`Engine::spawn`].
    #[doc(hidden)]
    pub async fn spawn_with(
        platform: splice_platform::Platform,
        ts: Arc<dyn crate::net::TsApi>,
        data_dir: std::path::PathBuf,
        net_opts: crate::net::NetOpts,
        poll_interval: Duration,
    ) -> anyhow::Result<EngineHandle> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = watch::channel(UiState::initial(MachineId(String::new())));
        let (ready_tx, ready_rx) = watch::channel(None);
        let inner = inner::Inner::new(
            platform,
            ts,
            data_dir,
            net_opts,
            poll_interval,
            cmd_rx,
            ui_tx,
            ready_tx,
        );
        tokio::spawn(inner.run());
        Ok(EngineHandle { cmd: cmd_tx, state: ui_rx, ready: ready_rx })
    }
}
