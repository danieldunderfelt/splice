use super::{tap::TapState, MacShared};
pub mod probe;
use crate::{
    raw::{hid::Decoder, RawCapture},
    PlatformError, PlatformEvent, Result,
};
use core_foundation::{
    array::CFArray,
    base::{CFType, CFTypeRef, TCFType},
    data::CFData,
    dictionary::CFDictionary,
    number::CFNumber,
    runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRef},
    string::{CFString, CFStringRef},
};
use parking_lot::Mutex;
use splice_proto::raw::{RawEvent, RawReport, MAX_DEVICES};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

type Ref = *mut c_void;
type DeviceCallback = unsafe extern "C" fn(Ref, i32, Ref, Ref);
type ReportCallback = unsafe extern "C" fn(Ref, i32, Ref, u32, u32, *mut u8, isize, u64);

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDCheckAccess(request: u32) -> u32;
    fn IOHIDManagerCreate(allocator: Ref, options: u32) -> Ref;
    fn IOHIDManagerSetDeviceMatching(
        manager: Ref,
        matching: core_foundation::dictionary::CFDictionaryRef,
    );
    fn IOHIDManagerRegisterDeviceMatchingCallback(
        manager: Ref,
        callback: DeviceCallback,
        context: Ref,
    );
    fn IOHIDManagerRegisterDeviceRemovalCallback(
        manager: Ref,
        callback: DeviceCallback,
        context: Ref,
    );
    fn IOHIDManagerRegisterInputReportWithTimeStampCallback(
        manager: Ref,
        callback: ReportCallback,
        context: Ref,
    );
    fn IOHIDManagerScheduleWithRunLoop(manager: Ref, runloop: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDManagerUnscheduleFromRunLoop(manager: Ref, runloop: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDManagerOpen(manager: Ref, options: u32) -> i32;
    fn IOHIDManagerClose(manager: Ref, options: u32) -> i32;
    fn IOHIDDeviceGetReport(
        device: Ref,
        kind: u32,
        id: isize,
        report: *mut u8,
        length: *mut isize,
    ) -> i32;
    fn IOHIDDeviceGetProperty(device: Ref, key: CFStringRef) -> CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: unsafe extern "C" fn(Ref, u32, Ref, Ref) -> Ref,
        context: Ref,
    ) -> core_foundation::mach_port::CFMachPortRef;
}

pub struct HidCapture {
    shared: Arc<MacShared>,
    tap: Arc<TapState>,
    state: Mutex<State>,
    media_recreate: AtomicBool,
}

struct Device {
    id: u64,
    name: String,
    decoder: Decoder,
    error: Option<String>,
}

#[derive(Default)]
struct State {
    devices: BTreeMap<usize, Device>,
    next_device: u64,
    sequence: u64,
    output: Option<Output>,
    error: Option<String>,
    ready: bool,
    media_ready: bool,
    rejected: BTreeMap<usize, String>,
}

struct Output {
    reports: mpsc::Sender<crate::raw::CapturedReport>,
    operation: Arc<crate::raw::RawOperation>,
}

impl State {
    fn held_keys(&self) -> std::collections::BTreeSet<u16> {
        self.devices
            .values()
            .flat_map(|d| d.decoder.snapshot())
            .filter_map(|event| match event {
                RawEvent::Key {
                    code,
                    pressed: true,
                } => Some(code),
                _ => None,
            })
            .collect()
    }

    fn readiness(&self) -> Result<()> {
        if let Some(error) = &self.error {
            return Err(PlatformError::Unavailable(error.clone()));
        }
        if !self.ready {
            return Err(PlatformError::Unavailable(
                "HID discovery is still starting".into(),
            ));
        }
        if !self.media_ready {
            return Err(PlatformError::Unavailable(
                "Raw media-key suppression is unavailable; check Accessibility permission".into(),
            ));
        }
        if let Some(error) = self.rejected.values().next() {
            return Err(PlatformError::Unavailable(error.clone()));
        }
        if let Some(error) = self.devices.values().find_map(|d| d.error.as_ref()) {
            return Err(PlatformError::Unavailable(error.clone()));
        }
        if !self.devices.values().any(|d| d.decoder.mouse)
            || !self.devices.values().any(|d| d.decoder.keyboard)
        {
            return Err(PlatformError::Unavailable("Raw input needs a relative HID mouse and a HID keyboard. Check Input Monitoring permission and device connections.".into()));
        }
        Ok(())
    }
}

