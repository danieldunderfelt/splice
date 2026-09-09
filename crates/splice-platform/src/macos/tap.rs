//! The event tap: a dedicated thread owning a CFRunLoop and an active session-level tap.
//!
//! Active taps are SYNCHRONOUS — every millisecond spent in the callback is a millisecond
//! of system-wide input lag, and going over the watchdog budget kills the tap outright. So
//! the callback does only four things: check the injected magic, keep the held-key ledger
//! for the panic chord, test edges when idle, and swallow+enqueue when capturing.

use super::ffi::{self, SPLICE_MAGIC};
use super::{cursor, MacShared};
use crate::keymap;
use crate::raw::shortcut::{Stream, SwitchShortcut};
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
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Hot corners live within this radius of a display-union corner; edge hits there are
/// ignored so Splice and macOS don't fight over the same gesture (DESIGN 11).
const CORNER_DEAD_ZONE: f64 = 16.0;
/// How close to the boundary coordinate counts as contact.
const EDGE_TOLERANCE: f64 = 1.5;
const HEALTH_POLL: Duration = Duration::from_secs(5);
const SECURE_INPUT_POLL: Duration = Duration::from_millis(250);
const PUMP_SLICE: Duration = Duration::from_millis(100);
const ACTIVITY_DEBOUNCE_MS: u64 = 50;
/// After handing the cursor back, the warp we post lands just inside the edge and
/// would immediately re-trigger the barrier. Ignore edge tests for this long so a
/// return never bounces straight back across.
const CROSS_COOLDOWN_MS: u64 = 250;

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
    pub raw_enabled: AtomicBool,
    pub raw_switching: AtomicBool,
    pub lifecycle: AtomicU64,
    available: AtomicBool,
    session_active: AtomicBool,
    secure_input: AtomicBool,
    switch: Mutex<SwitchShortcut>,
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
    /// Wall-clock ms of the last capture end; edge tests are muted briefly after it.
    last_end_ms: AtomicU64,
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
            raw_enabled: AtomicBool::new(false),
            raw_switching: AtomicBool::new(false),
            lifecycle: AtomicU64::new(0),
            available: AtomicBool::new(false),
            session_active: AtomicBool::new(true),
            secure_input: AtomicBool::new(false),
            switch: Mutex::new(SwitchShortcut::default()),
            shared,
            edges: RwLock::new(Vec::new()),
            corners: RwLock::new(corners),
            capturing: AtomicBool::new(false),
            capture_lock: Mutex::new(()),
            panic_chord,
            keys: Mutex::new(KeyState::default()),
            contact: Mutex::new(None),
            last_activity_ms: Mutex::new(0),
            last_end_ms: AtomicU64::new(0),
            port: AtomicPtr::new(std::ptr::null_mut()),
            need_recreate: AtomicBool::new(false),
        })
    }

    pub fn set_edges(&self, edges: Vec<EdgeSpec>) {
        let mut current = self.edges.write();
        if *current == edges {
            return;
        }
        *current = edges;
        drop(current);
        *self.contact.lock() = None;
    }

    pub fn refresh_corners(&self) {
        *self.corners.write() = super::displays::corners(&self.shared.displays.read());
    }

    pub fn begin(&self) -> crate::Result<()> {
        let _guard = self.capture_lock.lock();
        if !self.available() || unsafe { ffi::IsSecureEventInputEnabled() } != 0 {
            return Err(crate::PlatformError::Unavailable(
                "macOS input capture is unavailable or Secure Input is active".into(),
            ));
        }
        self.raw_switching.store(false, Ordering::SeqCst);
        if self.capturing.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        *self.contact.lock() = None;
        cursor::begin();
        self.emit_held_inputs(
            |key| unsafe {
                ffi::CGEventSourceKeyState(
                    core_graphics::event_source::CGEventSourceStateID::HIDSystemState,
                    key,
                )
            },
            |button| unsafe {
                ffi::CGEventSourceButtonState(
                    core_graphics::event_source::CGEventSourceStateID::HIDSystemState,
                    button,
                )
            },
        );
        Ok(())
    }

    fn emit_held_inputs(
        &self,
        mut key_down: impl FnMut(u16) -> bool,
        mut button_down: impl FnMut(u32) -> bool,
    ) {
        let mut keys = self.keys.lock();
        keys.held.retain(|code| {
            *code != keymap::ev::KEY_CAPSLOCK
                && keymap::evdev_to_mac(*code).is_some_and(&mut key_down)
        });
        keys.chord_active = !self.panic_chord.is_empty()
            && self.panic_chord.iter().all(|code| keys.held.contains(code));
        let shortcut = self.switch.lock();
        for event in crate::keymap::held_key_presses(
            keys.held
                .iter()
                .copied()
                .filter(|code| !shortcut.suppressed(Stream::Desktop, *code as u16)),
        ) {
            self.emit(CaptureEvent::Input(event));
        }
        for number in (0..32).filter(|number| button_down(*number)) {
            let button = match number {
                0 => PointerButton::Left,
                1 => PointerButton::Right,
                number => other_button(i64::from(number)),
            };
            self.emit(CaptureEvent::Input(InputEvent::Button {
                button,
                pressed: true,
            }));
        }
    }

    pub fn end(&self, warp_to: Option<CGPoint>) {
        self.finish(warp_to, false);
    }

    fn finish(&self, warp_to: Option<CGPoint>, switching: bool) {
        let _guard = self.capture_lock.lock();
        self.finish_locked(warp_to, switching);
    }

    fn finish_locked(&self, warp_to: Option<CGPoint>, switching: bool) {
        self.raw_switching.store(switching, Ordering::SeqCst);
        self.raw_enabled.store(false, Ordering::SeqCst);
        if !self.capturing.swap(false, Ordering::SeqCst) {
            return;
        }
        cursor::end(warp_to);
        self.last_end_ms.store(cursor::now_ms(), Ordering::SeqCst);
        *self.contact.lock() = None;
    }

    pub fn switch_target(&self, stream: Stream) -> bool {
        let _guard = self.capture_lock.lock();
        self.switch_target_locked(stream)
    }

    fn switch_target_locked(&self, stream: Stream) -> bool {
        if !self.switch.lock().press(stream, cursor::now_ms()) {
            return false;
        }
        self.finish_locked(None, true);
        self.shared.emit(PlatformEvent::SwitchTarget);
        true
    }

    pub fn shortcut_suppressed(&self, code: u16) -> bool {
        self.switch.lock().suppressed(Stream::Hid, code)
    }

    pub fn release_shortcut_key(&self, code: u16) {
        self.switch.lock().release(Stream::Hid, code);
    }

    pub fn touches_edge(&self, id: u32) -> bool {
        *self.contact.lock() == Some(id)
    }

    pub fn available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
            && self.session_active.load(Ordering::SeqCst)
            && !self.secure_input.load(Ordering::SeqCst)
            && !self.need_recreate.load(Ordering::SeqCst)
    }

    fn session_changed(&self, active: bool) {
        let _guard = self.capture_lock.lock();
        self.session_active.store(active, Ordering::SeqCst);
        self.available.store(false, Ordering::SeqCst);
        self.invalidate_input_locked("macOS sleep or login session changed; input released");
        self.need_recreate.store(true, Ordering::SeqCst);
    }

    fn invalidate_input_locked(&self, reason: &str) {
        self.finish_locked(None, false);
        *self.keys.lock() = KeyState::default();
        self.lifecycle.fetch_add(1, Ordering::SeqCst);
        self.emit(CaptureEvent::Broken {
            reason: reason.into(),
        });
    }

    fn secure_input_changed(&self, active: bool) {
        let _guard = self.capture_lock.lock();
        if self.secure_input.swap(active, Ordering::SeqCst) != active {
            if active {
                self.invalidate_input_locked("macOS Secure Input enabled; input released");
            } else {
                *self.keys.lock() = KeyState::default();
            }
        }
    }

    pub fn panic_from_hid(&self, held: &std::collections::BTreeSet<u16>) -> bool {
        if self.panic_chord.is_empty()
            || !self
                .panic_chord
                .iter()
                .all(|code| held.contains(&(*code as u16)))
        {
            return false;
        }
        self.end(None);
        self.emit(CaptureEvent::Panic);
        true
    }

    fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::SeqCst)
    }

    /// True for a short window after a capture ends, so the warp-back event that lands
    /// just inside the edge cannot immediately re-trigger a crossing.
    fn in_cross_cooldown(&self) -> bool {
        cursor::now_ms().saturating_sub(self.last_end_ms.load(Ordering::SeqCst)) < CROSS_COOLDOWN_MS
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
    let _wake_observers = install_wake_observers(st.clone());
    let mut live: Option<LiveTap> = None;
    let mut last_health = Instant::now();
    // Report Secure Input on the first pass rather than 2 s in.
    let mut last_secure = Instant::now()
        .checked_sub(SECURE_INPUT_POLL)
        .unwrap_or_else(Instant::now);

    while !st.shared.tx.is_closed() {
        if live.is_none() || st.need_recreate.swap(false, Ordering::SeqCst) {
            st.available.store(false, Ordering::SeqCst);
            st.port.store(std::ptr::null_mut(), Ordering::SeqCst);
            live = None;
            match create_tap(&st) {
                Some(t) => {
                    st.port.store(
                        t.tap.mach_port().as_concrete_TypeRef() as *mut c_void,
                        Ordering::SeqCst,
                    );
                    t.tap.enable();
                    st.available.store(true, Ordering::SeqCst);
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
        let port = st.port.load(Ordering::SeqCst) as CFMachPortRef;
        st.available.store(
            !port.is_null() && unsafe { ffi::CGEventTapIsEnabled(port) },
            Ordering::SeqCst,
        );
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
    st.available.store(false, Ordering::SeqCst);
    st.port.store(std::ptr::null_mut(), Ordering::SeqCst);
    st.end(None);
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
    let _guard = st.capture_lock.lock();

    match etype {
        CGEventType::TapDisabledByTimeout => {
            st.available.store(false, Ordering::SeqCst);
            st.invalidate_input_locked("macOS event tap timed out; input released");
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
            st.available.store(false, Ordering::SeqCst);
            st.invalidate_input_locked("event tap disabled by user input; input released");
            st.need_recreate.store(true, Ordering::SeqCst);
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

    let suppressed = matches!(
        etype,
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged
    ) && keymap::mac_to_evdev(
        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16,
    )
    .is_some_and(|code| st.switch.lock().suppressed(Stream::Desktop, code as u16));
    let key_edge = key_edge_of(etype, event);
    if let Some((code, pressed)) = key_edge {
        if !pressed {
            st.switch.lock().release(Stream::Desktop, code as u16);
        }
        if st.track_key(code, pressed) {
            // Panic must work with the network wedged: restore locally first, report after.
            st.finish_locked(None, false);
            st.emit(CaptureEvent::Panic);
            return CallbackResult::Drop;
        }
    }

    if key_edge == Some((88, true)) {
        let keys = st.keys.lock();
        let switch = keys.held.contains(&29) && keys.held.contains(&56);
        drop(keys);
        if switch {
            st.switch_target_locked(Stream::Desktop);
            return CallbackResult::Drop;
        }
    }
    if suppressed {
        return CallbackResult::Drop;
    }

    if !capturing {
        if let CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged = etype
        {
            if st.in_cross_cooldown() {
                *st.contact.lock() = None;
                return CallbackResult::Keep;
            }
            let loc = event.location();
            match st.edge_hit(loc) {
                Some((edge_id, along)) => {
                    let mut contact = st.contact.lock();
                    if *contact != Some(edge_id) {
                        *contact = Some(edge_id);
                        drop(contact);
                        st.emit(CaptureEvent::EdgeHit { edge_id, along });
                    } else {
                        drop(contact);
                        st.emit(CaptureEvent::EdgeMotion {
                            edge_id,
                            along,
                            dx: event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X),
                            dy: event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y),
                        });
                    }
                }
                None => {
                    if st.contact.lock().take().is_some() {
                        st.emit(CaptureEvent::EdgeLeft);
                    }
                }
            }
        }
        return CallbackResult::Keep;
    }

    if let CGEventType::MouseMoved
    | CGEventType::LeftMouseDragged
    | CGEventType::RightMouseDragged
    | CGEventType::OtherMouseDragged = etype
    {
        cursor::reassert(event.location());
    }
    if !st.raw_enabled.load(Ordering::SeqCst) {
        translate(etype, event, key_edge).emit(st);
    }
    CallbackResult::Drop
}

/// Key press/release edges, with autorepeat filtered (DESIGN 16: repeats are regenerated at
/// the destination). `None` for non-keyboard events and for unmapped keycodes.
fn key_edge_of(etype: CGEventType, event: &CGEvent) -> Option<(u32, bool)> {
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
        CGEventType::FlagsChanged => {
            let (code, mask) = match vk {
                54 => (keymap::ev::KEY_RIGHTMETA, 0x10),
                55 => (keymap::ev::KEY_LEFTMETA, 0x08),
                56 => (keymap::ev::KEY_LEFTSHIFT, 0x02),
                58 => (keymap::ev::KEY_LEFTALT, 0x20),
                59 => (keymap::ev::KEY_LEFTCTRL, 0x01),
                60 => (keymap::ev::KEY_RIGHTSHIFT, 0x04),
                61 => (keymap::ev::KEY_RIGHTALT, 0x40),
                62 => (keymap::ev::KEY_RIGHTCTRL, 0x2000),
                _ => return None,
            };
            Some((code, event.get_flags().bits() & mask != 0))
        }
        _ => None,
    }
}

enum Translated {
    None,
    One(InputEvent),
    Two(InputEvent, InputEvent),
}

impl Translated {
    fn emit(self, st: &TapState) {
        match self {
            Self::None => {}
            Self::One(event) => st.emit(CaptureEvent::Input(event)),
            Self::Two(first, second) => {
                st.emit(CaptureEvent::Input(first));
                st.emit(CaptureEvent::Input(second));
            }
        }
    }
}

fn translate(etype: CGEventType, event: &CGEvent, key_edge: Option<(u32, bool)>) -> Translated {
    match etype {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            // Post-acceleration deltas in points — exactly what the wire wants (DESIGN 10).
            let dx = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
            let dy = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);
            if dx == 0.0 && dy == 0.0 {
                Translated::None
            } else {
                Translated::One(InputEvent::Motion { dx, dy })
            }
        }
        CGEventType::LeftMouseDown
        | CGEventType::LeftMouseUp
        | CGEventType::RightMouseDown
        | CGEventType::RightMouseUp
        | CGEventType::OtherMouseDown
        | CGEventType::OtherMouseUp => {
            let (button, pressed) = button_edge(etype, event).expect("mouse button event");
            Translated::One(InputEvent::Button { button, pressed })
        }
        CGEventType::ScrollWheel => scroll(event),
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged => {
            let Some((code, pressed)) = key_edge else {
                return Translated::None;
            };
            // DESIGN keymap note: CapsLock is a lock state, not an edge — never forwarded.
            if code == keymap::ev::KEY_CAPSLOCK {
                return Translated::None;
            }
            Translated::One(InputEvent::Key { code, pressed })
        }
        _ => Translated::None,
    }
}

fn button_edge(etype: CGEventType, event: &CGEvent) -> Option<(PointerButton, bool)> {
    match etype {
        CGEventType::LeftMouseDown => Some((PointerButton::Left, true)),
        CGEventType::LeftMouseUp => Some((PointerButton::Left, false)),
        CGEventType::RightMouseDown => Some((PointerButton::Right, true)),
        CGEventType::RightMouseUp => Some((PointerButton::Right, false)),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => Some((
            other_button(event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER)),
            matches!(etype, CGEventType::OtherMouseDown),
        )),
        _ => None,
    }
}

