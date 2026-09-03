//! Cursor freeze/restore and the safety net around it.
//!
//! `CGAssociateMouseAndMouseCursorPosition(false)` is GLOBAL, PERSISTENT system state: if
//! this process dies while disassociated the user's mouse stays frozen with no cursor and
//! the only fix is a reboot or another process re-associating. Hence three independent
//! recovery paths — signal handlers, `atexit`, and a heartbeat watchdog.

use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Once;
use std::time::{Duration, SystemTime};

/// True while the pointer is disassociated and the cursor hidden.
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// Milliseconds since the epoch, bumped by the tap thread on every run-loop tick.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static GUARDS: Once = Once::new();
/// Where the cursor was frozen; while captured every mouse event must still report it.
static FROZEN_AT: Mutex<Option<CGPoint>> = Mutex::new(None);

const WATCHDOG_STALE: Duration = Duration::from_secs(2);
/// Movement beyond this many points from the frozen position means the system quietly
/// re-associated the pointer behind our back.
const DRIFT_TOLERANCE: f64 = 2.0;

fn current_position() -> Option<CGPoint> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    CGEvent::new(source).ok().map(|event| event.location())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn beat() {
    HEARTBEAT.store(now_ms(), Ordering::Relaxed);
}

pub fn is_captured() -> bool {
    CAPTURED.load(Ordering::SeqCst)
}

/// Freeze the pointer: the cursor stops moving, hides, and mouse events start carrying
/// pure deltas. Idempotent — the hide count must stay balanced.
pub fn begin() {
    if CAPTURED.swap(true, Ordering::SeqCst) {
        return;
    }
    beat();
    *FROZEN_AT.lock() = current_position();
    super::ffi::set_cursor_in_background(true);
    let _ = CGDisplay::main().hide_cursor();
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
}

/// Called with the location of every mouse event seen while captured. macOS can
/// re-associate the pointer on its own (display sleep/wake, another process calling
/// associate(true), space switches); the local cursor then moves in lock-step with the
/// remote one. Detect it by the reported location drifting from the frozen point and
/// re-freeze in place.
pub fn reassert(observed: CGPoint) {
    if !CAPTURED.load(Ordering::SeqCst) {
        return;
    }
    let mut frozen = FROZEN_AT.lock();
    let Some(at) = *frozen else {
        *frozen = Some(observed);
        return;
    };
    if (observed.x - at.x).abs() <= DRIFT_TOLERANCE && (observed.y - at.y).abs() <= DRIFT_TOLERANCE {
        return;
    }
    tracing::warn!(?at, ?observed, "pointer re-associated by the system while captured; re-freezing");
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
    let _ = CGDisplay::warp_mouse_cursor_position(at);
    let _ = CGDisplay::main().hide_cursor();
    let _ = CGDisplay::main().show_cursor();
    let _ = CGDisplay::main().hide_cursor();
}

/// Restore the pointer at `warp_to` (CG global points), or wherever it was frozen.
///
/// The double `associate(true)` around the warp is the SDL trick: a warp normally starts a
/// 0.25 s interval during which local mouse events are ignored, and re-associating cancels
/// it. Without this the cursor visibly stutters on every re-entry.
pub fn end(warp_to: Option<CGPoint>) {
    if !CAPTURED.swap(false, Ordering::SeqCst) {
        return;
    }
    *FROZEN_AT.lock() = None;
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    if let Some(pt) = warp_to {
        let _ = CGDisplay::warp_mouse_cursor_position(pt);
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    }
    let _ = CGDisplay::main().show_cursor();
    super::ffi::set_cursor_in_background(false);
}

/// Unconditional restore used by the crash paths. Does not consult `CAPTURED` bookkeeping
/// beyond deciding whether the hide count needs balancing.
fn force_restore() {
    let was = CAPTURED.swap(false, Ordering::SeqCst);
    *FROZEN_AT.lock() = None;
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    if was {
        let _ = CGDisplay::main().show_cursor();
    }
}

/// Installs the signal handlers, the `atexit` hook and the watchdog thread. Safe to call
/// more than once.
pub fn install_guards() {
    GUARDS.call_once(|| {
        for sig in [libc::SIGSEGV, libc::SIGABRT, libc::SIGINT, libc::SIGTERM] {
            unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
        }
        unsafe { libc::atexit(on_exit) };
        std::thread::Builder::new()
            .name("splice-cursor-watchdog".into())
            .spawn(watchdog)
            .expect("spawning cursor watchdog");
    });
}

extern "C" fn on_signal(sig: libc::c_int) {
    force_restore();
    // Restore the default disposition and re-raise so crashes still produce a real report.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

extern "C" fn on_exit() {
    force_restore();
}

fn watchdog() {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if !CAPTURED.load(Ordering::SeqCst) {
            continue;
        }
        let last = HEARTBEAT.load(Ordering::Relaxed);
        if now_ms().saturating_sub(last) > WATCHDOG_STALE.as_millis() as u64 {
            tracing::error!("capture thread stopped heartbeating; force-restoring the cursor");
            force_restore();
        }
    }
}