impl HidCapture {
    pub fn spawn(shared: Arc<MacShared>, tap: Arc<TapState>) -> Arc<Self> {
        let capture = Arc::new(Self {
            shared,
            tap,
            state: Mutex::new(State::default()),
            media_recreate: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&capture);
        std::thread::Builder::new()
            .name("splice-raw-hid".into())
            .spawn(move || {
                let Some(capture) = weak.upgrade() else {
                    return;
                };
                if let Err(error) = run(&capture) {
                    let reason = format!("Mac raw capture: {error}");
                    capture.state.lock().error = Some(reason.clone());
                    capture.fail(reason);
                }
            })
            .expect("spawning HID capture thread");
        capture
    }

    fn fail(&self, reason: String) {
        let mut state = self.state.lock();
        self.fail_locked(&mut state, reason);
    }

    fn fail_locked(&self, state: &mut State, reason: String) {
        let output = state.output.take();
        self.tap
            .raw_enabled
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(Output { reports, operation }) = output {
            operation.fail(reason);
            drop(reports);
            self.tap.end(None);
            self.shared.emit(PlatformEvent::RawCaptureFailed(operation));
        } else {
            tracing::warn!(%reason, "Mac raw capture unavailable");
        }
    }

    fn report_error(&self, device: Ref, reason: &str) {
        let mut state = self.state.lock();
        let Some(device) = state.devices.get_mut(&(device as usize)) else {
            return;
        };
        device.error = Some(reason.into());
        self.fail_locked(&mut state, reason.into());
    }

    fn release_shortcut_keys(&self, state: &State, removed: &Device) {
        let held = state.held_keys();
        for event in removed.decoder.snapshot() {
            if let RawEvent::Key { code, .. } = event {
                if !held.contains(&code) {
                    self.tap.release_shortcut_key(code);
                }
            }
        }
    }

    fn send(
        &self,
        state: &mut State,
        device: u64,
        events: Vec<RawEvent>,
        captured_us: u64,
    ) -> std::result::Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        let Some(output) = &state.output else {
            return Ok(());
        };
        let report = RawReport {
            device,
            sequence: state.sequence,
            captured_us,
            events,
        };
        report.validate().map_err(str::to_owned)?;
        output
            .reports
            .try_send(report.into())
            .map_err(|e| format!("Raw input could not keep up; capture released: {e}"))?;
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or("raw report sequence exhausted")?;
        Ok(())
    }
}

impl RawCapture for HidCapture {
    fn prepare(&self) -> Result<()> {
        if unsafe { IOHIDCheckAccess(1) } != 0 {
            return Err(PlatformError::Permission(
                "Grant Splice Input Monitoring permission and restart it".into(),
            ));
        }
        self.state.lock().readiness()?;
        if super::secure_input_status().is_some() {
            return Err(PlatformError::Permission(
                "Secure Input prevents raw keyboard capture".into(),
            ));
        }
        if !self.tap.available() {
            return Err(PlatformError::Permission(
                "Raw input needs an active Accessibility event tap for local suppression".into(),
            ));
        }
        Ok(())
    }

