use super::*;
use crate::raw::shortcut::Stream;
use splice_proto::{InputEvent, PointerButton};

fn shared() -> (Arc<Shared>, mpsc::UnboundedReceiver<PlatformEvent>) {
    let (tx, events) = mpsc::unbounded_channel();
    (
        Arc::new(Shared {
            capture_control: Default::default(),
            emission: Mutex::new(()),
            tx,
            health: Mutex::new(Default::default()),
            displays: parking_lot::RwLock::new(Vec::new()),
            epoch: Instant::now(),
            last_injection: Default::default(),
            injected_keys: Mutex::new(Default::default()),
        }),
        events,
    )
}

#[test]
fn discovery_distinguishes_bluetooth_from_virtual_outputs_and_irrelevant_nodes() {
    assert!(physical_sys_path(Path::new(
        "/sys/devices/virtual/misc/uhid/input6/event2"
    )));
    assert!(physical_sys_path(Path::new(
        "/sys/devices/pci0000:00/usb1/1-1/input3/event0"
    )));
    assert!(!physical_sys_path(Path::new(
        "/sys/devices/virtual/input/input6/event2"
    )));
    assert!(!candidate("10000000000000 0", "0").unwrap());
    assert!(candidate("0", "3").unwrap());
    assert!(candidate("100040000000", "0").unwrap());
    assert!(!candidate("0", "100").unwrap());
    assert!(candidate("not hex", "0").is_err());
}

#[test]
fn shortcut_callbacks_switch_once_reset_at_home_and_keep_held_buttons_typed() {
    let control = control::Control::default();
    let key = |code, pressed| InputEvent::Key { code, pressed };
    assert!(!control.key(Stream::Hid, 88, true, true, 0).0);
    control.activate();
    control.desktop_event(&key(29, true), 10);
    control.desktop_event(&key(56, true), 10);
    assert!(control.key(Stream::Hid, 88, true, true, 11).0);
    assert_eq!(control.desktop_event(&key(88, true), 600), (false, true));
    assert!(control.switching.load(Ordering::SeqCst));
    assert!(control.suppressed(29));
    control.raw_begin();
    control.desktop_event(&key(42, true), 700);
    control.release();
    assert!(control.failure(2000).is_none());
    control.activate();
    assert_eq!(control.desktop_event(&key(88, true), 3000), (false, false));
    assert!(!control.suppressed(29));
    control.desktop_event(
        &InputEvent::Button {
            button: PointerButton::Right,
            pressed: true,
        },
        3100,
    );
    assert_eq!(
        control.desktop_snapshot([30, 42].into(), true).unwrap(),
        vec![
            key(42, true),
            key(30, true),
            InputEvent::Button {
                button: PointerButton::Right,
                pressed: true
            }
        ]
    );
    assert!(control.desktop_snapshot([42].into(), false).is_none());
    control.desktop_event(
        &InputEvent::Button {
            button: PointerButton::Right,
            pressed: false,
        },
        3200,
    );
    assert!(control.desktop_snapshot([42].into(), true).is_none());
}

struct Fixture {
    device: Option<evdev::uinput::VirtualDevice>,
    capture: Arc<DeviceCapture>,
    path: PathBuf,
    mock: crate::mock::MockHandle,
    events: mpsc::UnboundedReceiver<PlatformEvent>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.device.take();
    }
}

