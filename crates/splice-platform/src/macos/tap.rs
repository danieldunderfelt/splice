//! The event tap: a dedicated thread owning a CFRunLoop and an active session-level tap.
//!
//! Active taps are SYNCHRONOUS — every millisecond spent in the callback is a millisecond
//! of system-wide input lag, and going over the watchdog budget kills the tap outright. So
//! the callback does only four things: check the injected magic, keep the held-key ledger
//! for the panic chord, test edges when idle, and swallow+enqueue when capturing.

use super::ffi::{self, SPLICE_MAGIC};
use super::{cursor, MacShared};
use crate::keymap;
use crate::{CaptureEvent, EdgeSide, EdgeSpec, PlatformEvent};
use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use core_graphics::geometry::CGPoint;
use parking_lot::{Mutex, RwLock};
use splice_proto::{InputEvent, PointerButton, Vec2};
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Hot corners live within this radius of a display-union corner; edge hits there are
/// ignored so Splice and macOS don't fight over the same gesture (DESIGN 11).
const CORNER_DEAD_ZONE: f64 = 16.0;
/// How close to the boundary coordinate counts as contact.
const EDGE_TOLERANCE: f64 = 1.5;
const HEALTH_POLL: Duration = Duration::from_secs(5);
const SECURE_INPUT_POLL: Duration = Duration::from_secs(2);
const PUMP_SLICE: Duration = Duration::from_millis(100);
const ACTIVITY_DEBOUNCE_MS: u64 = 50;

const TAPPED_EVENTS: &[CGEventType] = &[
    CGEventType::LeftMouseDown,
    CGEventType::LeftMouseUp,
    CGEventType::RightMouseDown,
    CGEventType::RightMouseUp,
    CGEventType::MouseMoved,
    CGEventType::LeftMouseDragged,
    CGEventType::RightMouseDragged,
    CGEventType::KeyDown,
    CGEventType::KeyUp,
    CGEventType::FlagsChanged,
    CGEventType::ScrollWheel,
    CGEventType::OtherMouseDown,
    CGEventType::OtherMouseUp,
    CGEventType::OtherMouseDragged,
];

pub struct TapState {
    shared: Arc<MacShared>,
    edges: RwLock<Vec<EdgeSpec>>,
    corners: RwLock<Vec<(f64, f64)>>,
    capturing: AtomicBool,
    capture_lock: Mutex<()>,
    panic_chord: Vec<u32>,
    keys: Mutex<KeyState>,
    /// Edge currently in contact; cleared when the cursor leaves it, so one approach
    /// produces exactly one `EdgeHit`.
    contact: Mutex<Option<u32>>,
    last_activity_ms: Mutex<u64>,
    /// Raw `CFMachPortRef` of the live tap, so the callback can re-enable it inline.
    port: AtomicPtr<c_void>,
    need_recreate: AtomicBool,
}

#[derive(Default)]
struct KeyState {
    held: HashSet<u32>,
    chord_active: bool,
}

impl TapState {
    pub fn new(shared: Arc<MacShared>, panic_chord: Vec<u32>) -> Arc<Self> {
        let corners = super::displays::corners(&shared.displays.read());
        Arc::new(Self {
            shared,
            edges: RwLock::new(Vec::new()),
            corners: RwLock::new(corners),
            capturing: AtomicBool::new(false),
            capture_lock: Mutex::new(()),
            panic_chord,
            keys: Mutex::new(KeyState::default()),
            contact: Mutex::new(None),
            last_activity_ms: Mutex::new(0),
            port: AtomicPtr::new(std::ptr::null_mut()),
            need_recreate: AtomicBool::new(false),
        })
    }

    pub fn set_edges(&self, edges: Vec<EdgeSpec>) {
        *self.edges.write() = edges;
        *self.contact.lock() = None;
    }

    pub fn refresh_corners(&self) {
        *self.corners.write() = super::displays::corners(&self.shared.displays.read());
    }

    pub fn begin(&self) {
        let _guard = self.capture_lock.lock();
        if self.capturing.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.contact.lock() = None;
        cursor::begin();
    }

    pub fn end(&self, warp_to: Option<CGPoint>) {
        let _guard = self.capture_lock.lock();
        if !self.capturing.swap(false, Ordering::SeqCst) {
            return;
        }
        cursor::end(warp_to);
        *self.contact.lock() = None;
    }

    fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::SeqCst)
    }

    fn emit(&self, ev: CaptureEvent) {
        self.shared.emit(PlatformEvent::Capture(ev));
    }

    fn note_physical_activity(&self) {
        let now = cursor::now_ms();
        let mut last = self.last_activity_ms.lock();
        if now.saturating_sub(*last) < ACTIVITY_DEBOUNCE_MS {
            return;
        }
        *last = now;
        drop(last);
        self.shared.emit(PlatformEvent::PhysicalActivity);
    }

    /// Updates the held ledger and reports whether the panic chord just completed.
    fn track_key(&self, code: u32, pressed: bool) -> bool {
        let mut ks = self.keys.lock();
        if pressed {
            ks.held.insert(code);
        } else {
            ks.held.remove(&code);
        }
        if self.panic_chord.is_empty() {
            return false;
        }
        let all_down = self.panic_chord.iter().all(|c| ks.held.contains(c));
        let fired = all_down && !ks.chord_active;
        ks.chord_active = all_down;
        fired
    }

    fn edge_hit(&self, loc: CGPoint) -> Option<(u32, f64)> {
        for (cx, cy) in self.corners.read().iter() {
            if (loc.x - cx).abs() <= CORNER_DEAD_ZONE && (loc.y - cy).abs() <= CORNER_DEAD_ZONE {
                return None;
            }
        }
        self.edges.read().iter().find_map(|e| {
            let (cross, along) = match e.side {
                EdgeSide::Left | EdgeSide::Right => (loc.x, loc.y),
                EdgeSide::Top | EdgeSide::Bottom => (loc.y, loc.x),
            };
            let touching = match e.side {
                EdgeSide::Left | EdgeSide::Top => cross <= e.at as f64 + EDGE_TOLERANCE,
                EdgeSide::Right | EdgeSide::Bottom => cross >= e.at as f64 - EDGE_TOLERANCE,
            };
            let within = along >= e.from as f64 && along <= e.to as f64;
            (touching && within).then_some((e.id, along))
        })
    }
}

/// Spawns the tap thread. It owns the run loop for the lifetime of the process.
pub fn spawn(st: Arc<TapState>) {
    std::thread::Builder::new()
        .name("splice-event-tap".into())
        .spawn(move || run(st))
        .expect("spawning the event tap thread");
}

struct LiveTap {
    tap: core_graphics::event::CGEventTap<'static>,
    source: core_foundation::runloop::CFRunLoopSource,
}

impl Drop for LiveTap {
    fn drop(&mut self) {
        CFRunLoop::get_current().remove_source(&self.source, unsafe { kCFRunLoopCommonModes });
    }
}

fn run(st: Arc<TapState>) {
    install_wake_observers(st.clone());
    let mut live: Option<LiveTap> = None;
    let mut last_health = Instant::now();
    // Report Secure Input on the first pass rather than 2 s in.
    let mut last_secure = Instant::now()
        .checked_sub(SECURE_INPUT_POLL)
        .unwrap_or_else(Instant::now);

    loop {
        if live.is_none() || st.need_recreate.swap(false, Ordering::SeqCst) {
            st.port.store(std::ptr::null_mut(), Ordering::SeqCst);
            live = None;
            match create_tap(&st) {
                Some(t) => {
                    st.port.store(
                        t.tap.mach_port().as_concrete_TypeRef() as *mut c_void,
                        Ordering::SeqCst,
                    );
                    t.tap.enable();
                    st.shared.set_health(|h| h.capture = None);
                    live = Some(t);
                }
                None => {
                    st.shared.set_health(|h| {
                        h.capture = Some(
                            "Accessibility permission missing — grant Splice access in \
                             System Settings › Privacy & Security › Accessibility"
                                .into(),
                        )
                    });
                }
            }
        }

        cursor::beat();
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, PUMP_SLICE, false);
        cursor::beat();

        if last_health.elapsed() >= HEALTH_POLL {
            last_health = Instant::now();
            poll_health(&st, live.is_some());
        }
        if last_secure.elapsed() >= SECURE_INPUT_POLL {
            last_secure = Instant::now();
            poll_secure_input(&st);
        }
    }
}

fn create_tap(st: &Arc<TapState>) -> Option<LiveTap> {
    let cb_state = st.clone();
    let tap = core_graphics::event::CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        TAPPED_EVENTS.to_vec(),
        move |_proxy, etype, event| on_event(&cb_state, etype, event),
    )
    .ok()?;
    let source = tap.mach_port().create_runloop_source(0).ok()?;
    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
    Some(LiveTap { tap, source })
}