fn other_button(n: i64) -> PointerButton {
    match n {
        2 => PointerButton::Middle,
        3 => PointerButton::Back,
        4 => PointerButton::Forward,
        n => PointerButton::Other(n.clamp(0, 255) as u8),
    }
}

fn scroll(event: &CGEvent) -> Translated {
    // Momentum is the target's job (DESIGN 15); forward only finger-driven scroll.
    if event.get_integer_value_field(ffi::FIELD_SCROLL_MOMENTUM_PHASE) != 0 {
        return Translated::None;
    }
    let phase = event.get_integer_value_field(ffi::FIELD_SCROLL_PHASE);

    if event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
        let dy = event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
        let dx = event.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2);
        let motion = (dx != 0.0 || dy != 0.0).then_some(crate::scroll::mac_pixels_to_wire(dx, dy));
        let stopped = phase == ffi::SCROLL_PHASE_ENDED || phase == ffi::SCROLL_PHASE_CANCELLED;
        let stop = stopped.then_some(InputEvent::ScrollStop {
            cancel: phase == ffi::SCROLL_PHASE_CANCELLED,
        });
        match (motion, stop) {
            (Some(first), Some(second)) => Translated::Two(first, second),
            (Some(event), None) | (None, Some(event)) => Translated::One(event),
            (None, None) => Translated::None,
        }
    } else {
        let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
        let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
        if dx == 0 && dy == 0 {
            return Translated::None;
        }
        Translated::One(crate::scroll::mac_lines_to_wire(dx, dy))
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
            st.emit(CaptureEvent::Broken {
                reason: "Accessibility permission revoked".into(),
            });
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
        CGEventTapOptions::Default,
        vec![CGEventType::MouseMoved],
        |_, _, _| CallbackResult::Keep,
    )
    .is_ok()
}

