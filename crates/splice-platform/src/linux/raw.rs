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
    timestamps: [EmissionClock; 2],
    diagnostics: crate::raw::diagnostics::Diagnostics,
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
        for clock in &mut state.devices.as_mut().expect("prepared devices").timestamps {
            clock.native_us = None;
        }
        self.shared.raw_boundary_begin(session);
        Ok(())
    }

    fn boundary_policy(&self, session: u64, boundary: bool) -> Result<()> {
        if self.state.lock().session != Some(session) {
            return Err(PlatformError::Unavailable(
                "stale raw boundary policy".into(),
            ));
        }
        self.shared.raw_boundary_policy(boundary);
        Ok(())
    }

    fn inject(&self, session: u64, report: &RawReport, captured_local_us: u64) -> Result<()> {
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
            if let Some((code, pressed)) = injected_key(event) {
                self.shared.note_injected_key(code, pressed);
            }
        }
        self.shared.raw_boundary_motion(
            events
                .iter()
                .any(|event| matches!(event, RawEvent::Motion { x, y } if *x != 0 || *y != 0)),
        );
        let result = state
            .devices
            .as_mut()
            .ok_or_else(|| {
                PlatformError::Unavailable("raw virtual devices are unavailable".into())
            })?
            .emit(&events, captured_local_us, Some(report.captured_us));
        if result.is_err() {
            self.shared.raw_boundary_end();
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
        self.shared.raw_boundary_end();
        let releases = state.ledger.release();
        let result = match &mut state.devices {
            Some(devices) => {
                devices.wheel = (0, 0);
                self.shared.note_injection();
                for event in &releases {
                    if let Some((code, pressed)) = injected_key(event) {
                        self.shared.note_injected_key(code, pressed);
                    }
                }
                let result = devices.emit(&releases, crate::raw::clock::now_us(), None);
                devices.diagnostics.finish_window();
                result
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
            timestamps: [EmissionClock::default(); 2],
            diagnostics: crate::raw::diagnostics::Diagnostics::new("uinput"),
        })
    }

    fn emit(
        &mut self,
        events: &[RawEvent],
        captured_local_us: u64,
        native_us: Option<u64>,
    ) -> std::io::Result<()> {
        for (keyboard, mut events) in encode(events, &mut self.wheel) {
            let now_us = crate::raw::clock::now_us();
            let clock = &mut self.timestamps[usize::from(keyboard)];
            let timestamp = clock.prepare(native_us, captured_local_us, now_us);
            if timestamp != captured_local_us {
                self.diagnostics.record(
                    "timestamp_adjustment",
                    timestamp.abs_diff(captured_local_us),
                );
            }
            self.diagnostics.record("injected_age", now_us - timestamp);
            for event in &mut events {
                let mut raw: libc::input_event = (*event).into();
                raw.time.tv_sec = (timestamp / 1_000_000) as libc::time_t;
                raw.time.tv_usec = (timestamp % 1_000_000) as libc::suseconds_t;
                *event = raw.into();
            }
            if keyboard {
                self.keyboard.emit(&events)?;
            } else {
                self.pointer.emit(&events)?;
            }
            clock.commit(native_us, timestamp);
            self.diagnostics
                .record("write_duration", crate::raw::clock::now_us() - now_us);
            self.diagnostics.flush_if_due(now_us);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct EmissionClock {
    emitted_us: u64,
    native_us: Option<u64>,
}

impl EmissionClock {
    fn prepare(&self, native_us: Option<u64>, mapped_us: u64, now_us: u64) -> u64 {
        let candidate = match (self.native_us, native_us) {
            (Some(last), Some(native)) => {
                mapped_us.max(self.emitted_us.saturating_add(native.saturating_sub(last)))
            }
            _ => mapped_us,
        };
        emission_time(candidate, self.emitted_us, now_us)
    }

    fn commit(&mut self, native_us: Option<u64>, emitted_us: u64) {
        self.native_us =
            native_us.map(|native| self.native_us.map_or(native, |last| last.max(native)));
        self.emitted_us = emitted_us;
    }
}

fn emission_time(captured_us: u64, last_us: u64, now_us: u64) -> u64 {
    captured_us.clamp(
        last_us
            .max(now_us.saturating_sub(2_000_000))
            .max(1)
            .min(now_us),
        now_us,
    )
}

fn injected_key(event: &RawEvent) -> Option<(u32, bool)> {
    match *event {
        RawEvent::Key { code, pressed } => Some((u32::from(code), pressed)),
        RawEvent::Button { number, pressed } => Some((0x110 + u32::from(number) - 1, pressed)),
        RawEvent::Motion { .. } | RawEvent::Wheel { .. } | RawEvent::Removed => None,
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
    fn output_timestamps_remain_valid_across_late_reports_and_sessions() {
        assert_eq!(emission_time(3_100_000, 3_000_000, 4_000_000), 3_100_000);
        assert_eq!(emission_time(2_900_000, 3_000_000, 4_000_000), 3_000_000);
        assert_eq!(emission_time(5_000_000, 3_000_000, 4_000_000), 4_000_000);
        assert_eq!(emission_time(1, 0, 4_000_000), 2_000_000);
        assert_eq!(emission_time(0, 0, 1000), 1);
    }

    #[test]
    fn clock_estimate_steps_preserve_native_spacing_when_time_is_available() {
        let mut clock = EmissionClock::default();
        let mut emitted = Vec::new();
        for index in 0..30 {
            let native = 1_000_000 + index * 1000;
            let offset = if index < 10 {
                10_000
            } else if index < 20 {
                5000
            } else {
                6000
            };
            let now = native + 50_000;
            let stamp = clock.prepare(Some(native), native + offset, now);
            clock.commit(Some(native), stamp);
            assert!(stamp <= now);
            emitted.push(stamp);
        }
        assert!(emitted.windows(2).all(|pair| pair[1] - pair[0] == 1000));
    }

    #[test]
    fn interleaved_native_reports_do_not_inflate_elapsed_time() {
        let mut clock = EmissionClock::default();
        let mut emitted = Vec::new();
        for native in [10_000, 9000, 11_000, 10_000, 12_000, 11_000] {
            let stamp = clock.prepare(Some(native), native + 1_000_000, 2_000_000);
            clock.commit(Some(native), stamp);
            emitted.push(stamp);
        }
        assert_eq!(
            emitted,
            [1_010_000, 1_010_000, 1_011_000, 1_011_000, 1_012_000, 1_012_000]
        );
        let stamp = clock.prepare(Some(13_000), 1_013_000, 1_012_500);
        assert_eq!(stamp, 1_012_500);
    }

    #[tokio::test]
    #[ignore = "requires /dev/uinput; grabs only generated fixture devices"]
    async fn native_raw_timestamps_and_button_echoes_survive_session_release() {
        use std::{
            os::fd::AsRawFd,
            time::{Duration, Instant, UNIX_EPOCH},
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            raw_destination: Default::default(),
            raw_boundary: Default::default(),
            capture_control: Default::default(),
            emission: Mutex::new(()),
            tx,
            health: Mutex::new(Default::default()),
            displays: parking_lot::RwLock::new(Vec::new()),
            epoch: Instant::now(),
            last_injection: Default::default(),
            injected_keys: Mutex::new(Default::default()),
        });
        let input = RelativeInput::new(shared.clone());
        input.prepare().await.unwrap();
        let mut readers = Vec::new();
        {
            let mut state = input.state.lock();
            let devices = state.devices.as_mut().unwrap();
            {
                let device = &mut devices.pointer;
                let path = device
                    .enumerate_dev_nodes_blocking()
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap();
                let mut reader = evdev::raw_stream::RawDevice::open(path).unwrap();
                reader.grab().unwrap();
                let clock = libc::CLOCK_MONOTONIC;
                assert_eq!(
                    unsafe {
                        libc::ioctl(reader.as_raw_fd(), 0x400445a0u64 as libc::c_ulong, &clock)
                    },
                    0
                );
                readers.push(reader);
            }
        }
        let native = crate::raw::clock::now_us() - 100_000;
        input.begin(1).unwrap();
        let report = RawReport {
            device: 1,
            sequence: 0,
            captured_us: native,
            events: vec![
                RawEvent::Motion { x: 17, y: -9 },
                RawEvent::Button {
                    number: 8,
                    pressed: true,
                },
            ],
        };
        input.inject(1, &report, native).unwrap();
        for reader in &mut readers {
            let events: Vec<_> = reader.fetch_events().unwrap().collect();
            assert!(events.len() >= 2);
            for event in events {
                assert_eq!(
                    event
                        .timestamp()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_micros() as u64,
                    native
                );
            }
        }
        assert!(shared.injected_recently(0x117, true, Duration::from_secs(1)));
        input.end(1).unwrap();
        assert!(shared
            .since_injection()
            .is_some_and(|age| age < Duration::from_millis(100)));
        assert!(shared.injected_recently(0x117, false, Duration::from_secs(1)));
        let mut released_at = 0;
        for reader in &mut readers {
            let events: Vec<_> = reader.fetch_events().unwrap().collect();
            assert!(events
                .iter()
                .any(|event| event.event_type() == EventType::KEY && event.value() == 0));
            let timestamp = events
                .last()
                .unwrap()
                .timestamp()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros() as u64;
            assert!(timestamp >= native + 100_000);
            released_at = released_at.max(timestamp);
        }
        input.begin(2).unwrap();
        input
            .inject(
                2,
                &RawReport {
                    sequence: 0,
                    events: vec![RawEvent::Motion { x: 1, y: 0 }],
                    ..report
                },
                native,
            )
            .unwrap();
        let events: Vec<_> = readers[0].fetch_events().unwrap().collect();
        let timestamp = events
            .last()
            .unwrap()
            .timestamp()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        assert!(timestamp >= native + 100_000 && timestamp <= released_at);
        input.end(2).unwrap();
        for (session, captured_us) in [(3, native + 1_000_000_000_000), (4, 1)] {
            tokio::time::sleep(Duration::from_millis(3)).await;
            input.begin(session).unwrap();
            let mapped = crate::raw::clock::now_us() - 1000;
            input
                .inject(
                    session,
                    &RawReport {
                        device: 1,
                        sequence: 0,
                        captured_us,
                        events: vec![RawEvent::Motion { x: 1, y: 0 }],
                    },
                    mapped,
                )
                .unwrap();
            let events: Vec<_> = readers[0].fetch_events().unwrap().collect();
            assert!(!events.is_empty());
            for event in events {
                assert_eq!(
                    event
                        .timestamp()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_micros() as u64,
                    mapped
                );
            }
            input.end(session).unwrap();
        }
    }

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

    #[test]
    fn injected_key_accounting_covers_keyboard_buttons_and_releases() {
        assert_eq!(
            injected_key(&RawEvent::Key {
                code: 29,
                pressed: true,
            }),
            Some((29, true))
        );
        assert_eq!(
            injected_key(&RawEvent::Button {
                number: 8,
                pressed: false,
            }),
            Some((0x117, false))
        );
        assert_eq!(injected_key(&RawEvent::Motion { x: 1, y: -1 }), None);
    }
}