    fn begin(
        &self,
        output: mpsc::Sender<crate::raw::CapturedReport>,
        edge: Option<u32>,
        operation: Arc<crate::raw::RawOperation>,
    ) -> Result<()> {
        self.prepare()?;
        let mut state = self.state.lock();
        state.readiness()?;
        if state.output.is_some() {
            return Err(PlatformError::Unavailable(
                "raw capture is already active".into(),
            ));
        }
        if edge.is_some_and(|id| !self.tap.touches_edge(id)) {
            return Err(PlatformError::Unavailable(
                "Edge crossing cancelled because the pointer left the edge".into(),
            ));
        }
        self.tap.begin()?;
        self.tap
            .raw_enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        state.sequence = 0;
        state.output = Some(Output {
            reports: output,
            operation,
        });
        let snapshots: Vec<_> = state.devices.values().map(|d| (d.id, d.decoder.snapshot().into_iter().filter(|e| !matches!(e, RawEvent::Key { code, .. } if self.tap.shortcut_suppressed(*code))).collect())).collect();
        for (device, events) in snapshots {
            if let Err(error) = self.send(&mut state, device, events, crate::raw::clock::now_us()) {
                state.output = None;
                self.tap.end(None);
                return Err(PlatformError::Unavailable(error));
            }
        }
        Ok(())
    }

    fn end(&self) {
        let mut state = self.state.lock();
        let active = state.output.take().is_some();
        self.tap
            .raw_enabled
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if active {
            self.tap.end(None);
        }
    }
}

unsafe fn property(device: Ref, name: &str) -> Option<CFType> {
    let raw = IOHIDDeviceGetProperty(device, CFString::new(name).as_concrete_TypeRef());
    (!raw.is_null()).then(|| CFType::wrap_under_get_rule(raw))
}

unsafe fn read_decoder(device: Ref, result: i32) -> anyhow::Result<Decoder> {
    anyhow::ensure!(result == 0, "HID device discovery failed: {result:#x}");
    let descriptor = property(device, "ReportDescriptor")
        .and_then(|v| v.downcast::<CFData>())
        .ok_or_else(|| anyhow::anyhow!("HID device has no report descriptor"))?;
    let mut decoder = Decoder::new(descriptor.bytes())?;
    for (id, length) in decoder.required_features() {
        let prefix = usize::from(id != 0);
        let mut bytes = vec![0; length + prefix];
        let mut count = bytes.len() as isize;
        let result = IOHIDDeviceGetReport(device, 2, id as isize, bytes.as_mut_ptr(), &mut count);
        anyhow::ensure!(
            result == 0 && count == bytes.len() as isize,
            "cannot read HID wheel resolution ({result:#x})"
        );
        anyhow::ensure!(
            id == 0 || bytes[0] == id,
            "wheel feature report ID mismatch"
        );
        decoder.feature(id, &bytes[prefix..])?;
    }
    Ok(decoder)
}

unsafe extern "C" fn added(context: Ref, result: i32, _: Ref, device: Ref) {
    let capture = &*(context as *const HidCapture);
    let name = property(device, "Product")
        .and_then(|v| v.downcast::<CFString>())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "Unnamed HID device".into());
    match read_decoder(device, result) {
        Ok(decoder) => {
            let mut state = capture.state.lock();
            if state.devices.contains_key(&(device as usize)) {
                return;
            }
            if state.devices.len() == MAX_DEVICES {
                state
                    .rejected
                    .insert(device as usize, "Too many HID input devices".into());
                drop(state);
                capture.fail("Too many HID input devices".into());
                return;
            }
            state.rejected.remove(&(device as usize));
            state.next_device += 1;
            let id = state.next_device;
            state.devices.insert(
                device as usize,
                Device {
                    id,
                    name,
                    decoder,
                    error: None,
                },
            );
        }
        Err(error) => {
            let reason = format!("Unsupported HID input device {name}: {error:#}");
            capture
                .state
                .lock()
                .rejected
                .insert(device as usize, reason.clone());
            capture.fail(reason);
        }
    }
}

unsafe extern "C" fn removed(context: Ref, _: i32, _: Ref, device: Ref) {
    let capture = &*(context as *const HidCapture);
    let mut state = capture.state.lock();
    state.rejected.remove(&(device as usize));
    if let Some(device) = state.devices.remove(&(device as usize)) {
        capture.release_shortcut_keys(&state, &device);
        if let Err(error) = capture.send(
            &mut state,
            device.id,
            vec![RawEvent::Removed],
            crate::raw::clock::now_us(),
        ) {
            capture.fail_locked(&mut state, error);
            return;
        }
        if state.output.is_some()
            && (!state.devices.values().any(|d| d.decoder.mouse)
                || !state.devices.values().any(|d| d.decoder.keyboard))
        {
            capture.fail_locked(&mut state, "Raw mouse or keyboard disconnected".into());
        }
    }
}

