//! `Emulate` via `CGEventPost`.
//!
//! Everything here goes through one reused `kCGEventSourceStateHIDSystemState` source and
//! carries the Splice magic in field 42, so our own capture tap can tell these apart from
//! physical input.

use super::ffi::{self, Event};
use super::MacShared;
use crate::keymap::{self, ev};
use crate::{Emulate, Result};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_graphics::event::{CGEventFlags, CGEventType, EventField};
use core_graphics::event_source::CGEventSourceStateID;
use core_graphics::geometry::CGPoint;
use core_graphics::sys::CGEventSourceRef;
use objc2_app_kit::NSEvent;
use parking_lot::Mutex;
use splice_proto::{DisplayRect, InputEvent, PointerButton, Vec2};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

const KEEP_AWAKE_REDECLARE: Duration = Duration::from_secs(30);
/// Line-unit scroll events must stay small; larger scrolls are chunked.
const MAX_LINES_PER_EVENT: i32 = 10;
const DETENT: f64 = 120.0;

const DEFAULT_REPEAT_DELAY: f64 = 0.5;
const DEFAULT_REPEAT_INTERVAL: f64 = 0.03;

/// `CGEventSourceRef` is a CF object; CF objects are safe to use from any thread as long as
/// we serialize our own mutable state, which `Core::state` does.
struct EventSource(CGEventSourceRef);
unsafe impl Send for EventSource {}
unsafe impl Sync for EventSource {}

struct State {
    pos: CGPoint,
    flags: CGEventFlags,
    held_keys: HashSet<u32>,
    held_buttons: HashSet<u32>,
    last_press: Option<(u32, u64, CGPoint)>,
    click_state: i64,
    event_number: i64,
    /// Value-120 remainder that hasn't reached a whole detent yet.
    scroll_residue: (f64, f64),
    assertion: Option<ffi::IOPMAssertionID>,
}

struct Core {
    shared: Arc<MacShared>,
    src: EventSource,
    state: Mutex<State>,
    double_click_interval_ms: u64,
}

pub struct Injector {
    core: Arc<Core>,
    runtime: tokio::runtime::Handle,
    tasks: Mutex<Tasks>,
}

#[derive(Default)]
struct Tasks {
    repeat: Option<JoinHandle<()>>,
    keep_awake: Option<JoinHandle<()>>,
}

impl Injector {
    pub fn new(shared: Arc<MacShared>) -> crate::Result<Self> {
        let src = unsafe { ffi::CGEventSourceCreate(CGEventSourceStateID::HIDSystemState) };
        if src.is_null() {
            return Err(crate::PlatformError::Unavailable(
                "could not create a CGEventSource (missing Accessibility permission?)".into(),
            ));
        }
        let seed = unsafe {
            ffi::CGEventSourceCounterForEventType(
                CGEventSourceStateID::HIDSystemState,
                CGEventType::LeftMouseDown as u32,
            )
        } as i64;
        let pos = shared
            .displays
            .read()
            .first()
            .map(|d| CGPoint::new(d.x as f64 + d.w as f64 / 2.0, d.y as f64 + d.h as f64 / 2.0))
            .unwrap_or(CGPoint::new(0.0, 0.0));
        let core = Core {
            shared,
            src: EventSource(src),
            double_click_interval_ms: (NSEvent::doubleClickInterval() * 1000.0).max(1.0)
                as u64,
            state: Mutex::new(State {
                pos,
                flags: CGEventFlags::CGEventFlagNull,
                held_keys: HashSet::new(),
                held_buttons: HashSet::new(),
                last_press: None,
                click_state: 1,
                event_number: seed,
                scroll_residue: (0.0, 0.0),
                assertion: None,
            }),
        };
        Ok(Self {
            core: Arc::new(core),
            runtime: tokio::runtime::Handle::current(),
            tasks: Mutex::new(Tasks::default()),
        })
    }