fn on_event(st: &Arc<TapState>, etype: CGEventType, event: &CGEvent) -> CallbackResult {
    cursor::beat();

    match etype {
        // Our callback exceeded the system budget (or the machine hitched). Re-enabling is
        // enough and capture state stays valid.
        CGEventType::TapDisabledByTimeout => {
            let port = st.port.load(Ordering::SeqCst) as CFMachPortRef;
            if !port.is_null() {
                unsafe { ffi::CGEventTapEnable(port, true) };
            }
            tracing::warn!("event tap disabled by timeout; re-enabled");
            return CallbackResult::Keep;
        }
        // Secure Input started or TCC was revoked. Re-enabling never sticks — the cursor
        // must be re-associated NOW or the machine is unusable.
        CGEventType::TapDisabledByUserInput => {
            st.end(None);
            st.need_recreate.store(true, Ordering::SeqCst);
            st.emit(CaptureEvent::Broken {
                reason: "event tap disabled by user input (Secure Input or revoked permission)"
                    .into(),
            });
            return CallbackResult::Keep;
        }
        _ => {}
    }

    if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == SPLICE_MAGIC {
        // Our own injection looping back through the session tap. Never physical.
        return CallbackResult::Keep;
    }

    let capturing = st.is_capturing();
    if !capturing {
        st.note_physical_activity();
    }

    let key_edge = key_edge_of(st, etype, event);
    if let Some((code, pressed)) = key_edge {
        if st.track_key(code, pressed) {
            // Panic must work with the network wedged: restore locally first, report after.
            st.end(None);
            st.emit(CaptureEvent::Panic);
            return CallbackResult::Drop;
        }
    }

    if !capturing {
        if let CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged = etype
        {
            let loc = event.location();
            match st.edge_hit(loc) {
                Some((edge_id, along)) => {
                    let mut contact = st.contact.lock();
                    if *contact != Some(edge_id) {
                        *contact = Some(edge_id);
                        drop(contact);
                        st.emit(CaptureEvent::EdgeHit { edge_id, along });
                    }
                }
                None => *st.contact.lock() = None,
            }
        }
        return CallbackResult::Keep;
    }

    for ev in translate(etype, event, key_edge) {
        st.emit(CaptureEvent::Input(ev));
    }
    CallbackResult::Drop
}

/// Key press/release edges, with autorepeat filtered (DESIGN 16: repeats are regenerated at
/// the destination). `None` for non-keyboard events and for unmapped keycodes.
fn key_edge_of(st: &Arc<TapState>, etype: CGEventType, event: &CGEvent) -> Option<(u32, bool)> {
    let vk = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    match etype {
        CGEventType::KeyDown | CGEventType::KeyUp => {
            if event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0 {
                return None;
            }
            let pressed = matches!(etype, CGEventType::KeyDown);
            match keymap::mac_to_evdev(vk) {
                Some(code) => Some((code, pressed)),
                None => {
                    tracing::debug!(vk, "dropping key with no evdev equivalent");
                    None
                }
            }
        }
        // FlagsChanged carries the keycode of the modifier that changed but no direction;
        // the held ledger supplies it. Reading the flag mask instead would be wrong with
        // both Shifts down — the mask stays set when only one of them is released.
        CGEventType::FlagsChanged => keymap::mac_to_evdev(vk)
            .map(|code| (code, !st.keys.lock().held.contains(&code))),
        _ => None,
    }
}

fn translate(
    etype: CGEventType,
    event: &CGEvent,
    key_edge: Option<(u32, bool)>,
) -> Vec<InputEvent> {
    match etype {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            // Post-acceleration deltas in points — exactly what the wire wants (DESIGN 10).
            let dx = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
            let dy = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);
            if dx == 0.0 && dy == 0.0 {
                vec![]
            } else {
                vec![InputEvent::Motion { dx, dy }]
            }
        }
        CGEventType::LeftMouseDown => vec![button(PointerButton::Left, true)],
        CGEventType::LeftMouseUp => vec![button(PointerButton::Left, false)],
        CGEventType::RightMouseDown => vec![button(PointerButton::Right, true)],
        CGEventType::RightMouseUp => vec![button(PointerButton::Right, false)],
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            let n = event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let pressed = matches!(etype, CGEventType::OtherMouseDown);
            vec![button(other_button(n), pressed)]
        }
        CGEventType::ScrollWheel => scroll(event),
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
            let Some((code, pressed)) = key_edge else { return vec![] };
            // DESIGN keymap note: CapsLock is a lock state, not an edge — never forwarded.
            if code == keymap::ev::KEY_CAPSLOCK {
                return vec![];
            }
            vec![InputEvent::Key { code, pressed }]
        }
        _ => vec![],
    }
}

