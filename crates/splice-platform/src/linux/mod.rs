//! Linux backend: a supervisor (`backends.rs`) that hot-swaps between implementations
//! per concern, chosen from the engine's preferences and what the session offers:
//!
//!   capture   — `capture.rs` (InputCapture portal + reis receiver: GNOME, KDE) or
//!               `overlay.rs` (layer-shell edge strips + pointer lock: KDE, wlroots,
//!               COSMIC, niri…; hides the cursor while away, never prompts).
//!   inject    — `emulate.rs` (RemoteDesktop portal + reis sender) or `uinput.rs`
//!               (virtual absolute pointer + keyboard; compositor-independent).
//!   clipboard — `clipboard.rs` (Clipboard portal on the RemoteDesktop session) or
//!               `datacontrol.rs` (ext/wlr data-control; no portal session needed).
//!
//! Always on: `activity.rs` (evdev read-only monitor, physical-activity signal + panic
//! chord), `displays.rs` (xdg-output geometry), `tokens.rs` (portal restore tokens),
//! `probe.rs` (what the compositor and the device nodes make available).
//!
//! READ `docs/research/wayland-input.md` and `docs/research/linux-native-input.md`
//! before touching the session-lifecycle code; the rules there are load-bearing.

mod activity;
mod backends;
mod capture;
mod clipboard;
mod datacontrol;
mod displays;
mod emulate;
mod overlay;
mod portal;
mod probe;
mod screensaver;
mod tokens;
mod uinput;

mod raw;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use splice_proto::DisplayRect;
use tokio::sync::mpsc::UnboundedSender;

use tokens::TokenStore;

use crate::{HealthReport, Platform, PlatformError, PlatformEvent, PlatformOpts, Result};

/// Name prefix of the uinput devices this process creates; the activity monitor skips
/// them so injected input never counts as physical.
pub const VIRTUAL_DEVICE_PREFIX: &str = "Splice Virtual";

/// State every Linux submodule needs: the event sink, the health report (published on
/// transitions only) and the current display geometry.
pub struct Shared {
    tx: UnboundedSender<PlatformEvent>,
    health: Mutex<HealthReport>,
    displays: RwLock<Vec<DisplayRect>>,
    epoch: Instant,
    /// Microseconds since `epoch` of the last uinput write; remappers (keyd, kanata,
    /// input-remapper) grab our virtual devices and re-emit on their own virtual
    /// devices, and the activity monitor uses this to recognise those echoes.
    last_injection: AtomicU64,
    /// Recently injected key/button edges (code, pressed, when), newest last.
    injected_keys: Mutex<std::collections::VecDeque<(u32, bool, Instant)>>,
}

const INJECTED_KEYS_KEPT: usize = 64;

impl Shared {
    pub fn note_injection(&self) {
        self.last_injection
            .store(self.epoch.elapsed().as_micros() as u64, Ordering::Release);
    }

    pub fn note_injected_key(&self, code: u32, pressed: bool) {
        let mut keys = self.injected_keys.lock();
        if keys.len() == INJECTED_KEYS_KEPT {
            keys.pop_front();
        }
        keys.push_back((code, pressed, Instant::now()));
    }

    /// Whether `code` with this state was injected within `window`.
    pub fn injected_recently(&self, code: u32, pressed: bool, window: std::time::Duration) -> bool {
        let now = Instant::now();
        self.injected_keys
            .lock()
            .iter()
            .rev()
            .take_while(|(_, _, at)| now.duration_since(*at) <= window)
            .any(|(c, p, _)| *c == code && *p == pressed)
    }

    /// Time since the last uinput write, or None if nothing was ever injected.
    pub fn since_injection(&self) -> Option<std::time::Duration> {
        let last = self.last_injection.load(Ordering::Acquire);
        if last == 0 {
            return None;
        }
        Some(self.epoch.elapsed().saturating_sub(std::time::Duration::from_micros(last)))
    }

    pub fn emit(&self, ev: PlatformEvent) {
        let _ = self.tx.send(ev);
    }

    /// Applies `f` to the health report and publishes it only if something changed.
    pub fn set_health(&self, f: impl FnOnce(&mut HealthReport)) {
        let mut health = self.health.lock();
        let before = health.clone();
        f(&mut health);
        if *health != before {
            let report = health.clone();
            drop(health);
            self.emit(PlatformEvent::Health(report));
        }
    }

    pub fn displays(&self) -> Vec<DisplayRect> {
        self.displays.read().clone()
    }

    fn set_displays(&self, displays: Vec<DisplayRect>) {
        *self.displays.write() = displays.clone();
        self.emit(PlatformEvent::DisplaysChanged { displays });
    }
}

/// Stops a running backend implementation; idempotent.
pub struct Stop(Box<dyn Fn() + Send + Sync>);

impl Stop {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Stop(Box::new(f))
    }

    pub fn stop(&self) {
        (self.0)()
    }
}

/// A locally-owned emergency release path used by the physical evdev monitor. It talks
/// directly to the active capture pump and does not depend on the engine or network.
#[derive(Clone)]
pub struct PanicRelease(Arc<dyn Fn() + Send + Sync>);

impl PanicRelease {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        PanicRelease(Arc::new(f))
    }

    pub fn trigger(&self) {
        (self.0)()
    }
}

pub async fn create(opts: PlatformOpts) -> Result<Platform> {
    let conn = match zbus::Connection::session().await {
        Ok(conn) => Some(conn),
        Err(err) => {
            tracing::warn!(error = %err, "no D-Bus session bus; portal backends unavailable");
            None
        }
    };
    let _ = std::fs::create_dir_all(&opts.data_dir);
    let tokens = Arc::new(TokenStore::load(&opts.data_dir));

    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        tx,
        health: Mutex::new(HealthReport::default()),
        displays: RwLock::new(Vec::new()),
        epoch: Instant::now(),
        last_injection: AtomicU64::new(0),
        injected_keys: Mutex::new(std::collections::VecDeque::with_capacity(INJECTED_KEYS_KEPT)),
    });

    let displays = displays::spawn(shared.clone())?;
    *shared.displays.write() = displays.clone();

    let (prefs_tx, prefs_rx) = tokio::sync::watch::channel(opts.backends);
    let handles = backends::spawn(
        shared.clone(),
        tokens,
        conn,
        opts.panic_chord.clone(),
        prefs_rx,
    )
    .await;
    let raw_input = Arc::new(raw::RelativeInput::new(shared.clone()));
    let raw_panic = PanicRelease::new({
        let raw_input = raw_input.clone();
        let panic = handles.panic.clone();
        move || {
            raw_input.force_release();
            panic.trigger();
        }
    });
    activity::spawn(
        shared.clone(),
        raw_panic,
        opts.panic_chord,
        handles.driven.clone(),
    );

    if handles.capture_unavailable && handles.inject_unavailable {
        return Err(PlatformError::Unavailable(
            "no input capture or injection implementation is available in this session".into(),
        ));
    }

    Ok(Platform {
        raw_capture: None,
        raw_emulate: Some(raw_input),
        capture: handles.capture,
        emulate: handles.emulate,
        clipboard: handles.clipboard,
        displays,
        events,
        backends: Some(prefs_tx),
    })
}