    fn cancel_repeat(&self) {
        if let Some(h) = self.tasks.lock().repeat.take() {
            h.abort();
        }
    }

    /// Injected CGEvents never auto-repeat, so the target regenerates them (DESIGN 16).
    fn start_repeat(&self, code: u32) {
        let (delay, interval) = repeat_timings();
        let core = self.core.clone();
        let handle = self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            let mut ticker = tokio::time::interval(Duration::from_secs_f64(interval));
            loop {
                ticker.tick().await;
                if !core.repeat_key(code) {
                    break;
                }
            }
        });
        self.tasks.lock().repeat = Some(handle);
    }
}

fn repeat_timings() -> (f64, f64) {
    let delay = NSEvent::keyRepeatDelay();
    let interval = NSEvent::keyRepeatInterval();
    let delay = if delay.is_finite() && delay > 0.0 { delay } else { DEFAULT_REPEAT_DELAY };
    let interval = if interval.is_finite() && interval > 0.0 {
        interval
    } else {
        DEFAULT_REPEAT_INTERVAL
    };
    (delay.clamp(0.1, 2.0), interval.clamp(0.01, 0.5))
}

impl Core {
    fn mouse(&self, etype: CGEventType, pos: CGPoint, button: u32) -> Option<Event> {
        Event::new(unsafe { ffi::CGEventCreateMouseEvent(self.src.0, etype, pos, button) })
    }

    fn key(&self, vk: u16, down: bool) -> Option<Event> {
        Event::new(unsafe { ffi::CGEventCreateKeyboardEvent(self.src.0, vk, down) })
    }

    fn scroll(&self, unit: u32, vertical: i32, horizontal: i32) -> Option<Event> {
        Event::new(unsafe {
            ffi::CGEventCreateScrollWheelEvent2(self.src.0, unit, 2, vertical, horizontal, 0)
        })
    }

    /// Cumulative modifier flags recomputed from the held ledger. Never derived from the
    /// previous event — a dropped key-up would otherwise stick a modifier forever.
    fn flags_for(&self, held: &HashSet<u32>) -> CGEventFlags {
        let mut flags = CGEventFlags::CGEventFlagNull;
        for code in held {
            flags |= match *code {
                ev::KEY_LEFTSHIFT | ev::KEY_RIGHTSHIFT => CGEventFlags::CGEventFlagShift,
                ev::KEY_LEFTCTRL | ev::KEY_RIGHTCTRL => CGEventFlags::CGEventFlagControl,
                ev::KEY_LEFTALT | ev::KEY_RIGHTALT => CGEventFlags::CGEventFlagAlternate,
                ev::KEY_LEFTMETA | ev::KEY_RIGHTMETA => CGEventFlags::CGEventFlagCommand,
                _ => CGEventFlags::CGEventFlagNull,
            };
        }
        flags
    }

    fn motion_type(&self, held_buttons: &HashSet<u32>) -> (CGEventType, u32) {
        if held_buttons.contains(&0) {
            (CGEventType::LeftMouseDragged, 0)
        } else if held_buttons.contains(&1) {
            (CGEventType::RightMouseDragged, 1)
        } else if let Some(b) = held_buttons.iter().copied().min() {
            (CGEventType::OtherMouseDragged, b)
        } else {
            (CGEventType::MouseMoved, 0)
        }
    }

    fn post_motion(&self, st: &mut State, dx: f64, dy: f64) {
        st.pos = {
            let displays = self.shared.displays.read();
            clamp_to_displays(CGPoint::new(st.pos.x + dx, st.pos.y + dy), &displays)
        };
        let (etype, button) = self.motion_type(&st.held_buttons);
        let Some(ev) = self.mouse(etype, st.pos, button) else {
            return;
        };
        ev.set_flags(st.flags);
        // Games read the delta fields rather than differencing positions.
        ev.set_double(EventField::MOUSE_EVENT_DELTA_X, dx);
        ev.set_double(EventField::MOUSE_EVENT_DELTA_Y, dy);
        ev.post();
    }