unsafe extern "C" fn report(
    context: Ref,
    result: i32,
    sender: Ref,
    kind: u32,
    id: u32,
    bytes: *mut u8,
    len: isize,
    timestamp: u64,
) {
    let capture = &*(context as *const HidCapture);
    if result != 0 || kind != 0 || bytes.is_null() || !(1..=4096).contains(&len) || id > 255 {
        capture.report_error(sender, "HID input report failed or exceeded its limits");
        return;
    }
    let bytes = std::slice::from_raw_parts(bytes, len as usize);
    let payload = if id == 0 {
        bytes
    } else if bytes[0] == id as u8 {
        &bytes[1..]
    } else {
        capture.report_error(sender, "HID report ID does not match its data");
        return;
    };
    let mut state = capture.state.lock();
    let Some(device) = state.devices.get_mut(&(sender as usize)) else {
        return;
    };
    let device_id = device.id;
    let mut events = match device.decoder.decode(id as u8, payload) {
        Ok(events) => {
            device.error = None;
            events
        }
        Err(error) => {
            let reason = format!("HID input device {}: {error:#}", device.name);
            device.error = Some(reason.clone());
            if state.output.is_some() {
                capture.fail_locked(&mut state, reason);
            }
            return;
        }
    };
    let held = if events.iter().any(|e| matches!(e, RawEvent::Key { .. })) {
        state.held_keys()
    } else {
        Default::default()
    };
    if events
        .iter()
        .any(|e| matches!(e, RawEvent::Key { pressed: true, .. }))
        && capture.tap.panic_from_hid(&held)
    {
        state.output = None;
        return;
    }
    if events.contains(&RawEvent::Key {
        code: 88,
        pressed: true,
    }) && held.contains(&29)
        && held.contains(&56)
    {
        capture.tap.switch_target(crate::raw::shortcut::Stream::Hid);
    }
    events.retain(|event| {
        if let RawEvent::Key { code, pressed } = event {
            let suppressed = capture.tap.shortcut_suppressed(*code);
            if !pressed && !held.contains(code) {
                capture.tap.release_shortcut_key(*code);
            }
            return !suppressed;
        }
        true
    });
    if capture
        .tap
        .raw_enabled
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        if let Err(error) = capture.send(
            &mut state,
            device_id,
            events,
            crate::raw::clock::mach_us(timestamp),
        ) {
            capture.fail_locked(&mut state, error);
        }
    }
}

unsafe extern "C" fn media(_: Ref, kind: u32, event: Ref, context: Ref) -> Ref {
    let capture = &*(context as *const HidCapture);
    if kind >= 0xffff_fffe {
        capture.state.lock().media_ready = false;
        capture.media_recreate.store(true, Ordering::SeqCst);
        capture.fail("Raw media-key suppression stopped; input released".into());
        return event;
    }
    if capture
        .tap
        .raw_enabled
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        std::ptr::null_mut()
    } else {
        event
    }
}

struct MediaTap {
    port: core_foundation::mach_port::CFMachPort,
    source: core_foundation::runloop::CFRunLoopSource,
}

impl MediaTap {
    unsafe fn new(context: Ref) -> Option<Self> {
        let port = CGEventTapCreate(1, 0, 0, 1 << 14, media, context);
        if port.is_null() {
            return None;
        }
        let port = core_foundation::mach_port::CFMachPort::wrap_under_create_rule(port);
        let source = port.create_runloop_source(0).ok()?;
        CFRunLoop::get_current().add_source(&source, kCFRunLoopDefaultMode);
        super::ffi::CGEventTapEnable(port.as_concrete_TypeRef(), true);
        Some(Self { port, source })
    }

