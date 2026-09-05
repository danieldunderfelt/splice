//! Platform abstraction for Splice: input capture, input emulation, clipboard, physical
//! activity, and system health — implemented per OS.
//!
//! This file is the CONTRACT between `splice-core` and the backends. Backends live in
//! `macos/` and `linux/` (cfg-gated). `mock` provides a scriptable in-memory
//! implementation for core's tests.
//!
//! Threading model: backends run their own event pumps (CFRunLoop thread on macOS; tokio
//! tasks on Linux) and communicate with the engine exclusively through the
//! [`PlatformEvent`] mpsc channel and the async trait methods below. Trait methods must be
//! quick (enqueue work, don't block on OS dialogs).

pub mod keymap;
pub mod mock;
pub mod raw;
pub mod scroll;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod linux;

use splice_proto::{DisplayRect, InputEvent, Vec2};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("permission missing: {0}")]
    Permission(String),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

/// Which side of the machine's display-union boundary an edge segment lies on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// An armed capture edge: a segment of this machine's outer display boundary, in this
/// machine's local logical coordinates, through which the cursor may leave.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeSpec {
    /// Engine-assigned id; reported back in [`CaptureEvent::EdgeHit`].
    pub id: u32,
    pub side: EdgeSide,
    /// The boundary coordinate on the crossing axis (e.g. x of a Left/Right edge).
    pub at: i32,
    /// Segment start/end along the edge (y-range for Left/Right, x-range for Top/Bottom).
    pub from: i32,
    pub to: i32,
}

/// Events flowing from the capture backend to the engine.
#[derive(Clone, Debug)]
pub enum CaptureEvent {
    EdgeLeft,
    EdgeMotion {
        edge_id: u32,
        along: f64,
        dx: f64,
        dy: f64,
    },
    /// Cursor hit an armed edge. `along` is the position on the edge's from..to axis, in
    /// local logical coords. The engine responds by calling `begin_capture` (or ignoring).
    EdgeHit { edge_id: u32, along: f64 },
    /// While capturing: swallowed local input to forward. Motion deltas are
    /// source-accelerated logical px; keys are evdev codes (already translated on macOS).
    Input(InputEvent),
    /// Capture ended abnormally (tap died, portal session closed, Deactivated…).
    /// The engine must treat this as Leave{CaptureLost} + ReleaseAll.
    Broken { reason: String },
    /// Local panic chord was pressed. Backend has ALREADY released capture locally;
    /// the engine broadcasts Leave{Panic} + ReleaseAll.
    Panic,
}

/// Capture side (this machine as SOURCE).
#[async_trait::async_trait]
pub trait Capture: Send + Sync {
    /// Replace the set of armed edges (Wayland: pointer barriers; macOS: edge tests in the
    /// tap). Called on every layout/topology change; implementations must debounce/batch
    /// and NEVER churn portal sessions per call.
    async fn set_edges(&self, edges: Vec<EdgeSpec>) -> Result<()>;
    /// Begin swallowing+forwarding local input (cursor freezes/locks). Called by the engine
    /// in response to `EdgeHit` once the target session is established.
    async fn begin_capture(&self) -> Result<()>;
    /// Stop capturing; restore the local cursor at `warp_to` (local logical coords) if given.
    async fn end_capture(&self, warp_to: Option<Vec2>) -> Result<()>;
}

/// Emulation side (this machine as TARGET).
#[async_trait::async_trait]
pub trait Emulate: Send + Sync {
    /// A remote session begins: place the cursor at `pos` (local logical coords), take the
    /// keep-awake assertion, prepare devices.
    async fn enter(&self, pos: Vec2) -> Result<()>;
    /// Inject one event. Must be cheap; called at input rate.
    async fn inject(&self, ev: InputEvent) -> Result<()>;
    /// Session ends: release keep-awake. `release_held` lists evdev codes/buttons the engine
    /// still believes are down — implementations must force-release them regardless of their
    /// own ledger, then clear all internal held state.
    async fn leave(&self) -> Result<()>;
    /// Unconditionally release every held key/button this backend ever injected.
    async fn release_all(&self) -> Result<()>;
}

/// Clipboard integration.
#[async_trait::async_trait]
pub trait Clipboard: Send + Sync {
    /// Advertise remote-owned clipboard contents. When a local app pastes, the backend
    /// calls `fetch` (provided by the engine) to pull bytes lazily.
    async fn set_remote_offer(&self, offer: ClipboardOffer, fetch: Arc<dyn ClipFetch>) -> Result<()>;
    /// Read one representation of the LOCAL clipboard (engine serves peers with this).
    async fn read_local(&self, mime: &str) -> Result<Vec<u8>>;
}

/// Engine-provided callback used by clipboard backends to lazily pull remote data.
#[async_trait::async_trait]
pub trait ClipFetch: Send + Sync {
    /// Fetch a representation from the offering peer. Returns None if unavailable.
    async fn fetch(&self, mime: &str) -> Option<Vec<u8>>;
}

