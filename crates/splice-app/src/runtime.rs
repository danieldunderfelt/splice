//! Tokio runtime on a background thread + engine bootstrap.
//!
//! Bootstrap: config_dir -> config load -> platform::create -> tailscale Client::discover
//! -> Engine::spawn. Any failure (including a panic from the still-stubbed core engine)
//! is surfaced as `BootStatus::Offline` and retried every 15 s (or immediately via the
//! UI's Retry button) — the app itself never crashes on bootstrap problems.
//!
//! Preview mode (`SPLICE_UI_PREVIEW=1`) skips bootstrap entirely and drives the UI from a
//! canned, mutable UiState (see `preview`).

use anyhow::Context;
use parking_lot::{Mutex, RwLock};
use splice_core::{Command, EngineHandle, UiState};
use splice_proto::MachineId;
use std::sync::Arc;
use std::time::Duration;

const RETRY_INTERVAL: Duration = Duration::from_secs(15);

/// What the UI should show about engine connectivity.
#[derive(Clone, Debug, PartialEq)]
pub enum BootStatus {
    Starting,
    Online,
    /// Bootstrap failed; value is the human-readable cause ("engine offline: {0}").
    Offline(String),
    Preview,
}

/// Everything the UI needs to render and command. Clone freely.
#[derive(Clone)]
pub struct Controller {
    state: Arc<RwLock<UiState>>,
    status: Arc<Mutex<BootStatus>>,
    mode: Mode,
    /// Handle of the background tokio runtime (Linux: used to spawn the ksni tray task).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub tokio: Option<tokio::runtime::Handle>,
}

#[derive(Clone)]
enum Mode {
    Engine {
        handle: Arc<Mutex<Option<EngineHandle>>>,
        retry: tokio::sync::mpsc::UnboundedSender<()>,
    },
    Preview,
}

impl Controller {
    /// Latest published snapshot. The UI renders this and nothing else.
    pub fn state(&self) -> UiState {
        self.state.read().clone()
    }

    pub fn status(&self) -> BootStatus {
        self.status.lock().clone()
    }

    /// True when commands are meaningful (engine online or preview driver).
    pub fn is_live(&self) -> bool {
        match &self.mode {
            Mode::Preview => true,
            Mode::Engine { .. } => matches!(*self.status.lock(), BootStatus::Online),
        }
    }

    pub fn send(&self, cmd: Command) {
        match &self.mode {
            Mode::Engine { handle, .. } => {
                if let Some(handle) = handle.lock().as_ref() {
                    handle.send(cmd);
                } else {
                    tracing::debug!(?cmd, "engine offline; dropping command");
                }
            }
            Mode::Preview => {
                preview::apply(&mut self.state.write(), &cmd);
            }
        }
    }

    /// Ask the bootstrap loop to retry now instead of waiting out the interval.
    pub fn retry(&self) {
        if let Mode::Engine { retry, .. } = &self.mode {
            let _ = retry.send(());
        }
    }

    /// Shared UiState for the Linux ksni tray task (menus are generated from state).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn shared_state(&self) -> Arc<RwLock<UiState>> {
        self.state.clone()
    }
}

/// Start the background runtime thread and return the UI-side controller.
pub fn start(preview: bool, ctx: egui::Context) -> Controller {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("splice-tokio")
        .enable_all()
        .build();

    match runtime {
        Ok(runtime) => start_with_runtime(runtime, preview, ctx),
        Err(err) => {
            // No runtime: engine mode can never come up, but the UI must still run.
            tracing::error!("failed to build tokio runtime: {err}");
            Controller {
                state: Arc::new(RwLock::new(UiState::initial(MachineId("self".into())))),
                status: Arc::new(Mutex::new(BootStatus::Offline(format!(
                    "failed to start background runtime: {err}"
                )))),
                mode: Mode::Preview,
                tokio: None,
            }
        }
    }
}