fn poll_secure_input(st: &Arc<TapState>) {
    let status = super::secure_input_status();
    st.secure_input_changed(status.is_some());
    st.shared.set_health(|h| h.secure_input = status.clone());
}

/// Taps die across sleep/wake and lock/unlock and never come back on their own.
struct WakeObservers {
    center: objc2::rc::Retained<objc2_foundation::NSNotificationCenter>,
    tokens: Vec<objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>>,
}

impl Drop for WakeObservers {
    fn drop(&mut self) {
        for token in &self.tokens {
            unsafe { self.center.removeObserver((**token).as_ref()) };
        }
    }
}

fn install_wake_observers(st: Arc<TapState>) -> WakeObservers {
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceSessionDidBecomeActiveNotification,
        NSWorkspaceSessionDidResignActiveNotification, NSWorkspaceWillSleepNotification,
    };
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let mut tokens = Vec::new();
    for (name, active) in [
        (unsafe { NSWorkspaceDidWakeNotification }, true),
        (
            unsafe { NSWorkspaceSessionDidBecomeActiveNotification },
            true,
        ),
        (unsafe { NSWorkspaceWillSleepNotification }, false),
        (
            unsafe { NSWorkspaceSessionDidResignActiveNotification },
            false,
        ),
    ] {
        let st = st.clone();
        let block = block2::RcBlock::new(
            move |_n: std::ptr::NonNull<objc2_foundation::NSNotification>| {
                tracing::info!("system wake/session change; recreating the event tap");
                st.session_changed(active);
            },
        );
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        tokens.push(token);
    }
    WakeObservers { center, tokens }
}

