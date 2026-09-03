//! Injection via /dev/uinput: a virtual ABSOLUTE pointer (QEMU-tablet style: ABS_X/ABS_Y
//! over the whole logical layout, mouse buttons, wheel) plus a virtual keyboard.
//!
//! Absolute placement is deliberate (docs/research/linux-native-input.md): libinput
//! applies no acceleration to absolute pointers, so the source's already-accelerated
//! deltas land exactly once and the target position stays in lock-step with the
//! source's virtual cursor. Every compositor maps an unmapped absolute pointer to the
//! bounding box of the whole layout (mutter, KWin, wlroots, Hyprland, niri, Xorg).
//! Keys are raw evdev codes; the compositor applies its own layout. The devices are
//! created once and kept for the process lifetime (udev needs a moment to tag them).

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    RelativeAxisCode, UinputAbsSetup,
};
use parking_lot::Mutex;
use splice_proto::{DisplayRect, InputEvent as WireEvent, PointerButton, Vec2};

use super::screensaver::ScreenSaver;
use super::{Shared, Stop, VIRTUAL_DEVICE_PREFIX};
use crate::{Emulate, PlatformError, Result};

const ABS_MAX: i32 = 65535;
const ABS_RANGE: f64 = 65536.0;
const BTN_LEFT: u16 = 0x110;
const BTN_TASK: u16 = 0x117;
/// Keyboard key codes: everything below the BTN_ block plus the KEY_ range above it
/// (KEY_OK..KEY_MAX), so media/remote keys such as KEY_MICMUTE are accepted.
const KEY_BTN_BLOCK: std::ops::RangeInclusive<u16> = 0x100..=0x15f;
/// BTN_TRIGGER_HAPPY: udev would tag the keyboard as a joystick and libinput ignores it.
const KEY_TRIGGER_HAPPY_BLOCK: std::ops::RangeInclusive<u16> = 0x2c0..=0x2ff;
const KEY_MAX: u16 = 0x2ff;
/// libinput's wheel click angle: 15 logical px of smooth scroll per detent.
const PIXELS_PER_DETENT: f64 = 15.0;
const DETENT: i32 = 120;
const UDEV_SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
const UDEV_SETTLE_POLL: Duration = Duration::from_millis(25);
const VENDOR: u16 = 0x5350;
const PRODUCT_POINTER: u16 = 0x0001;
const PRODUCT_KEYBOARD: u16 = 0x0002;

struct Devices {
    pointer: VirtualDevice,
    keyboard: VirtualDevice,
    shared: Arc<Shared>,
}

impl Devices {
    fn emit_pointer(&mut self, events: &[InputEvent]) -> Result<()> {
        self.note(events);
        emit(&mut self.pointer, events)
    }

    fn emit_keyboard(&mut self, events: &[InputEvent]) -> Result<()> {
        self.note(events);
        emit(&mut self.keyboard, events)
    }

    fn note(&self, events: &[InputEvent]) {
        self.shared.note_injection();
        for event in events {
            if event.event_type() == EventType::KEY {
                self.shared.note_injected_key(u32::from(event.code()), event.value() != 0);
            }
        }
    }
}

struct Ledger {
    entered: bool,
    pos: Vec2,
    /// Last absolute cell written; the kernel drops an ABS value equal to the
    /// previous one, so re-placing the pointer there needs a detour first.
    last_abs: Option<(i32, i32)>,
    held_keys: HashSet<u16>,
    held_buttons: HashSet<u16>,
    /// Hi-res wheel units (120 per detent) still to be emitted, fractional.
    wheel_x: f64,
    wheel_y: f64,
}

impl Default for Ledger {
    fn default() -> Self {
        Ledger {
            entered: false,
            pos: Vec2 { x: 0.0, y: 0.0 },
            last_abs: None,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            wheel_x: 0.0,
            wheel_y: 0.0,
        }
    }
}

fn is_keyboard_code(code: u16) -> bool {
    code != 0
        && code <= KEY_MAX
        && !KEY_BTN_BLOCK.contains(&code)
        && !KEY_TRIGGER_HAPPY_BLOCK.contains(&code)
}

pub struct UinputEmulate {
    shared: Arc<Shared>,
    devices: Mutex<Devices>,
    ledger: Mutex<Ledger>,
    screensaver: Arc<ScreenSaver>,
}

