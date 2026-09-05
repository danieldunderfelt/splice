use super::{Shared, VIRTUAL_DEVICE_PREFIX};
use crate::{raw::RawEmulate, PlatformError, Result};
use evdev::{
    uinput::VirtualDevice, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    RelativeAxisCode,
};
use parking_lot::Mutex;
use splice_proto::raw::{keyboard_code, RawEvent, RawLedger, RawReport};
use std::sync::Arc;

pub struct RelativeInput {
    shared: Arc<Shared>,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    devices: Option<Devices>,
    session: Option<u64>,
    ledger: RawLedger,
}

struct Devices {
    pointer: VirtualDevice,
    keyboard: VirtualDevice,
    wheel: (i64, i64),
}

impl RelativeInput {
    pub fn force_release(&self) {
        let session = self.state.lock().session;
        if let Some(session) = session {
            if let Err(error) = self.end(session) {
                tracing::error!(%error, "emergency raw input release failed");
            }
        }
    }

    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            state: Arc::new(Mutex::new(State::default())),
        }
    }
}

#[async_trait::async_trait]
impl RawEmulate for RelativeInput {
    async fn prepare(&self) -> Result<()> {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock();
            if state.devices.is_none() {
                state.devices = Some(Devices::open().map_err(|e| {
                    PlatformError::Unavailable(format!(
                        "Raw input needs /dev/uinput and packaging/linux/70-splice.rules: {e}"
                    ))
                })?);
            }
            Ok(())
        })
        .await
        .map_err(|e| PlatformError::Other(e.into()))?
    }

    fn begin(&self, session: u64) -> Result<()> {
        let mut state = self.state.lock();
        if state.session.is_some() {
            return Err(PlatformError::Unavailable(
                "raw input already has an owner".into(),
            ));
        }
        if state.devices.is_none() {
            return Err(PlatformError::Unavailable(
                "raw virtual devices are not prepared".into(),
            ));
        }
        state.session = Some(session);
        state.ledger = RawLedger::default();
        Ok(())
    }

    fn inject(&self, session: u64, report: &RawReport) -> Result<()> {
        let mut state = self.state.lock();
        if state.session != Some(session) {
            return Err(PlatformError::Unavailable("stale raw input session".into()));
        }
        let events = state
            .ledger
            .apply(report)
            .map_err(|e| PlatformError::Other(anyhow::anyhow!(e)))?;
        self.shared.note_injection();
        for event in &events {
            if let RawEvent::Key { code, pressed } = event {
                self.shared.note_injected_key(u32::from(*code), *pressed);
            }
        }
        let result = state
            .devices
            .as_mut()
            .ok_or_else(|| {
                PlatformError::Unavailable("raw virtual devices are unavailable".into())
            })?
            .emit(&events);
        if result.is_err() {
            state.devices = None;
            state.session = None;
            state.ledger.release();
        }
        result.map_err(|e| PlatformError::Other(e.into()))
    }

    fn end(&self, session: u64) -> Result<()> {
        let mut state = self.state.lock();
        if state.session != Some(session) {
            return Ok(());
        }
        state.session = None;
        let releases = state.ledger.release();
        let result = match &mut state.devices {
            Some(devices) => {
                devices.wheel = (0, 0);
                devices.emit(&releases)
            }
            None => Ok(()),
        };
        if result.is_err() {
            state.devices = None;
        }
        result.map_err(|e| PlatformError::Other(e.into()))
    }
}

impl Devices {
    fn open() -> Result<Self> {
        let mut buttons = AttributeSet::new();
        for code in 0x110..=0x117 {
            buttons.insert(KeyCode::new(code));
        }
        let mut axes = AttributeSet::new();
        for axis in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            axes.insert(axis);
        }
        let mut keys = AttributeSet::new();
        for code in (1..=0x2bf).filter(|c| keyboard_code(*c)) {
            keys.insert(KeyCode::new(code));
        }
        let build = || -> std::io::Result<(VirtualDevice, VirtualDevice)> {
            let pointer = VirtualDevice::builder()?
                .name(&format!("{VIRTUAL_DEVICE_PREFIX} Raw Mouse"))
                .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5350, 3, 1))
                .with_keys(&buttons)?
                .with_relative_axes(&axes)?
                .build()?;
            let keyboard = VirtualDevice::builder()?
                .name(&format!("{VIRTUAL_DEVICE_PREFIX} Raw Keyboard"))
                .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5350, 4, 1))
                .with_keys(&keys)?
                .build()?;
            Ok((pointer, keyboard))
        };
        let (mut pointer, mut keyboard) = build().map_err(|e| PlatformError::Other(e.into()))?;
        super::uinput::wait_for_udev(&mut pointer)?;
        super::uinput::wait_for_udev(&mut keyboard)?;
        Ok(Self {
            pointer,
            keyboard,
            wheel: (0, 0),
        })
    }

    fn emit(&mut self, events: &[RawEvent]) -> std::io::Result<()> {
        for (keyboard, events) in encode(events, &mut self.wheel) {
            if keyboard {
                self.keyboard.emit(&events)?;
            } else {
                self.pointer.emit(&events)?;
            }
        }
        Ok(())
    }
}