impl Fixture {
    fn new() -> Self {
        use evdev::{AttributeSet, BusType, InputId, uinput::VirtualDevice};
        let mut keys = AttributeSet::new();
        for code in [
            KeyCode::KEY_A,
            KeyCode::KEY_Z,
            KeyCode::KEY_LEFTSHIFT,
            KeyCode::BTN_LEFT,
            KeyCode::BTN_TASK,
            KeyCode::BTN_0,
            KeyCode::new(0x11f),
        ] {
            keys.insert(code);
        }
        let mut axes = AttributeSet::new();
        for code in [
            Rel::REL_X,
            Rel::REL_Y,
            Rel::REL_WHEEL,
            Rel::REL_WHEEL_HI_RES,
        ] {
            axes.insert(code);
        }
        let mut device = VirtualDevice::builder()
            .unwrap()
            .name("Splice raw capture isolated test")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5350, 91, 1))
            .with_keys(&keys)
            .unwrap()
            .with_relative_axes(&axes)
            .unwrap()
            .with_absolute_axis(&evdev::UinputAbsSetup::new(
                evdev::AbsoluteAxisCode::ABS_VOLUME,
                evdev::AbsInfo::new(0, 0, 255, 0, 0, 0),
            ))
            .unwrap()
            .build()
            .unwrap();
        crate::linux::uinput::wait_for_udev(&mut device).unwrap();
        let path = device
            .enumerate_dev_nodes_blocking()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let mut reader = open(&path, 1).unwrap().unwrap();
        reader
            .input
            .grab()
            .expect("isolate fixture before emitting any input");
        let (shared, events) = shared();
        let (platform, mock) = crate::mock::create(crate::mock::one_display());
        let capture = Arc::new(DeviceCapture {
            shared,
            desktop: platform.capture,
            runtime: tokio::runtime::Handle::current(),
            state: Mutex::new(State {
                devices: [(path.clone(), reader)].into(),
                ready: true,
                next_device: 1,
                ..Default::default()
            }),
            origin: Instant::now(),
        });
        Self {
            device: Some(device),
            capture,
            path,
            mock,
            events,
        }
    }

    fn emit(&mut self, events: &[evdev::InputEvent]) {
        self.device.as_mut().unwrap().emit(events).unwrap();
    }

    fn read(&self, forward: bool) -> anyhow::Result<()> {
        self.capture
            .read(&mut self.capture.state.lock(), &self.path, forward)
    }
}

fn event(kind: evdev::EventType, code: u16, value: i32) -> evdev::InputEvent {
    evdev::InputEvent::new(kind.0, code, value)
}