pub async fn create(
    shared: Arc<Shared>,
    conn: Option<zbus::Connection>,
) -> Result<(Arc<dyn Emulate>, Stop)> {
    let devices = tokio::task::spawn_blocking({
        let shared = shared.clone();
        move || open_devices(shared)
    })
    .await
    .map_err(|e| PlatformError::Other(anyhow::anyhow!("uinput setup task: {e}")))??;
    let screensaver = Arc::new(ScreenSaver::new(conn));
    let monitor = tokio::spawn({
        let screensaver = screensaver.clone();
        let shared = shared.clone();
        async move { screensaver.monitor(shared, Arc::new(AtomicBool::new(false))).await }
    });
    let emulate = Arc::new(UinputEmulate {
        shared,
        devices: Mutex::new(devices),
        ledger: Mutex::new(Ledger::default()),
        screensaver,
    });
    let stop = Stop::new({
        let emulate = emulate.clone();
        let monitor = monitor.abort_handle();
        move || {
            monitor.abort();
            let devices = &mut *emulate.devices.lock();
            let ledger = &mut *emulate.ledger.lock();
            release_all_held(devices, ledger);
            ledger.entered = false;
            let screensaver = emulate.screensaver.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { screensaver.uninhibit().await });
            }
        }
    });
    Ok((emulate, stop))
}

fn open_devices(shared: Arc<Shared>) -> Result<Devices> {
    let map_err = |what: &'static str| {
        move |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                PlatformError::Permission(format!(
                    "{what}: /dev/uinput is not accessible; install packaging/linux/70-splice.rules"
                ))
            } else {
                PlatformError::Other(anyhow::anyhow!("{what}: {e}"))
            }
        }
    };

    let mut buttons = AttributeSet::<KeyCode>::new();
    for code in BTN_LEFT..=BTN_TASK {
        buttons.insert(KeyCode::new(code));
    }
    let mut rel = AttributeSet::<RelativeAxisCode>::new();
    rel.insert(RelativeAxisCode::REL_WHEEL);
    rel.insert(RelativeAxisCode::REL_HWHEEL);
    rel.insert(RelativeAxisCode::REL_WHEEL_HI_RES);
    rel.insert(RelativeAxisCode::REL_HWHEEL_HI_RES);
    let axis = AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0);
    let pointer = VirtualDevice::builder()
        .map_err(map_err("open /dev/uinput"))?
        .name(format!("{VIRTUAL_DEVICE_PREFIX} Pointer").as_str())
        .input_id(InputId::new(BusType::BUS_VIRTUAL, VENDOR, PRODUCT_POINTER, 1))
        .with_keys(&buttons)
        .map_err(map_err("pointer buttons"))?
        .with_relative_axes(&rel)
        .map_err(map_err("pointer wheel"))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, axis))
        .map_err(map_err("pointer ABS_X"))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, axis))
        .map_err(map_err("pointer ABS_Y"))?
        .build()
        .map_err(map_err("create virtual pointer"))?;

    let mut keys = AttributeSet::<KeyCode>::new();
    for code in (1..=KEY_MAX).filter(|c| is_keyboard_code(*c)) {
        keys.insert(KeyCode::new(code));
    }
    let keyboard = VirtualDevice::builder()
        .map_err(map_err("open /dev/uinput"))?
        .name(format!("{VIRTUAL_DEVICE_PREFIX} Keyboard").as_str())
        .input_id(InputId::new(BusType::BUS_VIRTUAL, VENDOR, PRODUCT_KEYBOARD, 1))
        .with_keys(&keys)
        .map_err(map_err("keyboard keys"))?
        .build()
        .map_err(map_err("create virtual keyboard"))?;

    let mut devices = Devices { pointer, keyboard, shared };
    wait_for_udev(&mut devices.pointer)?;
    wait_for_udev(&mut devices.keyboard)?;
    Ok(devices)
}