fn encode(events: &[RawEvent], wheel: &mut (i64, i64)) -> Vec<(bool, Vec<InputEvent>)> {
    let mut batches: Vec<(bool, Vec<InputEvent>)> = Vec::new();
    for event in events {
        if matches!(event, RawEvent::Removed) {
            continue;
        }
        let keyboard = matches!(event, RawEvent::Key { .. });
        if batches.last().is_none_or(|(kind, _)| *kind != keyboard) {
            batches.push((keyboard, Vec::new()));
        }
        let output = &mut batches.last_mut().expect("batch was created").1;
        match *event {
            RawEvent::Motion { x, y } => {
                if x != 0 {
                    output.push(InputEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_X.0,
                        x,
                    ));
                }
                if y != 0 {
                    output.push(InputEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_Y.0,
                        y,
                    ));
                }
            }
            RawEvent::Wheel { x120, y120 } => {
                for (delta, remainder, hi, lo) in [
                    (
                        x120,
                        &mut wheel.0,
                        RelativeAxisCode::REL_HWHEEL_HI_RES,
                        RelativeAxisCode::REL_HWHEEL,
                    ),
                    (
                        y120,
                        &mut wheel.1,
                        RelativeAxisCode::REL_WHEEL_HI_RES,
                        RelativeAxisCode::REL_WHEEL,
                    ),
                ] {
                    if delta == 0 {
                        continue;
                    }
                    output.push(InputEvent::new(EventType::RELATIVE.0, hi.0, delta));
                    *remainder += i64::from(delta);
                    let detents = *remainder / 120;
                    *remainder %= 120;
                    if detents != 0 {
                        output.push(InputEvent::new(EventType::RELATIVE.0, lo.0, detents as i32));
                    }
                }
            }
            RawEvent::Key { code, pressed } => {
                output.push(InputEvent::new(EventType::KEY.0, code, i32::from(pressed)))
            }
            RawEvent::Button { number, pressed } => output.push(InputEvent::new(
                EventType::KEY.0,
                0x110 + u16::from(number) - 1,
                i32::from(pressed),
            )),
            RawEvent::Removed => {}
        }
    }
    batches.retain(|(_, events)| !events.is_empty());
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires /dev/uinput access and a running udev; emits no input"]
    fn native_raw_device_capabilities() {
        let mut devices = Devices::open().unwrap();
        for (device, kind) in [
            (&mut devices.pointer, "MOUSE"),
            (&mut devices.keyboard, "KEYBOARD"),
        ] {
            let path = device
                .enumerate_dev_nodes_blocking()
                .unwrap()
                .next()
                .unwrap()
                .unwrap();
            let input = evdev::Device::open(&path).unwrap();
            assert!(input.supported_absolute_axes().is_none());
            assert!(input.name().unwrap().starts_with(VIRTUAL_DEVICE_PREFIX));
            if kind == "MOUSE" {
                let axes = input.supported_relative_axes().unwrap();
                assert!(
                    axes.contains(RelativeAxisCode::REL_X)
                        && axes.contains(RelativeAxisCode::REL_Y)
                );
                assert!(axes.contains(RelativeAxisCode::REL_WHEEL_HI_RES));
            } else {
                assert!(input
                    .supported_keys()
                    .unwrap()
                    .contains(KeyCode::KEY_LEFTSHIFT));
                assert!(input
                    .supported_keys()
                    .unwrap()
                    .contains(KeyCode::KEY_VOLUMEUP));
            }
            let properties = std::process::Command::new("udevadm")
                .args(["info", "--query=property", "--name"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(properties.status.success());
            let properties = String::from_utf8(properties.stdout).unwrap();
            assert!(
                properties
                    .lines()
                    .any(|line| line == format!("ID_INPUT_{kind}=1")),
                "{properties}"
            );
            assert!(!properties.contains("ID_INPUT_JOYSTICK=1"));
        }
    }

    #[test]
    fn mixed_reports_keep_keyboard_and_pointer_transition_order() {
        let batches = encode(
            &[
                RawEvent::Button {
                    number: 1,
                    pressed: true,
                },
                RawEvent::Key {
                    code: 29,
                    pressed: true,
                },
                RawEvent::Motion { x: 3, y: -2 },
                RawEvent::Button {
                    number: 1,
                    pressed: false,
                },
                RawEvent::Key {
                    code: 29,
                    pressed: false,
                },
            ],
            &mut (0, 0),
        );
        assert_eq!(
            batches
                .iter()
                .map(|(keyboard, _)| *keyboard)
                .collect::<Vec<_>>(),
            [false, true, false, true]
        );
        assert_eq!(
            batches
                .iter()
                .flat_map(|(_, events)| events.iter().map(|e| (e.code(), e.value())))
                .collect::<Vec<_>>(),
            [(0x110, 1), (29, 1), (0, 3), (1, -2), (0x110, 0), (29, 0)]
        );
    }

    #[test]
    fn exact_relative_axes_and_fractional_detents() {
        let mut wheel = (0, 0);
        let batches = encode(
            &[
                RawEvent::Motion {
                    x: -32768,
                    y: 32767,
                },
                RawEvent::Wheel {
                    x120: 30,
                    y120: -15,
                },
                RawEvent::Key {
                    code: 30,
                    pressed: true,
                },
            ],
            &mut wheel,
        );
        let p = &batches[0].1;
        let k = &batches[1].1;
        assert_eq!(p[0].event_type(), EventType::RELATIVE);
        assert_eq!(
            (p[0].code(), p[0].value()),
            (RelativeAxisCode::REL_X.0, -32768)
        );
        assert_eq!(p[1].value(), 32767);
        assert_eq!(k[0].value(), 1);
        assert_eq!(wheel, (30, -15));
        let batches = encode(
            &[RawEvent::Wheel {
                x120: 90,
                y120: -105,
            }],
            &mut wheel,
        );
        let p = &batches[0].1;
        assert_eq!(
            p.iter().map(|e| e.value()).collect::<Vec<_>>(),
            [90, 1, -105, -1]
        );
        assert_eq!(wheel, (0, 0));
    }
}