#[tokio::test]
#[ignore = "requires /dev/uinput; only generated fixture devices are grabbed and emit input"]
async fn native_evdev_counts_snapshots_handoffs_and_failures() {
    use evdev::EventType;
    let mut fixture = Fixture::new();
    assert!(!physical_paths().unwrap().contains_key(&fixture.path));
    let (tx, mut reports) = mpsc::channel(32);
    assert!(
        fixture
            .capture
            .begin(tx.clone(), None, Arc::default())
            .is_err()
    );
    fixture.emit(&[
        event(EventType::RELATIVE, Rel::REL_X.0, 123),
        event(EventType::KEY, 42, 1),
        event(EventType::KEY, 0x110, 1),
    ]);
    fixture.read(false).unwrap();
    fixture.capture.shared.capture_control.activate();
    fixture.capture.begin(tx, Some(1), Arc::default()).unwrap();
    let snapshot = reports.try_recv().unwrap();
    assert_eq!(snapshot.sequence, 0);
    assert_eq!(
        snapshot.events,
        [
            RawEvent::Key {
                code: 42,
                pressed: true
            },
            RawEvent::Button {
                number: 1,
                pressed: true
            }
        ]
    );
    let mut observer = RawDevice::from_fd(
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fixture.path)
            .unwrap()
            .into(),
    )
    .unwrap();
    fixture.emit(&[
        event(EventType::RELATIVE, Rel::REL_X.0, -12345),
        event(EventType::KEY, KeyCode::KEY_A.0, 1),
        event(EventType::RELATIVE, Rel::REL_Y.0, 6789),
        event(EventType::RELATIVE, Rel::REL_WHEEL.0, 1),
        event(EventType::RELATIVE, Rel::REL_WHEEL_HI_RES.0, 15),
    ]);
    fixture.read(true).unwrap();
    let report = reports.try_recv().unwrap();
    assert_eq!(report.sequence, 1);
    assert!(report.captured_us >= snapshot.captured_us);
    assert_eq!(
        report.events,
        vec![
            RawEvent::Motion { x: -12345, y: 0 },
            RawEvent::Key {
                code: 30,
                pressed: true
            },
            RawEvent::Motion { x: 0, y: 6789 },
            RawEvent::Wheel { x120: 0, y120: 15 },
        ]
    );
    assert!(matches!(observer.fetch_events(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    fixture
        .capture
        .shared
        .emit(PlatformEvent::Capture(CaptureEvent::Input(
            InputEvent::Button {
                button: PointerButton::Right,
                pressed: true,
            },
        )));
    fixture.capture.end();
    assert!(
        fixture
            .capture
            .shared
            .capture_control
            .active
            .load(Ordering::SeqCst)
    );
    fixture.emit(&[event(EventType::KEY, 44, 1)]);
    fixture
        .capture
        .shared
        .emit(PlatformEvent::Capture(CaptureEvent::Input(
            InputEvent::Key {
                code: 44,
                pressed: true,
            },
        )));
    assert!(fixture.events.try_recv().is_err());
    fixture.capture.begin_capture().await.unwrap();
    let mut replayed = Vec::new();
    while let Ok(PlatformEvent::Capture(CaptureEvent::Input(event))) = fixture.events.try_recv() {
        replayed.push(event);
    }
    assert_eq!(
        replayed,
        vec![
            InputEvent::Key {
                code: 42,
                pressed: true
            },
            InputEvent::Key {
                code: 30,
                pressed: true
            },
            InputEvent::Key {
                code: 44,
                pressed: true
            },
            InputEvent::Button {
                button: PointerButton::Right,
                pressed: true
            },
        ]
    );
    fixture.emit(&[event(EventType::KEY, 0x110, 0)]);
    fixture
        .capture
        .shared
        .emit(PlatformEvent::Capture(CaptureEvent::Input(
            InputEvent::Button {
                button: PointerButton::Right,
                pressed: false,
            },
        )));
    assert!(matches!(
        fixture.events.try_recv().unwrap(),
        PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Button {
            button: PointerButton::Right,
            pressed: false
        }))
    ));
    fixture.emit(&[event(EventType::KEY, 44, 0)]);
    fixture
        .capture
        .shared
        .emit(PlatformEvent::Capture(CaptureEvent::Input(
            InputEvent::Key {
                code: 44,
                pressed: false,
            },
        )));
    assert!(matches!(
        fixture.events.try_recv().unwrap(),
        PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Key {
            code: 44,
            pressed: false
        }))
    ));
    assert!(fixture.mock.state.lock().capture_ends.is_empty());
    fixture.capture.end_capture(None).await.unwrap();
    assert!(
        !fixture
            .capture
            .shared
            .capture_control
            .active
            .load(Ordering::SeqCst)
    );
    assert!(!fixture.capture.state.lock().desktop_snapshot);
    fixture.emit(&[
        event(EventType::KEY, 42, 0),
        event(EventType::KEY, 30, 0),
        event(EventType::KEY, 0x110, 0),
    ]);
    fixture.read(false).unwrap();
    fixture.capture.shared.capture_control.activate();
    let (output, mut reports) = mpsc::channel(1);
    let operation = Arc::new(crate::raw::RawOperation::default());
    fixture
        .capture
        .begin(output, Some(1), operation.clone())
        .unwrap();
    fixture.emit(&[event(EventType::RELATIVE, Rel::REL_X.0, 1)]);
    fixture.emit(&[event(EventType::RELATIVE, Rel::REL_X.0, 2)]);
    let reason = fixture.read(true).unwrap_err().to_string();
    assert!(reason.contains("queue overflowed"));
    let capture = fixture.capture.clone();
    tokio::task::spawn_blocking(move || capture.fail(&mut capture.state.lock(), reason))
        .await
        .unwrap();
    assert_eq!(
        reports.try_recv().unwrap().events,
        [RawEvent::Motion { x: 1, y: 0 }]
    );
    assert!(matches!(
        reports.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    let failure = fixture.events.try_recv().unwrap();
    assert!(
        matches!(failure, PlatformEvent::RawCaptureFailed(failed) if Arc::ptr_eq(&failed, &operation))
    );
    assert!(fixture.events.try_recv().is_err());
    assert!(
        !fixture
            .capture
            .shared
            .capture_control
            .active
            .load(Ordering::SeqCst)
    );
    assert_eq!(fixture.mock.state.lock().capture_ends.len(), 2);
    fixture.device.take();
    assert!(fixture.read(false).is_err());
}

#[test]
#[ignore = "requires physical input devices and read permissions; reads capabilities only, never grabs or forwards input"]
fn native_raw_source_descriptors() {
    let mut mouse = false;
    let mut keyboard = false;
    for (index, path) in physical_paths().unwrap().keys().enumerate() {
        if let Some(device) = open(path, index as u64 + 1).unwrap() {
            mouse |= device.mouse;
            keyboard |= device.keyboard;
        }
    }
    assert!(
        mouse && keyboard,
        "a readable relative mouse and keyboard are required"
    );
}

#[test]
fn releasing_one_keyboard_keeps_the_shortcut_suppressed_on_the_other() {
    let control = control::Control::default();
    control.activate();
    let mut held: BTreeSet<_> = [29, 56, 88].into();
    assert!(control.raw_key(88, true, &held, 0).0);
    assert_eq!(control.raw_key(29, false, &held, 100), (false, true));
    assert!(control.suppressed(29));
    held.remove(&29);
    assert_eq!(control.raw_key(29, false, &held, 200), (false, true));
    assert!(!control.suppressed(29));
}

#[tokio::test]
#[ignore = "requires /dev/uinput; only generated fixture devices are grabbed and emit input"]
async fn native_evdev_desktop_handoff_waits_for_wayland_buttons() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let mut fixture = Fixture::new();
    let capture = fixture.capture.clone();
    capture.shared.capture_control.activate();
    capture.prepare().unwrap();
    fixture.emit(&[event(evdev::EventType::KEY, KeyCode::BTN_LEFT.0, 1)]);
    let mut handoff = Box::pin(capture.begin_capture());
    poll_fn(|cx| {
        assert!(handoff.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(
        capture
            .shared
            .capture_control
            .raw_hold
            .load(Ordering::SeqCst)
    );
    let emission = capture.shared.emission.lock();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let callback = {
        let shared = capture.shared.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            shared.emit(PlatformEvent::Capture(CaptureEvent::Input(
                InputEvent::Button {
                    button: PointerButton::Right,
                    pressed: true,
                },
            )));
        })
    };
    barrier.wait();
    drop(emission);
    tokio::time::timeout(Duration::from_secs(1), handoff)
        .await
        .unwrap()
        .unwrap();
    callback.join().unwrap();
    assert!(matches!(
        fixture.events.try_recv().unwrap(),
        PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Button {
            button: PointerButton::Right,
            pressed: true
        }))
    ));
    assert!(fixture.events.try_recv().is_err());
    assert!(
        !capture
            .shared
            .capture_control
            .raw_hold
            .load(Ordering::SeqCst)
    );

    capture.prepare().unwrap();
    fixture.emit(&[event(evdev::EventType::KEY, KeyCode::BTN_LEFT.0, 0)]);
    let mut handoff = Box::pin(capture.begin_capture());
    poll_fn(|cx| {
        assert!(handoff.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    capture
        .shared
        .emit(PlatformEvent::Capture(CaptureEvent::Input(
            InputEvent::Button {
                button: PointerButton::Right,
                pressed: false,
            },
        )));
    handoff.await.unwrap();
    assert!(fixture.events.try_recv().is_err());

    capture.prepare().unwrap();
    fixture.emit(&[event(evdev::EventType::KEY, KeyCode::BTN_LEFT.0, 1)]);
    let error = capture.begin_capture().await.unwrap_err();
    assert!(error.to_string().contains("within 500 ms"));
    assert!(
        capture
            .shared
            .capture_control
            .raw_hold
            .load(Ordering::SeqCst)
    );
    assert!(fixture.events.try_recv().is_err());
    capture.end_capture(None).await.unwrap();
    assert!(
        !capture
            .shared
            .capture_control
            .raw_hold
            .load(Ordering::SeqCst)
    );
    assert!(!capture.shared.capture_control.active.load(Ordering::SeqCst));
}