/// udev tags the node after it appears; the compositor only opens it once the udev
/// database entry (`/run/udev/data/c13:N`) exists. Events written before that are
/// lost, so a device that does not settle is a setup failure, not a warning.
fn wait_for_udev(device: &mut VirtualDevice) -> Result<()> {
    let deadline = Instant::now() + UDEV_SETTLE_TIMEOUT;
    let mut db: Option<(String, std::path::PathBuf)> = None;
    while Instant::now() < deadline {
        if db.is_none() {
            let node = device
                .enumerate_dev_nodes_blocking()
                .ok()
                .and_then(|nodes| nodes.into_iter().find_map(|n| n.ok()));
            if let Some(node) = node {
                if let Ok(meta) = std::fs::metadata(&node) {
                    let rdev = meta.rdev();
                    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
                    db = Some((format!("/run/udev/data/c{major}:{minor}"), node));
                }
            }
        }
        if let Some((path, _)) = &db {
            if std::path::Path::new(path).exists() {
                std::thread::sleep(UDEV_SETTLE_POLL * 4);
                return Ok(());
            }
        }
        std::thread::sleep(UDEV_SETTLE_POLL);
    }
    Err(PlatformError::Unavailable(format!(
        "udev did not register the virtual input device {} within {:?}",
        db.map(|(_, n)| n.display().to_string()).unwrap_or_else(|| "(no node)".into()),
        UDEV_SETTLE_TIMEOUT
    )))
}

/// Nearest point inside the union of `displays` (same rule as the engine's
/// `clamp_into_displays`, so source and target agree on the position).
pub fn clamp_to_union(displays: &[DisplayRect], p: Vec2) -> Vec2 {
    let mut nearest: Option<(f64, Vec2)> = None;
    for display in displays {
        if display.w == 0 || display.h == 0 {
            continue;
        }
        let left = f64::from(display.x);
        let right = left + f64::from(display.w);
        let top = f64::from(display.y);
        let bottom = top + f64::from(display.h);
        let candidate = Vec2 {
            x: p.x.clamp(left, right - 1.0),
            y: p.y.clamp(top, bottom - 1.0),
        };
        let d = (candidate.x - p.x).powi(2) + (candidate.y - p.y).powi(2);
        if nearest.is_none_or(|(best, _)| d < best) {
            nearest = Some((d, candidate));
        }
    }
    nearest.map(|(_, c)| c).unwrap_or(p)
}

fn inside_union(displays: &[DisplayRect], p: Vec2) -> bool {
    displays.iter().any(|d| {
        p.x >= f64::from(d.x)
            && p.x < f64::from(d.x) + f64::from(d.w)
            && p.y >= f64::from(d.y)
            && p.y < f64::from(d.y) + f64::from(d.h)
    })
}

/// Logical position → absolute axis values over the layout bounding box, using
/// libinput's `v * width / 65536` mapping so `v` lands on pixel `px`.
pub fn abs_coords(displays: &[DisplayRect], p: Vec2) -> Option<(i32, i32)> {
    let min_x = displays.iter().map(|d| d.x).min()?;
    let min_y = displays.iter().map(|d| d.y).min()?;
    let max_x = displays.iter().map(|d| d.x + d.w as i32).max()?;
    let max_y = displays.iter().map(|d| d.y + d.h as i32).max()?;
    let (w, h) = (f64::from(max_x - min_x), f64::from(max_y - min_y));
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = |v: f64, origin: i32, extent: f64| {
        (((v - f64::from(origin)) + 0.5) * ABS_RANGE / extent).floor().clamp(0.0, ABS_MAX as f64)
            as i32
    };
    Some((scale(p.x, min_x, w), scale(p.y, min_y, h)))
}

fn key_event(code: u16, pressed: bool) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code, pressed as i32)
}

fn button_code(button: PointerButton) -> Option<u16> {
    let code = match button {
        PointerButton::Left => BTN_LEFT,
        PointerButton::Right => BTN_LEFT + 1,
        PointerButton::Middle => BTN_LEFT + 2,
        PointerButton::Back => BTN_LEFT + 3,
        PointerButton::Forward => BTN_LEFT + 4,
        PointerButton::Other(n) => BTN_LEFT + u16::from(n),
    };
    (code <= BTN_TASK).then_some(code)
}

fn emit(device: &mut VirtualDevice, events: &[InputEvent]) -> Result<()> {
    device
        .emit(events)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("uinput write: {e}")))
}

fn abs_frame(x: i32, y: i32) -> [InputEvent; 2] {
    [
        InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, x),
        InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, y),
    ]
}

