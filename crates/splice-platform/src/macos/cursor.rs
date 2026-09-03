//! Cursor freeze/restore and the safety net around it.
//!
//! `CGAssociateMouseAndMouseCursorPosition(false)` is GLOBAL, PERSISTENT system state: if
//! this process dies while disassociated the user's mouse stays frozen with no cursor and
//! the only fix is a reboot or another process re-associating. Hence three independent
//! recovery paths — signal handlers, `atexit`, and a heartbeat watchdog.

use core_graphics::display::CGDisplay;
use core_graphics::geometry::CGPoint;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Once;
use std::time::{Duration, SystemTime};

/// True while the pointer is disassociated and the cursor hidden.
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// Milliseconds since the epoch, bumped by the tap thread on every run-loop tick.
static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static GUARDS: Once = Once::new();
/// Outstanding `CGDisplayHideCursor` calls. Every restore path unwinds all of them.
static HIDE_DEPTH: AtomicU32 = AtomicU32::new(0);
static FREEZE: Mutex<Freeze> = Mutex::new(Freeze { anchor: None, since_ms: 0 });

const WATCHDOG_STALE: Duration = Duration::from_secs(2);
/// Movement beyond this many points from the frozen position means the system quietly
/// re-associated the pointer behind our back.
const DRIFT_TOLERANCE: f64 = 2.0;
/// Events already in flight when the pointer freezes (or is warped) still carry their
/// pre-freeze locations; drift is not judged until they have drained.
const FREEZE_SETTLE_MS: u64 = 250;

/// Where the cursor is frozen, learned from the events themselves: the first location
/// reported after the settle window is where the pointer actually stopped.
struct Freeze {
    anchor: Option<CGPoint>,
    since_ms: u64,
}

impl Freeze {
    fn new(now: u64) -> Self {
        Self { anchor: None, since_ms: now }
    }

    /// Returns the anchor the pointer must be re-frozen at when `observed` proves the
    /// system re-associated it; re-arms the settle window in that case.
    fn observe(&mut self, now: u64, observed: CGPoint) -> Option<CGPoint> {
        if now.saturating_sub(self.since_ms) < FREEZE_SETTLE_MS {
            return None;
        }
        let Some(at) = self.anchor else {
            self.anchor = Some(observed);
            return None;
        };
        if (observed.x - at.x).abs() <= DRIFT_TOLERANCE
            && (observed.y - at.y).abs() <= DRIFT_TOLERANCE
        {
            return None;
        }
        *self = Self::new(now);
        Some(at)
    }
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

fn hide() {
    if CGDisplay::main().hide_cursor().is_ok() {
        HIDE_DEPTH.fetch_add(1, Ordering::SeqCst);
    }
}

fn show() {
    if HIDE_DEPTH
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |depth| depth.checked_sub(1))
        .is_ok()
    {
        let _ = CGDisplay::main().show_cursor();
    }
}

fn show_all() {
    while HIDE_DEPTH.load(Ordering::SeqCst) > 0 {
        show();
    }
}

/// Freeze the pointer: the cursor stops moving, hides, and mouse events start carrying
/// pure deltas. Idempotent — the hide count must stay balanced.
pub fn begin() {
    if CAPTURED.swap(true, Ordering::SeqCst) {
        return;
    }
    beat();
    super::ffi::set_cursor_in_background(true);
    hide();
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
    *FREEZE.lock() = Freeze::new(now_ms());
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
    let Some(at) = FREEZE.lock().observe(now_ms(), observed) else {
        return;
    };
    tracing::warn!(?at, ?observed, "pointer re-associated by the system while captured; re-freezing");
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
    let _ = CGDisplay::warp_mouse_cursor_position(at);
    show();
    hide();
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
    show_all();
    super::ffi::set_cursor_in_background(false);
}

/// Unconditional restore used by the crash paths. Takes no locks: it runs from signal
/// handlers.
fn force_restore() {
    CAPTURED.store(false, Ordering::SeqCst);
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    show_all();
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

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000;
    const SETTLED: u64 = T0 + FREEZE_SETTLE_MS;

    fn pt(x: f64, y: f64) -> CGPoint {
        CGPoint::new(x, y)
    }

    fn xy(p: Option<CGPoint>) -> Option<(f64, f64)> {
        p.map(|p| (p.x, p.y))
    }

    #[test]
    fn stale_locations_inside_the_settle_window_never_anchor() {
        let mut f = Freeze::new(T0);
        assert_eq!(xy(f.observe(T0 + 1, pt(3800.0, 969.0))), None);
        assert_eq!(xy(f.observe(T0 + 100, pt(3820.0, 969.0))), None);
        assert!(f.anchor.is_none(), "pre-freeze locations must not become the anchor");
        assert_eq!(xy(f.observe(SETTLED, pt(3839.98, 969.0))), None);
        assert_eq!(xy(f.anchor), Some((3839.98, 969.0)));
    }

    #[test]
    fn a_frozen_pointer_reports_no_drift() {
        let mut f = Freeze::new(T0);
        f.observe(SETTLED, pt(3839.98, 731.0));
        assert_eq!(xy(f.observe(SETTLED + 10, pt(3839.98, 731.0))), None);
        assert_eq!(
            xy(f.observe(SETTLED + 20, pt(3839.0, 732.5))),
            None,
            "sub-tolerance jitter is fine"
        );
    }

    #[test]
    fn real_drift_reports_the_anchor_and_rearms_the_settle_window() {
        let mut f = Freeze::new(T0);
        f.observe(SETTLED, pt(100.0, 100.0));
        assert_eq!(xy(f.observe(SETTLED + 10, pt(140.0, 100.0))), Some((100.0, 100.0)));
        assert_eq!(
            xy(f.observe(SETTLED + 11, pt(141.0, 100.0))),
            None,
            "the warp-back is in flight"
        );
        assert_eq!(xy(f.observe(SETTLED + 10 + FREEZE_SETTLE_MS, pt(100.0, 100.0))), None);
        assert_eq!(
            xy(f.anchor),
            Some((100.0, 100.0)),
            "re-anchored on what is actually observed"
        );
    }
}
