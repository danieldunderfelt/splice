//! Linux Wayland backend: InputCapture portal + reis receiver (capture), RemoteDesktop
//! portal + reis sender (emulation), Clipboard portal on the RemoteDesktop session,
//! evdev read-only monitor (physical activity), ScreenSaver inhibit (keep-awake).
//!
//! READ `docs/research/wayland-input.md` BEFORE IMPLEMENTING. Session-lifecycle rules
//! (never Disable(), never churn, single-use restore tokens) are load-bearing.
//!
//! Structure (implement in these modules):
//!   capture.rs   — InputCapture session, portal zones, EdgeSpec→barriers,
//!                  Activated/Deactivated/ZonesChanged handling, reis receiver pump,
//!                  KDE barrier-id fallback, panic-chord detection from captured keys.
//!   emulate.rs   — RemoteDesktop session + reis sender pump, device management,
//!                  frames, scroll-120 accumulation, held-key ledger, release_all,
//!                  ScreenSaver inhibit while entered, screen-lock backoff.
//!   clipboard.rs — Clipboard portal on the RemoteDesktop session: SetSelection /
//!                  SelectionOwnerChanged / SelectionRead / SelectionTransfer+Write.
//!   activity.rs  — evdev read-only monitor with inotify hotplug + graceful degrade.
//!   displays.rs  — pre-consent xdg-output logical geometry + hotplug monitor.
//!   tokens.rs    — restore-token persistence (atomic write to data_dir/tokens.json).

mod activity;
mod capture;
mod clipboard;
mod emulate;
mod displays;
mod portal;
mod tokens;

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use tokens::TokenStore;

use crate::{HealthReport, Platform, PlatformError, PlatformEvent, PlatformOpts, Result};

/// State every Wayland submodule needs: the event sink and the health report (which is
/// only published on transitions, not on every update).
pub struct WaylandShared {
    tx: UnboundedSender<PlatformEvent>,
    health: Mutex<HealthReport>,
}

impl WaylandShared {
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
}

pub async fn create(opts: PlatformOpts) -> Result<Platform> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| PlatformError::Unavailable(format!("no D-Bus session bus: {e}")))?;
    // Token persistence writes into data_dir; it must exist for first-run consent to stick.
    let _ = std::fs::create_dir_all(&opts.data_dir);
    let tokens = Arc::new(TokenStore::load(&opts.data_dir));

    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(WaylandShared {
        tx,
        health: Mutex::new(HealthReport::default()),
    });

    // xdg-output is available before any portal consent. This breaks the
    // permission deadlock: peers can see and drive this machine while its
    // InputCapture dialog is still open.
    let displays = displays::spawn(shared.clone())?;

    // emulate before clipboard: RequestClipboard must precede RemoteDesktop Start (that
    // ordering lives inside emulate's session setup); clipboard consumes the session
    // watch channel emulate produces.
    let (emulate, clip_session_rx) =
        emulate::create(shared.clone(), tokens.clone(), conn.clone());
    let clipboard = clipboard::create(shared.clone(), conn.clone(), clip_session_rx);
    let (capture, panic_release) =
        capture::create(shared.clone(), tokens, conn, opts.panic_chord.clone());
    activity::spawn(shared.clone(), panic_release, opts.panic_chord);

    Ok(Platform {
        capture,
        emulate,
        clipboard,
        displays,
        events,
    })
}
