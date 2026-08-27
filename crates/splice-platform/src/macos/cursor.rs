//! Cursor freeze/restore and the safety net around it.
//!
//! `CGAssociateMouseAndMouseCursorPosition(false)` is GLOBAL, PERSISTENT system state: if
//! this process dies while disassociated the user's mouse stays frozen with no cursor and
//! the only fix is a reboot or another process re-associating. Hence three independent
//! recovery paths — signal handlers, `atexit`, and a heartbeat watchdog.

use core_graphics::display::CGDisplay;
use core_graphics::geometry::CGPoint;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Once;
use std::time::{Duration, SystemTime};

/// True while the pointer is disassociated and the cursor hidden.
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// Milliseconds since the epoch, bumped by the tap thread on every run-loop tick.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static GUARDS: Once = Once::new();

const WATCHDOG_STALE: Duration = Duration::from_secs(2);

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
    super::ffi::set_cursor_in_background(true);
    let _ = CGDisplay::main().hide_cursor();
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
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