/// `warp_to` in engine coords is already CG global points on macOS.
pub fn warp_point(v: Vec2) -> CGPoint {
    CGPoint::new(v.x, v.y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PlatformEvent;

    fn flags_event(vk: u16, flags: u64) -> CGEvent {
        let source = core_graphics::event_source::CGEventSource::new(
            core_graphics::event_source::CGEventSourceStateID::Private,
        ).unwrap();
        let event = CGEvent::new_keyboard_event(source, vk, false).unwrap();
        event.set_type(CGEventType::FlagsChanged);
        event.set_flags(core_graphics::event::CGEventFlags::from_bits_retain(flags));
        event
    }

    #[test]
    fn modifier_notifications_cannot_invent_an_a_press() {
        let st = state();
        let event = flags_event(0, 0);
        on_event(&st, CGEventType::FlagsChanged, &event);
        assert!(st.keys.lock().held.is_empty());
    }

    #[test]
    fn modifier_release_without_a_press_cannot_hold_shift() {
        let st = state();
        let event = flags_event(56, 0);
        on_event(&st, CGEventType::FlagsChanged, &event);
        on_event(&st, CGEventType::FlagsChanged, &event);
        assert!(!st.keys.lock().held.contains(&42));
        assert_eq!(key_edge_of(CGEventType::FlagsChanged, &event), Some((42, false)));
    }

    #[test]
    fn releasing_one_shift_preserves_the_other_shift() {
        let st = state();
        for (vk, flags) in [(56, 0x20002), (60, 0x20006), (56, 0x20004)] {
            on_event(&st, CGEventType::FlagsChanged, &flags_event(vk, flags));
        }
        assert_eq!(st.keys.lock().held, HashSet::from([54]));
        on_event(&st, CGEventType::FlagsChanged, &flags_event(56, 0x20004));
        assert_eq!(st.keys.lock().held, HashSet::from([54]));
    }

    #[test]
    fn desktop_handoff_never_replays_caps_lock_as_a_press() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(MacShared {
            tx,
            displays: RwLock::new(Vec::new()),
            health: Mutex::new(Default::default()),
        });
        let st = TapState::new(shared, Vec::new());
        on_event(&st, CGEventType::FlagsChanged, &flags_event(57, 0x10000));
        while rx.try_recv().is_ok() {}
        st.emit_held_inputs(|_| true, |_| false);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn tap_timeout_releases_desktop_input_and_clears_remembered_keys() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(MacShared {
            tx,
            displays: RwLock::new(Vec::new()),
            health: Mutex::new(Default::default()),
        });
        let st = TapState::new(shared, Vec::new());
        st.keys.lock().held.extend([30, 42]);
        let event = flags_event(0, 0);
        on_event(&st, CGEventType::TapDisabledByTimeout, &event);
        assert!(st.keys.lock().held.is_empty());
        assert!(matches!(rx.try_recv().unwrap(), PlatformEvent::Capture(CaptureEvent::Broken { .. })));
    }

    #[test]
    fn secure_input_transitions_clear_keys_and_prevent_capture() {
        let st = state();
        st.available.store(true, Ordering::SeqCst);
        st.keys.lock().held.extend([30, 42]);
        st.secure_input_changed(true);
        assert!(st.keys.lock().held.is_empty());
        assert!(!st.available());
        assert!(st.begin().is_err());
        assert!(!st.is_capturing());
        st.keys.lock().held.insert(54);
        st.secure_input_changed(false);
        assert!(st.keys.lock().held.is_empty());
        assert!(st.available());
    }

    #[test]
    fn unavailable_tap_cannot_capture_local_input() {
        let st = state();
        assert!(st.begin().is_err());
        assert!(!st.is_capturing());
    }

    #[test]
    fn desktop_handoff_prunes_keys_released_while_callbacks_were_unavailable() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(MacShared {
            tx,
            displays: RwLock::new(Vec::new()),
            health: Mutex::new(Default::default()),
        });
        let st = TapState::new(shared, Vec::new());
        st.keys.lock().held.extend([30, 42]);
        st.emit_held_inputs(|_| false, |_| false);
        assert!(st.keys.lock().held.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn all_modifier_edges_follow_their_device_flags() {
        for (vk, code, mask) in [
            (54, 126, 0x10), (55, 125, 0x08), (56, 42, 0x02), (58, 56, 0x20),
            (59, 29, 0x01), (60, 54, 0x04), (61, 100, 0x40), (62, 97, 0x2000),
        ] {
            assert_eq!(key_edge_of(CGEventType::FlagsChanged, &flags_event(vk, mask)), Some((code, true)));
            assert_eq!(key_edge_of(CGEventType::FlagsChanged, &flags_event(vk, 0)), Some((code, false)));
        }
    }

    fn state() -> Arc<TapState> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<PlatformEvent>();
        let shared = Arc::new(MacShared {
            tx,
            displays: RwLock::new(Vec::new()),
            health: Mutex::new(Default::default()),
        });
        TapState::new(shared, Vec::new())
    }

    #[test]
    fn closed_platform_stops_tap_thread_and_releases_notification_observers() {
        let st = state();
        let weak = Arc::downgrade(&st);
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            run(st);
            done.send(()).unwrap();
        });
        completed.recv_timeout(Duration::from_secs(2))
            .expect("event tap survived the platform receiver");
        worker.join().unwrap();
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn cross_cooldown_mutes_edges_only_briefly() {
        let st = state();
        assert!(
            !st.in_cross_cooldown(),
            "a fresh tap must not mute edge tests"
        );
        st.last_end_ms.store(cursor::now_ms(), Ordering::SeqCst);
        assert!(
            st.in_cross_cooldown(),
            "edges are muted right after a capture ends"
        );
        st.last_end_ms.store(
            cursor::now_ms().saturating_sub(CROSS_COOLDOWN_MS + 5),
            Ordering::SeqCst,
        );
        assert!(
            !st.in_cross_cooldown(),
            "muting lifts once the cooldown elapses"
        );
    }

    #[test]
    fn set_edges_are_deduplicated() {
        let st = state();
        let edge = EdgeSpec {
            id: 0,
            side: EdgeSide::Right,
            at: 1920,
            from: 0,
            to: 1080,
        };
        st.set_edges(vec![edge.clone()]);
        *st.contact.lock() = Some(0);
        st.set_edges(vec![edge]);
        assert_eq!(
            *st.contact.lock(),
            Some(0),
            "an unchanged edge set keeps live contact"
        );
        st.set_edges(vec![EdgeSpec {
            id: 0,
            side: EdgeSide::Left,
            at: 0,
            from: 0,
            to: 1080,
        }]);
        assert_eq!(
            *st.contact.lock(),
            None,
            "a real edge change resets contact"
        );
    }

    #[test]
    fn desktop_handoff_queries_current_buttons_even_without_prior_callbacks() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(MacShared {
            tx,
            displays: RwLock::new(Vec::new()),
            health: Mutex::new(Default::default()),
        });
        let st = TapState::new(shared, Vec::new());
        st.emit_held_inputs(|_| false, |number| matches!(number, 0 | 4));
        let mut buttons = HashSet::new();
        while let Ok(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Button {
            button,
            pressed: true,
        }))) = rx.try_recv()
        {
            buttons.insert(button);
        }
        assert_eq!(
            buttons,
            [PointerButton::Left, PointerButton::Forward].into()
        );
        st.emit_held_inputs(|_| false, |_| false);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn session_changes_release_raw_input_and_invalidate_held_keys_before_recreation() {
        let st = state();
        st.raw_enabled.store(true, Ordering::SeqCst);
        st.available.store(true, Ordering::SeqCst);
        st.keys.lock().held.insert(42);
        st.session_changed(false);
        assert!(!st.raw_enabled.load(Ordering::SeqCst));
        assert!(!st.available());
        assert!(st.keys.lock().held.is_empty());
        assert_eq!(st.lifecycle.load(Ordering::SeqCst), 1);
        st.session_changed(true);
        assert!(!st.available());
        assert_eq!(st.lifecycle.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn hid_emergency_chord_releases_without_an_engine_or_desktop_callback() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(MacShared {
            tx,
            displays: RwLock::new(Vec::new()),
            health: Mutex::new(Default::default()),
        });
        let st = TapState::new(shared, vec![42, 54, 1]);
        st.raw_enabled.store(true, Ordering::SeqCst);
        assert!(!st.panic_from_hid(&[42, 54].into_iter().collect()));
        assert!(st.raw_enabled.load(Ordering::SeqCst));
        assert!(st.panic_from_hid(&[42, 54, 1].into_iter().collect()));
        assert!(!st.raw_enabled.load(Ordering::SeqCst));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PlatformEvent::Capture(CaptureEvent::Panic)
        ));
    }
}