fn move_to(devices: &mut Devices, ledger: &mut Ledger, displays: &[DisplayRect], next: Vec2) -> Result<()> {
    ledger.pos = if inside_union(displays, next) { next } else { clamp_to_union(displays, next) };
    let Some((x, y)) = abs_coords(displays, ledger.pos) else { return Ok(()) };
    if ledger.last_abs == Some((x, y)) {
        return Ok(());
    }
    ledger.last_abs = Some((x, y));
    devices.emit_pointer(&abs_frame(x, y))
}

/// Placement must reach the compositor even when the requested cell equals the last
/// one written (the kernel filters unchanged ABS values): take a one-unit detour.
fn place(devices: &mut Devices, ledger: &mut Ledger, displays: &[DisplayRect], pos: Vec2) -> Result<()> {
    ledger.pos = if inside_union(displays, pos) { pos } else { clamp_to_union(displays, pos) };
    let Some((x, y)) = abs_coords(displays, ledger.pos) else { return Ok(()) };
    if ledger.last_abs == Some((x, y)) {
        let detour = if x < ABS_MAX { x + 1 } else { x - 1 };
        devices.emit_pointer(&abs_frame(detour, y))?;
    }
    ledger.last_abs = Some((x, y));
    devices.emit_pointer(&abs_frame(x, y))
}

/// Emits whole wheel detents from the hi-res accumulators (REL_WHEEL ±1 and
/// REL_WHEEL_HI_RES ±120 in one frame, like a real HID mouse), keeping the remainder.
fn flush_wheel(devices: &mut Devices, ledger: &mut Ledger) -> Result<()> {
    let steps_x = (ledger.wheel_x / f64::from(DETENT)).trunc() as i32;
    let steps_y = (ledger.wheel_y / f64::from(DETENT)).trunc() as i32;
    if steps_x == 0 && steps_y == 0 {
        return Ok(());
    }
    ledger.wheel_x -= f64::from(steps_x * DETENT);
    ledger.wheel_y -= f64::from(steps_y * DETENT);
    let mut events = Vec::with_capacity(4);
    if steps_y != 0 {
        events.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_WHEEL.0, steps_y));
        events.push(InputEvent::new(
            EventType::RELATIVE.0,
            RelativeAxisCode::REL_WHEEL_HI_RES.0,
            steps_y * DETENT,
        ));
    }
    if steps_x != 0 {
        events.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_HWHEEL.0, steps_x));
        events.push(InputEvent::new(
            EventType::RELATIVE.0,
            RelativeAxisCode::REL_HWHEEL_HI_RES.0,
            steps_x * DETENT,
        ));
    }
    devices.emit_pointer(&events)
}

fn release_all_held(devices: &mut Devices, ledger: &mut Ledger) {
    let keys: Vec<InputEvent> = ledger.held_keys.drain().map(|k| key_event(k, false)).collect();
    if !keys.is_empty() {
        let _ = devices.emit_keyboard(&keys);
    }
    let buttons: Vec<InputEvent> = ledger.held_buttons.drain().map(|b| key_event(b, false)).collect();
    if !buttons.is_empty() {
        let _ = devices.emit_pointer(&buttons);
    }
    ledger.wheel_x = 0.0;
    ledger.wheel_y = 0.0;
}

#[async_trait::async_trait]
impl Emulate for UinputEmulate {
    async fn enter(&self, pos: Vec2) -> Result<()> {
        let displays = self.shared.displays();
        {
            let devices = &mut *self.devices.lock();
            let ledger = &mut *self.ledger.lock();
            release_all_held(devices, ledger);
            ledger.entered = true;
            place(devices, ledger, &displays, pos)?;
        }
        let screensaver = self.screensaver.clone();
        let _ = tokio::spawn(async move { screensaver.inhibit().await });
        Ok(())
    }