    fn enabled(&self) -> bool {
        unsafe { super::ffi::CGEventTapIsEnabled(self.port.as_concrete_TypeRef()) }
    }
}

impl Drop for MediaTap {
    fn drop(&mut self) {
        unsafe {
            super::ffi::CGEventTapEnable(self.port.as_concrete_TypeRef(), false);
            CFRunLoop::get_current().remove_source(&self.source, kCFRunLoopDefaultMode);
        }
    }
}

fn run(capture: &Arc<HidCapture>) -> anyhow::Result<()> {
    unsafe {
        let context = Arc::as_ptr(capture) as Ref;
        let manager = HidManager::open(context, added, removed, report)?;
        let mut media = MediaTap::new(context);
        capture.state.lock().media_ready = media.as_ref().is_some_and(MediaTap::enabled);
        let mut lifecycle = capture.tap.lifecycle.load(Ordering::SeqCst);
        let mut last_attempt = Instant::now();
        capture.state.lock().ready = true;
        while Arc::strong_count(capture) > 1 && !capture.shared.tx.is_closed() {
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_millis(20), false);
            let current_lifecycle = capture.tap.lifecycle.load(Ordering::SeqCst);
            if current_lifecycle != lifecycle
                || capture.media_recreate.swap(false, Ordering::SeqCst)
                || media.as_ref().is_some_and(|tap| !tap.enabled())
            {
                if current_lifecycle != lifecycle {
                    for device in capture.state.lock().devices.values_mut() {
                        device.decoder.reset();
                    }
                }
                lifecycle = current_lifecycle;
                capture.state.lock().media_ready = false;
                capture.fail("macOS input suppression changed; input released".into());
                media = None;
                last_attempt = Instant::now() - Duration::from_secs(1);
            }
            if media.is_none() && last_attempt.elapsed() >= Duration::from_secs(1) {
                last_attempt = Instant::now();
                media = MediaTap::new(context);
                capture.state.lock().media_ready = media.as_ref().is_some_and(MediaTap::enabled);
            }
            let active = {
                let mut state = capture.state.lock();
                if !capture
                    .tap
                    .raw_enabled
                    .load(std::sync::atomic::Ordering::SeqCst)
                    && !capture
                        .tap
                        .raw_switching
                        .load(std::sync::atomic::Ordering::SeqCst)
                {
                    state.output = None;
                }
                state.output.is_some()
                    && capture
                        .tap
                        .raw_enabled
                        .load(std::sync::atomic::Ordering::SeqCst)
            };
            if active
                && (super::secure_input_status().is_some()
                    || IOHIDCheckAccess(1) != 0
                    || !capture.tap.available())
            {
                capture.fail(
                    "Raw input permission or suppression was lost, or Secure Input became active"
                        .into(),
                );
            }
        }
        capture.end();
        drop(media);
        drop(manager);
        Ok(())
    }
}

struct HidManager {
    owner: CFType,
    runloop: CFRunLoop,
}

impl HidManager {
    unsafe fn discover() -> anyhow::Result<Self> {
        let manager = IOHIDManagerCreate(std::ptr::null_mut(), 0);
        anyhow::ensure!(!manager.is_null(), "cannot create HID manager");
        let manager = Self {
            owner: CFType::wrap_under_create_rule(manager as CFTypeRef),
            runloop: CFRunLoop::get_current(),
        };
        let matches: Vec<_> = [(1, 2), (1, 6), (12, 1)]
            .into_iter()
            .map(|(page, usage)| {
                CFDictionary::from_CFType_pairs(&[
                    (
                        CFString::new("DeviceUsagePage").as_CFType(),
                        CFNumber::from(page).as_CFType(),
                    ),
                    (
                        CFString::new("DeviceUsage").as_CFType(),
                        CFNumber::from(usage).as_CFType(),
                    ),
                ])
            })
            .collect();
        let matching = CFDictionary::from_CFType_pairs(&[(
            CFString::new("DeviceUsagePairs").as_CFType(),
            CFArray::from_CFTypes(&matches).as_CFType(),
        )]);
        let handle = manager.owner.as_CFTypeRef() as Ref;
        IOHIDManagerSetDeviceMatching(handle, matching.as_concrete_TypeRef());
        Ok(manager)
    }