fn button(button: PointerButton, pressed: bool) -> InputEvent {
    InputEvent::Button { button, pressed }
}

fn other_button(n: i64) -> PointerButton {
    match n {
        2 => PointerButton::Middle,
        3 => PointerButton::Back,
        4 => PointerButton::Forward,
        n => PointerButton::Other(n.clamp(0, 255) as u8),
    }
}

fn scroll(event: &CGEvent) -> Vec<InputEvent> {
    // Momentum is the target's job (DESIGN 15); forward only finger-driven scroll.
    if event.get_integer_value_field(ffi::FIELD_SCROLL_MOMENTUM_PHASE) != 0 {
        return vec![];
    }
    let phase = event.get_integer_value_field(ffi::FIELD_SCROLL_PHASE);
    // Device direction: the target applies its own natural-scroll preference.
    let sign = if super::natural_scroll_enabled() { -1.0 } else { 1.0 };

    if event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
        let dy = event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1)
            * sign;
        let dx = event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2)
            * sign;
        let mut out = Vec::new();
        if dx != 0.0 || dy != 0.0 {
            out.push(InputEvent::ScrollPixels { dx, dy });
        }
        if phase == ffi::SCROLL_PHASE_ENDED || phase == ffi::SCROLL_PHASE_CANCELLED {
            out.push(InputEvent::ScrollStop { cancel: phase == ffi::SCROLL_PHASE_CANCELLED });
        }
        out
    } else {
        let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
        let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
        if dx == 0 && dy == 0 {
            return vec![];
        }
        vec![InputEvent::Scroll120 {
            dx: (dx as f64 * sign) as i32 * 120,
            dy: (dy as f64 * sign) as i32 * 120,
        }]
    }
}

/// A non-nil, "enabled" tap can still be silently dead (re-signing, Launch Services). The
/// throwaway `tapCreate` is the only reliable revocation probe.
fn poll_health(st: &Arc<TapState>, have_tap: bool) {
    if have_tap {
        let port = st.port.load(Ordering::SeqCst) as CFMachPortRef;
        if !port.is_null() && !unsafe { ffi::CGEventTapIsEnabled(port) } {
            unsafe { ffi::CGEventTapEnable(port, true) };
            if !unsafe { ffi::CGEventTapIsEnabled(port) } {
                tracing::warn!("event tap will not re-enable; recreating");
                st.need_recreate.store(true, Ordering::SeqCst);
            }
        }
    }
    if !preflight_tap_create() {
        // Revocation while disassociated wedges system input — restore before anything else.
        if st.is_capturing() {
            st.end(None);
            st.emit(CaptureEvent::Broken { reason: "Accessibility permission revoked".into() });
        }
        st.need_recreate.store(true, Ordering::SeqCst);
        st.shared.set_health(|h| {
            h.capture = Some("Accessibility permission revoked — re-grant it for Splice".into())
        });
    } else if have_tap {
        st.shared.set_health(|h| h.capture = None);
    }
}

/// `CGEventTapCreate` returns NULL exactly when post access is missing.
fn preflight_tap_create() -> bool {
    core_graphics::event::CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::TailAppendEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::MouseMoved],
        |_, _, _| CallbackResult::Keep,
    )
    .is_ok()
}

fn poll_secure_input(st: &Arc<TapState>) {
    let status = super::secure_input_status();
    st.shared.set_health(|h| h.secure_input = status.clone());
}

/// Taps die across sleep/wake and lock/unlock and never come back on their own.
fn install_wake_observers(st: Arc<TapState>) {
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification,
        NSWorkspaceSessionDidBecomeActiveNotification,
    };
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    for name in [
        unsafe { NSWorkspaceDidWakeNotification },
        unsafe { NSWorkspaceSessionDidBecomeActiveNotification },
    ] {
        let st = st.clone();
        let block = block2::RcBlock::new(move |_n: std::ptr::NonNull<objc2_foundation::NSNotification>| {
            tracing::info!("system wake/session change; recreating the event tap");
            st.need_recreate.store(true, Ordering::SeqCst);
        });
        // Leaked on purpose: the observer must outlive the process's capture stack.
        let token =
            unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) };
        std::mem::forget(token);
    }
}

/// `warp_to` in engine coords is already CG global points on macOS.
pub fn warp_point(v: Vec2) -> CGPoint {
    CGPoint::new(v.x, v.y)
}
