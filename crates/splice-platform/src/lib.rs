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

pub mod file_shelf;
pub mod files;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

pub trait ClipboardObserver: Send + Sync {
    fn invalidated(&self, generation: u64);
    fn changed(&self, generation: u64, mimes: Vec<String>, inline_text: Option<String>);
}

#[derive(Default)]
pub struct ClipboardClock {
    generation: std::sync::atomic::AtomicU64,
    observer: parking_lot::Mutex<Option<Arc<dyn ClipboardObserver>>>,
}

impl ClipboardClock {
    pub fn current(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn observe(&self, observer: Arc<dyn ClipboardObserver>) {
        *self.observer.lock() = Some(observer);
    }

    pub fn invalidate(&self) -> u64 {
        let observer = self.observer.lock();
        let generation = self.generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
        if let Some(observer) = observer.as_ref() {
            observer.invalidated(generation);
        }
        generation
    }

    pub fn changed(&self, generation: u64, mimes: Vec<String>, inline_text: Option<String>) -> bool {
        let observer = self.observer.lock();
        if self.current() != generation {
            return true;
        }
        if let Some(observer) = observer.as_ref() {
            observer.changed(generation, mimes, inline_text);
            true
        } else {
            false
        }
    }
}

pub fn native_clipboard_clock() -> Arc<ClipboardClock> {
    static CLOCK: std::sync::OnceLock<Arc<ClipboardClock>> = std::sync::OnceLock::new();
    CLOCK.get_or_init(Default::default).clone()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublicationState {
    Pending,
    Published,
    Cancelled,
}

#[derive(Clone)]
pub struct ClipboardGuard {
    clock: Arc<ClipboardClock>,
    generation: u64,
    intent: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
    publication: Arc<parking_lot::Mutex<PublicationState>>,
}

impl ClipboardGuard {
    pub fn new(clock: Arc<ClipboardClock>, generation: u64, intent: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>) -> Self {
        Self { clock, generation, intent, publication: Arc::new(parking_lot::Mutex::new(PublicationState::Pending)) }
    }

    pub fn published(&self) {
        let mut state = self.publication.lock();
        if *state == PublicationState::Pending {
            *state = PublicationState::Published;
        }
    }

    pub fn is_published(&self) -> bool {
        *self.publication.lock() == PublicationState::Published
    }

    pub fn is_cancelled(&self) -> bool {
        *self.publication.lock() == PublicationState::Cancelled
    }

    pub fn cancel(&self) {
        *self.publication.lock() = PublicationState::Cancelled;
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(PlatformError::Unavailable("clipboard publication was cancelled".into()));
        }
        if self.clock.current() != self.generation {
            return Err(PlatformError::Unavailable("the clipboard changed while the received files were being prepared".into()));
        }
        if self.intent.as_ref().is_some_and(|(latest, sequence)| latest.load(std::sync::atomic::Ordering::Acquire) != *sequence) {
            return Err(PlatformError::Unavailable("a newer receive superseded this publication".into()));
        }
        Ok(())
    }
}

/// Engine-provided callback used by clipboard backends to lazily pull remote data.
#[async_trait::async_trait]
pub trait ClipFetch: Send + Sync {
    fn publication_guard(&self) -> Option<ClipboardGuard> {
        None
    }

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
    RawBoundary { session: u64, edge: EdgeSpec, along: f64 },
    SwitchTarget,
    RawCaptureFailed(Arc<raw::RawOperation>),
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
    pub files: Option<file_shelf::FileAdapter>,
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

#[cfg(test)]
mod clipboard_clock_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Observer(parking_lot::Mutex<Vec<(u64, bool, Vec<String>)>>);

    impl ClipboardObserver for Observer {
        fn invalidated(&self, generation: u64) {
            self.0.lock().push((generation, false, Vec::new()));
        }

        fn changed(&self, generation: u64, mimes: Vec<String>, _inline_text: Option<String>) {
            self.0.lock().push((generation, true, mimes));
        }
    }

    #[test]
    fn invalidation_is_synchronous_and_empty_selection_supersedes_pending_text() {
        let clock = Arc::new(ClipboardClock::default());
        let observer = Arc::new(Observer::default());
        clock.observe(observer.clone());
        let text = clock.invalidate();
        assert_eq!(*observer.0.lock(), vec![(text, false, Vec::new())]);
        let empty = clock.invalidate();
        assert!(clock.changed(empty, Vec::new(), None));
        assert!(clock.changed(text, vec!["text/plain".into()], Some("late".into())));
        assert_eq!(*observer.0.lock(), vec![(text, false, Vec::new()), (empty, false, Vec::new()), (empty, true, Vec::new())]);
    }

    struct Fetch(ClipboardGuard);

    #[async_trait::async_trait]
    impl ClipFetch for Fetch {
        fn publication_guard(&self) -> Option<ClipboardGuard> {
            Some(self.0.clone())
        }

        async fn fetch(&self, _mime: &str) -> Option<Vec<u8>> {
            Some(Vec::new())
        }
    }

    #[tokio::test]
    async fn queued_native_publication_rechecks_generation_intent_and_cancellation() {
        for change in 0..3 {
            let (platform, mock) = mock::create(Vec::new());
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            mock.state.lock().clipboard_offer_gate = Some(gate.clone());
            let clock = Arc::new(ClipboardClock::default());
            let latest = Arc::new(AtomicU64::new(1));
            let guard = ClipboardGuard::new(clock.clone(), clock.current(), Some((latest.clone(), 1)));
            let fetch = Arc::new(Fetch(guard.clone()));
            let task = tokio::spawn(async move {
                platform.clipboard.set_remote_offer(ClipboardOffer { id: 1, mimes: vec!["text/uri-list".into()], inline_text: None }, fetch).await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while mock.state.lock().clipboard_offers_started == 0 {
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            match change {
                0 => { clock.invalidate(); }
                1 => latest.store(2, Ordering::Release),
                _ => guard.cancel(),
            }
            gate.add_permits(1);
            assert!(task.await.unwrap().is_err());
            assert!(mock.state.lock().remote_offers.is_empty());
            assert!(!guard.is_published());
        }
    }
}