    async fn inject(&self, ev: WireEvent) -> Result<()> {
        let devices = &mut *self.devices.lock();
        let ledger = &mut *self.ledger.lock();
        if !ledger.entered {
            return Ok(());
        }
        match ev {
            WireEvent::Motion { dx, dy } => {
                let displays = self.shared.displays();
                let next = Vec2 { x: ledger.pos.x + dx, y: ledger.pos.y + dy };
                move_to(devices, ledger, &displays, next)
            }
            WireEvent::Button { button, pressed } => {
                let Some(code) = button_code(button) else { return Ok(()) };
                if pressed {
                    ledger.held_buttons.insert(code);
                } else {
                    ledger.held_buttons.remove(&code);
                }
                devices.emit_pointer(&[key_event(code, pressed)])
            }
            WireEvent::Key { code, pressed } => {
                let Ok(code) = u16::try_from(code) else { return Ok(()) };
                if !is_keyboard_code(code) {
                    return Ok(());
                }
                if pressed {
                    ledger.held_keys.insert(code);
                } else {
                    ledger.held_keys.remove(&code);
                }
                devices.emit_keyboard(&[key_event(code, pressed)])
            }
            WireEvent::ScrollPixels { dx, dy } => {
                ledger.wheel_x += dx * f64::from(DETENT) / PIXELS_PER_DETENT;
                ledger.wheel_y += -dy * f64::from(DETENT) / PIXELS_PER_DETENT;
                flush_wheel(devices, ledger)
            }
            WireEvent::Scroll120 { dx, dy } => {
                ledger.wheel_x += f64::from(dx);
                ledger.wheel_y -= f64::from(dy);
                flush_wheel(devices, ledger)
            }
            WireEvent::ScrollStop { .. } => {
                ledger.wheel_x = 0.0;
                ledger.wheel_y = 0.0;
                Ok(())
            }
        }
    }

    async fn leave(&self) -> Result<()> {
        {
            let devices = &mut *self.devices.lock();
            let ledger = &mut *self.ledger.lock();
            release_all_held(devices, ledger);
            ledger.entered = false;
        }
        let screensaver = self.screensaver.clone();
        let _ = tokio::spawn(async move { screensaver.uninhibit().await });
        Ok(())
    }

    async fn release_all(&self) -> Result<()> {
        let devices = &mut *self.devices.lock();
        let ledger = &mut *self.ledger.lock();
        release_all_held(devices, ledger);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
        DisplayRect { id: format!("{x},{y}"), x, y, w, h, scale: 1.0 }
    }

    #[test]
    fn abs_coords_span_the_layout_bounding_box() {
        let displays = [display(0, 0, 1920, 1080), display(1920, 0, 2560, 1440)];
        let (x, y) = abs_coords(&displays, Vec2 { x: 0.0, y: 0.0 }).unwrap();
        assert!(x < 20 && y < 30);
        let (x, y) = abs_coords(&displays, Vec2 { x: 4479.0, y: 1439.0 }).unwrap();
        assert!(x >= ABS_MAX - 10 && x <= ABS_MAX);
        assert!(y >= ABS_MAX - 30 && y <= ABS_MAX);
        let (x, _) = abs_coords(&displays, Vec2 { x: 2240.0, y: 0.0 }).unwrap();
        assert!((x - ABS_MAX / 2).abs() <= 20);
    }

    #[test]
    fn abs_coords_honour_negative_layout_origins() {
        let displays = [display(-1920, 0, 1920, 1080), display(0, 0, 1920, 1080)];
        let (x, _) = abs_coords(&displays, Vec2 { x: -1920.0, y: 0.0 }).unwrap();
        assert!(x < 20);
        let (x, _) = abs_coords(&displays, Vec2 { x: 0.0, y: 0.0 }).unwrap();
        assert!((x - ABS_MAX / 2).abs() <= 20);
    }

    #[test]
    fn clamp_picks_nearest_display_point() {
        let displays = [display(0, 0, 1920, 1080), display(1920, 200, 1920, 1080)];
        let p = clamp_to_union(&displays, Vec2 { x: 2500.0, y: 50.0 });
        assert_eq!(p, Vec2 { x: 2500.0, y: 200.0 });
        let p = clamp_to_union(&displays, Vec2 { x: -30.0, y: 500.0 });
        assert_eq!(p, Vec2 { x: 0.0, y: 500.0 });
    }

    #[test]
    fn wheel_accumulates_fractional_pixels_with_kernel_sign() {
        let mut ledger = Ledger::default();
        for _ in 0..300 {
            ledger.wheel_y += -0.05 * f64::from(DETENT) / PIXELS_PER_DETENT;
        }
        assert_eq!((ledger.wheel_y / f64::from(DETENT)).trunc() as i32, -1);
        assert!(is_keyboard_code(248));
        assert!(!is_keyboard_code(0x110));
        assert!(is_keyboard_code(0x160));
    }
}