#[derive(Clone, Debug)]
pub struct ClipboardOffer {
    pub id: u64,
    /// MIME types in preference order (normalized: image/png, text/plain;charset=utf-8, …).
    pub mimes: Vec<String>,
    /// Small text payload inlined by the offerer (usable without a fetch round-trip).
    pub inline_text: Option<String>,
}

/// Events flowing from platform monitors to the engine.
#[derive(Clone, Debug)]
pub enum PlatformEvent {
    SwitchTarget,
    RawError(String),
    Capture(CaptureEvent),
    /// Physical (non-injected) local input observed → engine may claim sourceness.
    /// Debounced ≥50 ms by the backend.
    PhysicalActivity,
    /// Local clipboard changed with these normalized MIME types (+small text inline).
    ClipboardChanged { mimes: Vec<String>, inline_text: Option<String> },
    /// Display set changed; `displays` is the fresh list in local logical coords.
    DisplaysChanged { displays: Vec<DisplayRect> },
    /// Health/permission state changed (drives the UI status panel).
    Health(HealthReport),
    /// Linux: the active backend selection changed (drives the UI backend picker).
    Backends(BackendStatus),
}

/// Per-concern health status for the UI. `None` = OK.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HealthReport {
    /// macOS: Accessibility missing / tap dead. Linux: capture portal problem.
    pub capture: Option<String>,
    /// Injection-side problem (portal session, permission).
    pub emulate: Option<String>,
    /// macOS: Secure Input active — value is the culprit process description.
    pub secure_input: Option<String>,
    /// Linux: evdev monitor unavailable (udev rule missing).
    pub activity_monitor: Option<String>,
    /// Clipboard backend degraded.
    pub clipboard: Option<String>,
}

/// Capture implementation preference (Linux). `Auto` prefers the overlay where the
/// compositor supports it, because it never prompts and hides the cursor while away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CapturePref {
    #[default]
    Auto,
    /// xdg-desktop-portal InputCapture (GNOME, KDE).
    Portal,
    /// Wayland layer-shell edge strips + pointer lock (KDE, wlroots, COSMIC, niri…).
    Overlay,
}

/// Injection implementation preference (Linux). `Auto` prefers uinput when
/// `/dev/uinput` is accessible: it is compositor-independent and skips the EIS path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InjectPref {
    #[default]
    Auto,
    /// xdg-desktop-portal RemoteDesktop + libei.
    Portal,
    /// Virtual absolute pointer + keyboard on `/dev/uinput`.
    Uinput,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BackendPrefs {
    pub capture: CapturePref,
    pub inject: InjectPref,
}

/// What the Linux backend supervisor resolved the preferences to (drives the UI's
/// backend picker). Strings are human-readable implementation names.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BackendStatus {
    pub prefs: BackendPrefs,
    pub capture: String,
    pub inject: String,
    pub clipboard: String,
    pub overlay_available: bool,
    pub uinput_available: bool,
    pub portal_capture_available: bool,
    pub portal_inject_available: bool,
}

/// Everything the engine needs from the OS, bundled.
pub struct Platform {
    pub raw_capture: Option<Arc<dyn raw::RawCapture>>,
    pub raw_emulate: Option<Arc<dyn raw::RawEmulate>>,
    pub capture: Arc<dyn Capture>,
    pub emulate: Arc<dyn Emulate>,
    pub clipboard: Arc<dyn Clipboard>,
    /// Current displays at startup (later updates arrive via `DisplaysChanged`).
    pub displays: Vec<DisplayRect>,
    /// Unified event stream (capture events, activity, clipboard, displays, health).
    pub events: tokio::sync::mpsc::UnboundedReceiver<PlatformEvent>,
    /// Live backend selection (Linux only); the backend hot-swaps implementations when
    /// the engine publishes new preferences here.
    pub backends: Option<tokio::sync::watch::Sender<BackendPrefs>>,
}

/// Options for constructing the platform backend.
#[derive(Clone, Debug)]
pub struct PlatformOpts {
    /// Config directory (portal restore tokens live there).
    pub data_dir: std::path::PathBuf,
    /// Panic chord as evdev codes, all held simultaneously. The capture backend detects
    /// this LOCALLY (must work even if the engine/network is wedged), releases capture,
    /// then emits [`CaptureEvent::Panic`].
    pub panic_chord: Vec<u32>,
    /// Initial Linux backend preferences (ignored elsewhere).
    pub backends: BackendPrefs,
}

/// Construct the real platform backend for this OS.
///
/// Must be called from the process's main thread on macOS (event tap + run loop init).
pub async fn create(opts: PlatformOpts) -> Result<Platform> {
    #[cfg(target_os = "macos")]
    {
        macos::create(opts).await
    }
    #[cfg(target_os = "linux")]
    {
        linux::create(opts).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = opts;
        Err(PlatformError::Unavailable("unsupported OS".into()))
    }
}