    unsafe fn open(
        context: Ref,
        added: DeviceCallback,
        removed: DeviceCallback,
        report: ReportCallback,
    ) -> anyhow::Result<Self> {
        let manager = Self::discover()?;
        let handle = manager.owner.as_CFTypeRef() as Ref;
        IOHIDManagerRegisterDeviceMatchingCallback(handle, added, context);
        IOHIDManagerRegisterDeviceRemovalCallback(handle, removed, context);
        IOHIDManagerRegisterInputReportWithTimeStampCallback(handle, report, context);
        IOHIDManagerScheduleWithRunLoop(
            handle,
            manager.runloop.as_concrete_TypeRef(),
            kCFRunLoopDefaultMode,
        );
        let result = IOHIDManagerOpen(handle, 0);
        anyhow::ensure!(result == 0, "HID access denied ({result:#x}); grant Splice Input Monitoring permission and restart it");
        Ok(manager)
    }
}

impl Drop for HidManager {
    fn drop(&mut self) {
        unsafe {
            let handle = self.owner.as_CFTypeRef() as Ref;
            IOHIDManagerUnscheduleFromRunLoop(
                handle,
                self.runloop.as_concrete_TypeRef(),
                kCFRunLoopDefaultMode,
            );
            IOHIDManagerClose(handle, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::set::{CFSet, CFSetGetValues, CFSetRef};
    use parking_lot::RwLock;

    extern "C" {
        fn IOHIDManagerCopyDevices(manager: Ref) -> CFSetRef;
        fn IOHIDDeviceGetService(device: Ref) -> u32;
        fn IORegistryEntryGetRegistryEntryID(service: u32, id: *mut u64) -> i32;
    }

    #[test]
    #[ignore = "requires an attached composite HID device; enumerates without capturing input"]
    fn native_hid_discovery_subscribes_once_per_service() {
        unsafe {
            let manager = HidManager::discover().unwrap();
            let raw = IOHIDManagerCopyDevices(manager.owner.as_CFTypeRef() as Ref);
            assert!(!raw.is_null(), "no attached HID input devices");
            let devices: CFSet = CFSet::wrap_under_create_rule(raw);
            let mut values = vec![std::ptr::null(); devices.len()];
            CFSetGetValues(devices.as_concrete_TypeRef(), values.as_mut_ptr());
            let mut ids = std::collections::BTreeSet::new();
            let mut composite = false;
            for device in values {
                let device = device as Ref;
                composite |= property(device, "DeviceUsagePairs")
                    .and_then(|p| p.downcast::<CFArray>())
                    .is_some_and(|p| p.len() > 1);
                let mut id = 0;
                assert_eq!(
                    IORegistryEntryGetRegistryEntryID(IOHIDDeviceGetService(device), &mut id),
                    0
                );
                assert!(
                    ids.insert(id),
                    "duplicate HID subscription for registry service {id}"
                );
            }
            assert!(composite, "no attached composite HID device");
        }
    }

    fn capture() -> HidCapture {
        let (tx, _) = mpsc::unbounded_channel();
        let shared = Arc::new(MacShared {
            tx,
            displays: RwLock::new(Vec::new()),
            health: Mutex::new(Default::default()),
        });
        HidCapture {
            tap: TapState::new(shared.clone(), Vec::new()),
            shared,
            state: Mutex::new(State::default()),
            media_recreate: AtomicBool::new(false),
        }
    }

    #[test]
    fn capture_failures_keep_the_operation_that_failed() {
        let mut capture = capture();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let shared = Arc::new(MacShared {
            tx,
            displays: RwLock::new(Vec::new()),
            health: Mutex::new(Default::default()),
        });
        capture.tap = TapState::new(shared.clone(), Vec::new());
        capture.shared = shared;
        let old = Arc::default();
        let (reports, mut reports_rx) = mpsc::channel(1);
        capture.state.lock().output = Some(Output {
            reports,
            operation: Arc::clone(&old),
        });
        capture.fail("old device failure".into());
        assert!(reports_rx.try_recv().is_err());
        let new = Arc::default();
        let (reports, _reports_rx) = mpsc::channel(1);
        capture.state.lock().output = Some(Output {
            reports,
            operation: Arc::clone(&new),
        });
        let PlatformEvent::RawCaptureFailed(failed) = rx.try_recv().unwrap() else {
            panic!("unscoped capture failure")
        };
        assert!(Arc::ptr_eq(&failed, &old));
        assert_eq!(failed.error().as_deref(), Some("old device failure"));
        assert!(new.error().is_none());
        assert!(capture.state.lock().output.is_some());
    }

    #[test]
    fn hid_callback_preserves_native_time_through_decoding_and_enqueue() {
        let capture = capture();
        let decoder = Decoder::new(include_bytes!(
            "../../tests/fixtures/hid/logitech-c548-keyboard.bin"
        ))
        .unwrap();
        let (reports, mut rx) = mpsc::channel(4);
        {
            let mut state = capture.state.lock();
            state.devices.insert(
                1,
                Device {
                    id: 1,
                    name: "keyboard".into(),
                    decoder,
                    error: None,
                },
            );
            state.output = Some(Output {
                reports,
                operation: Arc::default(),
            });
        }
        capture.tap.raw_enabled.store(true, Ordering::SeqCst);
        let ticks = unsafe { crate::raw::clock::mach::mach_absolute_time() } - 1_000_000;
        let native_us = crate::raw::clock::mach_us(ticks);
        unsafe {
            report(
                &capture as *const HidCapture as Ref,
                0,
                1usize as Ref,
                0,
                0,
                [0u8, 0, 4, 0, 0, 0, 0, 0].as_mut_ptr(),
                8,
                ticks,
            )
        };
        let captured = rx.try_recv().unwrap();
        assert_eq!(captured.report.captured_us, native_us);
        assert!(captured.enqueued_us > native_us);
        assert_eq!(
            captured.report.events,
            [RawEvent::Key {
                code: 30,
                pressed: true
            }]
        );
        capture.end();
    }

    #[test]
    fn shortcut_release_waits_for_the_last_keyboard_and_handles_removal() {
        let capture = capture();
        for id in [1, 2] {
            let mut decoder = Decoder::new(include_bytes!(
                "../../tests/fixtures/hid/logitech-c548-keyboard.bin"
            ))
            .unwrap();
            decoder.decode(0, &[1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
            capture.state.lock().devices.insert(
                id,
                Device {
                    id: id as u64,
                    name: "keyboard".into(),
                    decoder,
                    error: None,
                },
            );
        }
        capture.tap.switch_target(crate::raw::shortcut::Stream::Hid);
        let context = &capture as *const HidCapture as Ref;
        unsafe {
            report(
                context,
                0,
                1usize as Ref,
                0,
                0,
                [0u8; 8].as_mut_ptr(),
                8,
                crate::raw::clock::mach::mach_absolute_time(),
            )
        };
        assert!(capture.tap.shortcut_suppressed(29));
        unsafe { removed(context, 0, std::ptr::null_mut(), 2usize as Ref) };
        assert!(!capture.tap.shortcut_suppressed(29));
    }

    #[test]
    fn media_timeout_invalidates_suppression_and_requests_recreation_without_a_sticky_error() {
        let capture = capture();
        capture.state.lock().media_ready = true;
        capture.tap.raw_enabled.store(true, Ordering::SeqCst);
        unsafe {
            media(
                std::ptr::null_mut(),
                0xffff_fffe,
                std::ptr::null_mut(),
                &capture as *const HidCapture as Ref,
            )
        };
        assert!(!capture.state.lock().media_ready);
        assert!(capture.media_recreate.load(Ordering::SeqCst));
        assert!(capture.state.lock().error.is_none());
        assert!(!capture.tap.raw_enabled.load(Ordering::SeqCst));
    }
}
