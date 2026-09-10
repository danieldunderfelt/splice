//! The engine task: owns all state, consumes platform/net/discovery/UI inputs, drives
//! capture/emulation, publishes UiState. Specification: docs/DESIGN.md (“Focus & source
//! arbitration”, “Core decisions”). Implemented by the core agent; the public surface
//! below is the contract for splice-app / splice-daemon.

mod inner;
mod clipboard_clock;

pub(crate) enum NativeClipboardEvent {
    Invalidated { generation: u64, epoch: u64 },
    Changed { generation: u64, mimes: Vec<String>, inline_text: Option<String>, epoch: u64 },
    Captured { selection: crate::files::LocalSelection, generation: u64, epoch: u64 },
}

use crate::ui_state::UiState;
use splice_proto::{MachineId, Vec2I};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Commands from UI / tray / daemon control.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Command {
    Files(crate::files::FileCommand),
    FileClipboardSelection { paths: Vec<std::path::PathBuf>, generation: u64 },
    SetInputSettings(crate::input_settings::InputSettings),
    SelectTarget(MachineId),
    Update {
        machine: MachineId,
        action: splice_update::control::Action,
    },
    ExportDiagnostics,
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
    files: crate::files::FileHandle,
    native_selection: clipboard_clock::Sender,
    cmd: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<UiState>,
    ready: watch::Receiver<Option<SocketAddr>>,
}

impl EngineHandle {
    pub fn invalidate_file_clipboard(&self, generation: u64) -> anyhow::Result<()> {
        self.native_selection.send(NativeClipboardEvent::Invalidated { generation, epoch: self.files.clipboard_epoch() })
    }

    pub fn clipboard_changed(&self, generation: u64, mimes: Vec<String>, inline_text: Option<String>) -> anyhow::Result<()> {
        anyhow::ensure!(mimes.len() <= 64 && mimes.iter().all(|m| m.len() <= 256) && inline_text.as_ref().is_none_or(|text| text.len() <= splice_proto::CLIP_INLINE_TEXT_MAX), "clipboard event exceeds metadata limits");
        self.native_selection.send(NativeClipboardEvent::Changed { generation, mimes, inline_text, epoch: self.files.clipboard_epoch() })
    }

    pub fn capture_file_clipboard(&self, selection: crate::files::LocalSelection, generation: u64) -> anyhow::Result<()> {
        selection.validate()?;
        self.native_selection.send(NativeClipboardEvent::Captured { selection, generation, epoch: self.files.clipboard_epoch() })
    }

    pub fn files(&self) -> crate::files::FileHandle { self.files.clone() }

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
        let update_host = splice_update::Host::new(&data_dir)?;
        Self::spawn_internal(
            platform,
            Arc::new(ts),
            data_dir,
            crate::net::NetOpts::default(),
            Duration::from_secs(15),
            Some(update_host),
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
        Self::spawn_internal(platform, ts, data_dir, net_opts, poll_interval, None).await
    }

    async fn spawn_internal(
        platform: splice_platform::Platform,
        ts: Arc<dyn crate::net::TsApi>,
        data_dir: std::path::PathBuf,
        net_opts: crate::net::NetOpts,
        poll_interval: Duration,
        update_host: Option<splice_update::Host>,
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
            update_host,
        )?;
        let files = inner.file_handle();
        let native_selection = inner.native_selection_sender();
        tokio::spawn(inner.run());
        Ok(EngineHandle { files, native_selection, cmd: cmd_tx, state: ui_rx, ready: ready_rx })
    }
}