fn start_with_runtime(
    runtime: tokio::runtime::Runtime,
    preview: bool,
    ctx: egui::Context,
) -> Controller {
    let tokio = Some(runtime.handle().clone());

    if preview {
        let spawned = std::thread::Builder::new()
            .name("splice-runtime".into())
            .spawn(move || {
                // No bootstrap in preview; keep the runtime parked for tray tasks (Linux).
                runtime.block_on(std::future::pending::<()>());
            });
        if let Err(err) = spawned {
            tracing::error!("failed to spawn runtime thread: {err}");
        }
        return Controller {
            state: Arc::new(RwLock::new(preview::initial_state())),
            status: Arc::new(Mutex::new(BootStatus::Preview)),
            mode: Mode::Preview,
            tokio,
        };
    }

    let state = Arc::new(RwLock::new(UiState::initial(MachineId("self".into()))));
    let status = Arc::new(Mutex::new(BootStatus::Starting));
    let (retry_tx, retry_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let handle_slot: Arc<Mutex<Option<EngineHandle>>> = Arc::new(Mutex::new(None));

    let ctrl = Controller {
        state: state.clone(),
        status: status.clone(),
        mode: Mode::Engine {
            handle: handle_slot.clone(),
            retry: retry_tx,
        },
        tokio,
    };

    let spawned = std::thread::Builder::new()
        .name("splice-runtime".into())
        .spawn(move || runtime_thread(runtime, retry_rx, state, status, handle_slot, ctx));
    if let Err(err) = spawned {
        tracing::error!("failed to spawn runtime thread: {err}");
        *ctrl.status.lock() = BootStatus::Offline(format!("failed to spawn runtime: {err}"));
    }

    ctrl
}

fn runtime_thread(
    runtime: tokio::runtime::Runtime,
    mut retry_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    state: Arc<RwLock<UiState>>,
    status: Arc<Mutex<BootStatus>>,
    handle_slot: Arc<Mutex<Option<EngineHandle>>>,
    ctx: egui::Context,
) {
    loop {
        // A panic anywhere in bootstrap must read as an ordinary bootstrap
        // failure, never a crash of the whole app.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(bootstrap())
        }));

        let handle = match outcome {
            Ok(Ok(handle)) => handle,
            Ok(Err(err)) => {
                tracing::warn!("engine bootstrap failed: {err:#}");
                *status.lock() = BootStatus::Offline(format!("{err:#}"));
                ctx.request_repaint();
                wait_for_retry(&runtime, &mut retry_rx);
                continue;
            }
            Err(payload) => {
                let msg = panic_message(&payload);
                tracing::warn!("engine bootstrap panicked: {msg}");
                *status.lock() =
                    BootStatus::Offline(format!("engine crashed during startup: {msg}"));
                ctx.request_repaint();
                wait_for_retry(&runtime, &mut retry_rx);
                continue;
            }
        };

        // Online: publish immediately, then forward every watch change into a repaint,
        // coalesced to <=10 Hz (DESIGN: "repaints on change (coalesced <=10 Hz)").
        *state.write() = handle.state().borrow().clone();
        *handle_slot.lock() = Some(handle.clone());
        *status.lock() = BootStatus::Online;
        ctx.request_repaint();

        let mut watch = handle.state();
        let fwd_state = state.clone();
        let fwd_status = status.clone();
        let fwd_ctx = ctx.clone();
        runtime.spawn(async move {
            loop {
                if watch.changed().await.is_err() {
                    *fwd_status.lock() = BootStatus::Offline("engine stopped".into());
                    fwd_ctx.request_repaint();
                    break;
                }
                *fwd_state.write() = watch.borrow_and_update().clone();
                fwd_ctx.request_repaint();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        // Bootstrap succeeded; park this thread, keeping the runtime (and engine) alive.
        runtime.block_on(std::future::pending::<()>());
    }
}

fn wait_for_retry(
    runtime: &tokio::runtime::Runtime,
    retry_rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    runtime.block_on(async {
        tokio::select! {
            _ = tokio::time::sleep(RETRY_INTERVAL) => {}
            _ = retry_rx.recv() => {}
        }
    });
}

async fn bootstrap() -> anyhow::Result<EngineHandle> {
    let data_dir = splice_core::config::config_dir().context("resolving config dir")?;
    let cfg = splice_core::config::load(&data_dir);
    let platform = splice_platform::create(splice_platform::PlatformOpts {
        data_dir: data_dir.clone(),
        panic_chord: cfg.panic_chord.clone(),
    })
    .await
    .context("initializing platform backend")?;
    let ts = splice_tailscale::Client::discover()
        .await
        .context("connecting to tailscaled (is Tailscale running?)")?;
    splice_core::Engine::spawn(platform, ts, data_dir)
        .await
        .context("spawning engine")
}

fn panic_message(payload: &dyn std::any::Any) -> String {
    if let Some(msg) = payload.downcast_ref::<&str>() {
        (*msg).to_owned()
    } else if let Some(msg) = payload.downcast_ref::<String>() {
        msg.clone()
    } else {
        "unknown panic".into()
    }
}

/// Preview-mode driver: a canned, mutable UiState for UI development and smoke runs.
pub mod preview {
    use splice_core::layout::{self, MachineGeom};
    use splice_core::ui_state::{UiConnection, UiEdge, UiFocus, UiMachine};
    use splice_core::{Command, UiState};
    use splice_platform::HealthReport;
    use splice_proto::{DisplayRect, MachineId, MachinePlacement, Os, Vec2I};
    use std::collections::BTreeMap;

    const SELF_ID: &str = "n100self";
    const GNOME_ID: &str = "n200gnome";
    const KDE_ID: &str = "n300kde";
    const THINKPAD_ID: &str = "n400thinkpad";

    fn display(id: &str, x: i32, y: i32, w: u32, h: u32, scale: f64) -> DisplayRect {
        DisplayRect {
            id: id.into(),
            x,
            y,
            w,
            h,
            scale,
        }
    }

    fn machine(
        id: &str,
        hostname: &str,
        os: Os,
        displays: Vec<DisplayRect>,
        offset: (i32, i32),
        enabled: bool,
        connection: UiConnection,
    ) -> UiMachine {
        UiMachine {
            id: MachineId(id.into()),
            hostname: hostname.into(),
            os,
            displays,
            offset: Vec2I {
                x: offset.0,
                y: offset.1,
            },
            enabled,
            connection,
            is_source: false,
        }
    }

    pub fn initial_state() -> UiState {
        let self_id = MachineId(SELF_ID.into());
        let hostname = std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "this-mac".into());

        let mut this = machine(
            SELF_ID,
            &hostname,
            Os::Macos,
            vec![
                display("1", 0, 0, 1512, 982, 2.0),
                display("2", 1512, 0, 1920, 1080, 1.0),
            ],
            (0, 0),
            true,
            UiConnection::SelfMachine,
        );
        this.is_source = true;

        let mut state = UiState {
            self_id: self_id.clone(),
            master_enabled: true,
            clipboard_sync: true,
            machines: vec![
                this,
                machine(
                    GNOME_ID,
                    "fedora-gnome",
                    Os::Linux,
                    vec![display("1", 0, 0, 2560, 1440, 1.0)],
                    (3432, 0),
                    true,
                    UiConnection::Direct { rtt_ms: 4.8 },
                ),
                machine(
                    KDE_ID,
                    "fedora-kde",
                    Os::Linux,
                    vec![display("1", 0, 0, 3840, 1080, 1.0)],
                    (3432, 1440),
                    true,
                    UiConnection::Derp { rtt_ms: 38.0 },
                ),
                machine(
                    THINKPAD_ID,
                    "old-thinkpad",
                    Os::Linux,
                    vec![display("1", 0, 0, 1920, 1080, 1.0)],
                    (0, 1080),
                    false,
                    UiConnection::Offline,
                ),
            ],
            edges: Vec::new(),
            source: Some(self_id),
            focus: UiFocus::Local,
            health: HealthReport {
                secure_input: Some("1Password".into()),
                ..HealthReport::default()
            },
            panic_chord: "Left Shift+Right Shift+Esc".into(),
            sensitivity: BTreeMap::new(),
            tailscale_error: None,
        };
        recompute_edges(&mut state);
        state
    }

    /// Apply a UI command to the canned state, mirroring what the engine would do.
    pub fn apply(state: &mut UiState, cmd: &Command) {
        match cmd {
            Command::SetMasterEnabled(on) => {
                state.master_enabled = *on;
                recompute_edges(state);
            }
            Command::SetMachineEnabled(id, on) => {
                if let Some(machine) = state.machines.iter_mut().find(|m| &m.id == id) {
                    machine.enabled = *on;
                }
                recompute_edges(state);
            }
            Command::SetArrangement(placements) => {
                for (id, offset) in placements {
                    if let Some(machine) = state.machines.iter_mut().find(|m| &m.id == id) {
                        machine.offset = *offset;
                    }
                }
                recompute_edges(state);
            }
            Command::SetSensitivity { link_key, factor } => {
                state
                    .sensitivity
                    .insert(link_key.clone(), factor.clamp(0.25, 4.0));
            }
            Command::SetClipboardSync(on) => {
                state.clipboard_sync = *on;
            }
            Command::Panic => {
                state.focus = UiFocus::Local;
            }
            Command::Refresh => {}
        }
    }

    fn reachable(state: &UiState, machine: &UiMachine) -> bool {
        state.master_enabled
            && matches!(
                machine.connection,
                UiConnection::SelfMachine | UiConnection::Direct { .. } | UiConnection::Derp { .. }
            )
    }

    /// Rebuild UiState.edges from geometry: every touching machine pair yields a strip;
    /// crossable iff both sides are enabled + reachable with the master switch on.
    fn recompute_edges(state: &mut UiState) {
        let mut geometry: BTreeMap<MachineId, MachineGeom> = state
            .machines
            .iter()
            .map(|m| {
                (
                    m.id.clone(),
                    MachineGeom {
                        id: m.id.clone(),
                        displays: m.displays.clone(),
                        placement: MachinePlacement {
                            offset: m.offset,
                            enabled: true,
                        },
                        reachable: true,
                    },
                )
            })
            .collect();
        let touching = layout::compute_links(&geometry);

        for (id, geom) in geometry.iter_mut() {
            if let Some(machine) = state.machines.iter().find(|m| &m.id == id) {
                geom.placement.enabled = machine.enabled;
                geom.reachable = reachable(state, machine);
            }
        }
        let crossable = layout::compute_links(&geometry);

        let crossable_keys: std::collections::BTreeSet<(MachineId, MachineId, i32, i32, i32, i32)> =
            crossable.iter().map(|link| link_key(state, link)).collect();

        let mut edges: Vec<UiEdge> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for link in &touching {
            let key = link_key(state, link);
            if !seen.insert(key.clone()) {
                continue;
            }
            let crossable = crossable_keys.contains(&key);
            edges.push(UiEdge {
                a: key.0,
                b: key.1,
                x1: key.2,
                y1: key.3,
                x2: key.4,
                y2: key.5,
                crossable,
            });
        }
        state.edges = edges;
    }

    fn edges_a(link: &layout::EdgeLink) -> MachineId {
        link.from.clone().min(link.to.clone())
    }

    fn edges_b(link: &layout::EdgeLink) -> MachineId {
        link.from.clone().max(link.to.clone())
    }

    /// Unordered dedupe key for a directed link, in canvas coordinates.
    fn link_key(
        state: &UiState,
        link: &layout::EdgeLink,
    ) -> (MachineId, MachineId, i32, i32, i32, i32) {
        let offset = state
            .machines
            .iter()
            .find(|m| m.id == link.from)
            .map(|m| m.offset)
            .unwrap_or_default();
        let (x1, y1, x2, y2) = match link.side {
            splice_platform::EdgeSide::Left | splice_platform::EdgeSide::Right => {
                let x = link.at + offset.x;
                (x, link.from_range.0 + offset.y, x, link.from_range.1 + offset.y)
            }
            splice_platform::EdgeSide::Top | splice_platform::EdgeSide::Bottom => {
                let y = link.at + offset.y;
                (link.from_range.0 + offset.x, y, link.from_range.1 + offset.x, y)
            }
        };
        (edges_a(link), edges_b(link), x1, y1, x2, y2)
    }
}
