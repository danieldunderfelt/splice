//! macOS backend: CGEventTap capture, CGEventPost emulation, NSPasteboard clipboard,
//! physical-activity via event-source tagging, Secure Input + permission health.
//!
//! READ `docs/research/macos-input.md` BEFORE IMPLEMENTING. Every rule in it exists
//! because a shipping KVM got it wrong.
//!
//! Structure (implement in these modules):
//!   tap.rs      — event tap thread (CFRunLoop), edge tests, capture swallow/forward,
//!                 disable-event handling, health poll, wake/session notifications,
//!                 panic-chord detection, physical-vs-injected discrimination (field 42).
//!   inject.rs   — CGEventPost emulation: modifiers as FlagsChanged + cumulative flags,
//!                 click-state & event numbers, scroll (line + pixel), key-repeat synth,
//!                 IOPMAssertion keep-awake, release_all ledger.
//!   cursor.rs   — hide/show + (dis)associate + warp, crash-safe re-association guard
//!                 (signal handlers + atexit + watchdog).
//!   pasteboard.rs — changeCount poll, offer normalization (TIFF→PNG), promised items
//!                 via NSPasteboardItemDataProvider, loop guard.
//!   displays.rs — CGGetActiveDisplayList → DisplayRect, reconfiguration callback.
//!   ffi.rs      — small extern "C" decls: IOPMAssertion*, CGS SetsCursorInBackground,
//!                 IsSecureEventInputEnabled, kCGSSessionSecureInputPID lookup.

pub mod cursor;
pub mod displays;
pub mod ffi;
pub mod inject;
pub mod pasteboard;
pub mod tap;

mod raw;

use crate::{
    Capture, EdgeSpec, HealthReport, Platform, PlatformEvent, PlatformOpts, Result,
};
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use parking_lot::{Mutex, RwLock};
use splice_proto::{DisplayRect, Vec2};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// State every macOS submodule needs: the event sink, the live display list, and the
/// health report (which is only published on transitions, not on every poll).
pub struct MacShared {
    tx: UnboundedSender<PlatformEvent>,
    pub displays: RwLock<Vec<DisplayRect>>,
    health: Mutex<HealthReport>,
}

impl MacShared {
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

/// `None` when Secure Input is off. While it is on, keyboard events vanish from every tap
/// (the mouse keeps flowing), so the UI has to name the culprit.
pub fn secure_input_status() -> Option<String> {
    if unsafe { ffi::IsSecureEventInputEnabled() } == 0 {
        return None;
    }
    let culprit = secure_input_pid()
        .map(|pid| match ffi::process_name(pid) {
            Some(name) => format!("{name} (pid {pid})"),
            None => format!("pid {pid}"),
        })
        .unwrap_or_else(|| "an unknown process".into());
    Some(format!("keyboard paused: {culprit} has Secure Input enabled"))
}

fn secure_input_pid() -> Option<i32> {
    let raw = unsafe { ffi::CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        return None;
    }
    let dict: CFDictionary<CFString, core_foundation::base::CFType> =
        unsafe { CFDictionary::wrap_under_create_rule(raw) };
    let key = CFString::new("kCGSSessionSecureInputPID");
    dict.find(&key)?.downcast::<CFNumber>()?.to_i32()
}

/// Startup permission preflight. The Accessibility grant transitively covers listen+post on
/// macOS 13+, so the prompt is raised exactly once and points at exactly one toggle.
pub fn preflight_permissions() -> (bool, bool, bool) {
    let post = unsafe { ffi::CGPreflightPostEventAccess() };
    let listen = unsafe { ffi::CGPreflightListenEventAccess() };
    let trusted = ax_trusted(!post || !listen);
    (post, listen, trusted)
}

/// `AXIsProcessTrustedWithOptions` returns `Boolean` (unsigned char), not `int`.
pub fn ax_trusted(prompt: bool) -> bool {
    let key = unsafe { CFString::wrap_under_get_rule(ffi::kAXTrustedCheckOptionPrompt) };
    let options = CFDictionary::from_CFType_pairs(&[(
        key.as_CFType(),
        CFBoolean::from(prompt).as_CFType(),
    )]);
    unsafe { ffi::AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0 }
}

struct MacCapture {
    state: Arc<tap::TapState>,
}

#[async_trait::async_trait]
impl Capture for MacCapture {
    async fn set_edges(&self, edges: Vec<EdgeSpec>) -> Result<()> {
        self.state.set_edges(edges);
        Ok(())
    }

    async fn begin_capture(&self) -> Result<()> {
        self.state.begin();
        Ok(())
    }

    async fn end_capture(&self, warp_to: Option<Vec2>) -> Result<()> {
        self.state.end(warp_to.map(tap::warp_point));
        Ok(())
    }
}

pub async fn create(opts: PlatformOpts) -> Result<Platform> {
    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    let displays = displays::snapshot();
    let shared = Arc::new(MacShared {
        tx,
        displays: RwLock::new(displays.clone()),
        health: Mutex::new(HealthReport::default()),
    });

    // Before anything touches the pointer: a frozen cursor must never survive this process.
    cursor::install_guards();

    let (post, listen, trusted) = preflight_permissions();
    if !(post && listen && trusted) {
        shared.set_health(|h| {
            h.capture = Some(
                "Splice needs Accessibility access: System Settings › Privacy & Security › \
                 Accessibility. Input capture and injection stay off until it is granted."
                    .into(),
            )
        });
        tracing::warn!(post, listen, trusted, "macOS input permissions incomplete");
    }
    shared.set_health(|h| h.secure_input = secure_input_status());

    displays::register(shared.clone());

    let tap_state = tap::TapState::new(shared.clone(), opts.panic_chord);
    tap::spawn(tap_state.clone());

    // Corners depend on the display list; keep them fresh alongside DisplaysChanged.
    {
        let tap_state = tap_state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            loop {
                ticker.tick().await;
                tap_state.refresh_corners();
            }
        });
    }

    let emulate = Arc::new(inject::Injector::new(shared.clone())?);
    let clipboard = Arc::new(pasteboard::PasteboardClip::new(shared.clone()));

    Ok(Platform {
        raw_capture: Some(raw::HidCapture::spawn(shared.clone(), tap_state.clone())),
        raw_emulate: None,
        capture: Arc::new(MacCapture { state: tap_state }),
        emulate,
        clipboard,
        displays,
        events,
        backends: None,
    })
}