    fn post_button(&self, st: &mut State, button: u32, pressed: bool) {
        let etype = match (button, pressed) {
            (0, true) => CGEventType::LeftMouseDown,
            (0, false) => CGEventType::LeftMouseUp,
            (1, true) => CGEventType::RightMouseDown,
            (1, false) => CGEventType::RightMouseUp,
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        };
        if pressed {
            let now = super::cursor::now_ms();
            st.click_state = match st.last_press {
                Some((b, t, p))
                    if b == button
                        && now.saturating_sub(t) <= self.double_click_interval_ms
                        && (p.x - st.pos.x).abs() < 4.0
                        && (p.y - st.pos.y).abs() < 4.0 =>
                {
                    (st.click_state + 1).min(3)
                }
                _ => 1,
            };
            st.last_press = Some((button, now, st.pos));
            st.event_number += 1;
            st.held_buttons.insert(button);
        } else {
            st.held_buttons.remove(&button);
        }
        let Some(ev) = self.mouse(etype, st.pos, button) else {
            return;
        };
        ev.set_flags(st.flags);
        ev.set_int(EventField::MOUSE_EVENT_BUTTON_NUMBER, button as i64);
        ev.set_int(EventField::MOUSE_EVENT_CLICK_STATE, st.click_state);
        ev.set_int(EventField::MOUSE_EVENT_NUMBER, st.event_number);
        ev.post();
    }

    /// Posts a key edge. Modifiers go out as `FlagsChanged` carrying their own virtual
    /// keycode (keycode 0 there is lan-mouse's phantom-'A' bug).
    fn post_key(&self, st: &mut State, code: u32, pressed: bool, autorepeat: bool) {
        let Some(vk) = keymap::evdev_to_mac(code) else {
            tracing::debug!(evdev = code, "dropping key with no macOS equivalent");
            return;
        };
        if pressed {
            st.held_keys.insert(code);
        } else {
            st.held_keys.remove(&code);
        }
        let is_mod = keymap::is_modifier(code);
        if is_mod {
            st.flags = self.flags_for(&st.held_keys);
        }
        let Some(ev) = self.key(vk, pressed) else {
            return;
        };
        let mut flags = st.flags;
        if is_mod {
            unsafe { ffi::CGEventSetType(ev.0, CGEventType::FlagsChanged) };
        } else if keymap::is_nav_key(code) {
            flags |= CGEventFlags::CGEventFlagNumericPad | CGEventFlags::CGEventFlagSecondaryFn;
        }
        ev.set_flags(flags);
        ev.set_int(EventField::KEYBOARD_EVENT_AUTOREPEAT, autorepeat as i64);
        ev.post();
    }

    /// Returns false once the key is no longer held, ending the repeat task.
    fn repeat_key(&self, code: u32) -> bool {
        let st = self.state.lock();
        if !st.held_keys.contains(&code) {
            return false;
        }
        let Some(vk) = keymap::evdev_to_mac(code) else {
            return false;
        };
        let Some(ev) = self.key(vk, true) else {
            return false;
        };
        let mut flags = st.flags;
        if keymap::is_nav_key(code) {
            flags |= CGEventFlags::CGEventFlagNumericPad | CGEventFlags::CGEventFlagSecondaryFn;
        }
        ev.set_flags(flags);
        ev.set_int(EventField::KEYBOARD_EVENT_AUTOREPEAT, 1);
        ev.post();
        true
    }

    fn post_scroll_lines(&self, st: &mut State, dx120: i32, dy120: i32) {
        let (dx, dy) = crate::scroll::wire_pixels_to_mac(dx120 as f64, dy120 as f64);
        st.scroll_residue.0 += dx;
        st.scroll_residue.1 += dy;
        // std::trunc semantics, not floor: -119/120 must round toward zero, not to -1.
        let mut h = (st.scroll_residue.0 / DETENT).trunc() as i32;
        let mut v = (st.scroll_residue.1 / DETENT).trunc() as i32;
        st.scroll_residue.0 -= h as f64 * DETENT;
        st.scroll_residue.1 -= v as f64 * DETENT;
        while h != 0 || v != 0 {
            let hc = h.clamp(-MAX_LINES_PER_EVENT, MAX_LINES_PER_EVENT);
            let vc = v.clamp(-MAX_LINES_PER_EVENT, MAX_LINES_PER_EVENT);
            h -= hc;
            v -= vc;
            if let Some(ev) = self.scroll(ffi::kCGScrollEventUnitLine, vc, hc) {
                ev.set_flags(st.flags);
                ev.post();
            }
        }
    }

    fn post_scroll_pixels(&self, st: &mut State, dx: f64, dy: f64) {
        let (dx, dy) = crate::scroll::wire_pixels_to_mac(dx, dy);
        let Some(ev) = self.scroll(
            ffi::kCGScrollEventUnitPixel,
            dy.round() as i32,
            dx.round() as i32,
        ) else {
            return;
        };
        ev.set_int(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 1);
        ev.set_flags(st.flags);
        ev.post();
    }

    fn release_everything(&self) {
        let mut st = self.state.lock();
        let keys: Vec<u32> = st.held_keys.iter().copied().collect();
        for code in keys {
            self.post_key(&mut st, code, false, false);
        }
        let buttons: Vec<u32> = st.held_buttons.iter().copied().collect();
        for button in buttons {
            self.post_button(&mut st, button, false);
        }
        st.held_keys.clear();
        st.held_buttons.clear();
        st.flags = CGEventFlags::CGEventFlagNull;
        st.scroll_residue = (0.0, 0.0);
    }
}

/// Synthesized CGEvents alone do not wake a slept display, so injection has to say so.
fn declare_user_activity() {
    let name = CFString::new("Splice input");
    let mut id: ffi::IOPMAssertionID = 0;
    unsafe {
        ffi::IOPMAssertionDeclareUserActivity(
            name.as_concrete_TypeRef(),
            ffi::kIOPMUserActiveLocal,
            &mut id,
        );
    }
}

fn button_number(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => 0,
        PointerButton::Right => 1,
        PointerButton::Middle => 2,
        PointerButton::Back => 3,
        PointerButton::Forward => 4,
        PointerButton::Other(n) => n as u32,
    }
}

/// Clamps to the nearest point inside some display; non-rectangular arrangements have dead
/// regions inside the bounding box that the cursor must never land in.
pub fn clamp_to_displays(pt: CGPoint, displays: &[DisplayRect]) -> CGPoint {
    if displays.is_empty() {
        return pt;
    }
    let inside = displays.iter().any(|d| {
        pt.x >= d.x as f64
            && pt.x < (d.x + d.w as i32) as f64
            && pt.y >= d.y as f64
            && pt.y < (d.y + d.h as i32) as f64
    });
    if inside {
        return pt;
    }
    let mut best = pt;
    let mut best_dist = f64::MAX;
    for d in displays {
        let x = pt.x.clamp(d.x as f64, (d.x + d.w as i32) as f64 - 1.0);
        let y = pt.y.clamp(d.y as f64, (d.y + d.h as i32) as f64 - 1.0);
        let dist = (x - pt.x).powi(2) + (y - pt.y).powi(2);
        if dist < best_dist {
            best_dist = dist;
            best = CGPoint::new(x, y);
        }
    }
    best
}

#[async_trait::async_trait]
impl Emulate for Injector {
    async fn enter(&self, pos: Vec2) -> Result<()> {
        {
            let displays = self.core.shared.displays.read().clone();
            let mut st = self.core.state.lock();
            st.pos = clamp_to_displays(CGPoint::new(pos.x, pos.y), &displays);
            self.core.post_motion(&mut st, 0.0, 0.0);

            let mut id: ffi::IOPMAssertionID = 0;
            let kind = CFString::new("PreventUserIdleDisplaySleep");
            let name = CFString::new("Splice remote session");
            let rc = unsafe {
                ffi::IOPMAssertionCreateWithName(
                    kind.as_concrete_TypeRef(),
                    ffi::kIOPMAssertionLevelOn,
                    name.as_concrete_TypeRef(),
                    &mut id,
                )
            };
            if rc == 0 {
                st.assertion = Some(id);
            }
        }
        declare_user_activity();

        let mut tasks = self.tasks.lock();
        if let Some(h) = tasks.keep_awake.take() {
            h.abort();
        }
        tasks.keep_awake = Some(self.runtime.spawn(async move {
            let mut ticker = tokio::time::interval(KEEP_AWAKE_REDECLARE);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                declare_user_activity();
            }
        }));
        Ok(())
    }

    async fn inject(&self, event: InputEvent) -> Result<()> {
        match event {
            InputEvent::Motion { dx, dy } => {
                let mut st = self.core.state.lock();
                self.core.post_motion(&mut st, dx, dy);
            }
            InputEvent::Button { button, pressed } => {
                let mut st = self.core.state.lock();
                self.core.post_button(&mut st, button_number(button), pressed);
            }
            InputEvent::ScrollPixels { dx, dy } => {
                let mut st = self.core.state.lock();
                self.core.post_scroll_pixels(&mut st, dx, dy);
            }
            InputEvent::Scroll120 { dx, dy } => {
                let mut st = self.core.state.lock();
                self.core.post_scroll_lines(&mut st, dx, dy);
            }
            // Momentum is the target's business (DESIGN 15); we inject nothing.
            InputEvent::ScrollStop { .. } => {}
            InputEvent::Key { code, pressed } => {
                if code == ev::KEY_CAPSLOCK {
                    // CGEventPost cannot move the CapsLock lock state; forwarding the edge
                    // would desynchronize the two machines (DESIGN keymap note).
                    return Ok(());
                }
                self.cancel_repeat();
                {
                    let mut st = self.core.state.lock();
                    self.core.post_key(&mut st, code, pressed, false);
                }
                if pressed && !keymap::is_modifier(code) && keymap::evdev_to_mac(code).is_some() {
                    self.start_repeat(code);
                }
            }
        }
        Ok(())
    }

    async fn leave(&self) -> Result<()> {
        self.cancel_repeat();
        if let Some(h) = self.tasks.lock().keep_awake.take() {
            h.abort();
        }
        self.core.release_everything();
        let assertion = self.core.state.lock().assertion.take();
        if let Some(id) = assertion {
            unsafe { ffi::IOPMAssertionRelease(id) };
        }
        Ok(())
    }

    async fn release_all(&self) -> Result<()> {
        self.cancel_repeat();
        self.core.release_everything();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(id: &str, x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
        DisplayRect { id: id.into(), x, y, w, h, scale: 1.0 }
    }

    #[test]
    fn clamp_keeps_points_inside_a_display() {
        let displays = vec![rect("1", 0, 0, 1920, 1080)];
        let p = clamp_to_displays(CGPoint::new(100.0, 100.0), &displays);
        assert_eq!((p.x, p.y), (100.0, 100.0));
    }

    #[test]
    fn clamp_pulls_dead_regions_to_the_nearest_display() {
        // Two displays offset vertically leave a dead region below the right one; the
        // bounding box would happily place the cursor there.
        let displays = vec![rect("1", 0, 0, 1000, 1000), rect("2", 1000, -500, 1000, 500)];
        let p = clamp_to_displays(CGPoint::new(1900.0, 200.0), &displays);
        assert_eq!((p.x, p.y), (1900.0, -1.0));
    }

    #[test]
    fn clamp_is_identity_without_displays() {
        let p = clamp_to_displays(CGPoint::new(7.0, 9.0), &[]);
        assert_eq!((p.x, p.y), (7.0, 9.0));
    }
}
