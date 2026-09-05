use super::{tap::TapState, MacShared};
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
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

type Ref = *mut c_void;
type DeviceCallback = unsafe extern "C" fn(Ref, i32, Ref, Ref);
type ReportCallback = unsafe extern "C" fn(Ref, i32, Ref, u32, u32, *mut u8, isize);

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDCheckAccess(request: u32) -> u32;
    fn IOHIDManagerCreate(allocator: Ref, options: u32) -> Ref;
    fn IOHIDManagerSetDeviceMatchingMultiple(
        manager: Ref,
        matching: core_foundation::array::CFArrayRef,
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
    fn IOHIDManagerRegisterInputReportCallback(
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
    origin: Instant,
}

struct Device {
    id: u64,
    decoder: Decoder,
    error: Option<String>,
}

#[derive(Default)]
struct State {
    devices: BTreeMap<usize, Device>,
    next_device: u64,
    sequence: u64,
    output: Option<mpsc::Sender<RawReport>>,
    error: Option<String>,
    ready: bool,
    rejected: BTreeMap<usize, String>,
}

impl State {
    fn readiness(&self) -> Result<()> {
        if let Some(error) = &self.error {
            return Err(PlatformError::Unavailable(error.clone()));
        }
        if !self.ready {
            return Err(PlatformError::Unavailable(
                "HID discovery is still starting".into(),
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
            origin: Instant::now(),
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
        let active = state.output.take().is_some();
        self.tap
            .raw_enabled
            .store(false, std::sync::atomic::Ordering::SeqCst);
        drop(state);
        if active {
            self.tap.end(None);
        }
        self.shared.emit(PlatformEvent::RawError(reason));
    }

    fn report_error(&self, device: Ref, reason: &str) {
        let mut state = self.state.lock();
        let Some(device) = state.devices.get_mut(&(device as usize)) else {
            return;
        };
        device.error = Some(reason.into());
        drop(state);
        self.fail(reason.into());
    }

    fn send(
        &self,
        state: &mut State,
        device: u64,
        events: Vec<RawEvent>,
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
            captured_us: self.origin.elapsed().as_micros() as u64,
            events,
        };
        report.validate().map_err(str::to_owned)?;
        output
            .try_send(report)
            .map_err(|e| format!("Raw input could not keep up; capture released: {e}"))?;
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or("raw report sequence exhausted")?;
        Ok(())
    }
}

impl RawCapture for HidCapture {
    fn readiness(&self) -> Result<()> {
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

    fn begin(&self, output: mpsc::Sender<RawReport>, edge: Option<u32>) -> Result<()> {
        self.readiness()?;
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
        self.tap.begin();
        self.tap
            .raw_enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        state.sequence = 0;
        state.output = Some(output);
        let snapshots: Vec<_> = state.devices.values().map(|d| (d.id, d.decoder.snapshot().into_iter().filter(|e| !matches!(e, RawEvent::Key { code, .. } if self.tap.shortcut_suppressed(*code))).collect())).collect();
        for (device, events) in snapshots {
            if let Err(error) = self.send(&mut state, device, events) {
                state.output = None;
                drop(state);
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
        drop(state);
        if active {
            self.tap.end(None);
        }
    }
}

unsafe fn property(device: Ref, name: &str) -> Option<CFType> {
    let raw = IOHIDDeviceGetProperty(device, CFString::new(name).as_concrete_TypeRef());
    (!raw.is_null()).then(|| CFType::wrap_under_get_rule(raw))
}

unsafe extern "C" fn added(context: Ref, result: i32, _: Ref, device: Ref) {
    let capture = &*(context as *const HidCapture);
    let parse = || -> anyhow::Result<Decoder> {
        anyhow::ensure!(result == 0, "HID device discovery failed: {result:#x}");
        let descriptor = property(device, "ReportDescriptor")
            .and_then(|v| v.downcast::<CFData>())
            .ok_or_else(|| anyhow::anyhow!("HID device has no report descriptor"))?;
        let mut decoder = Decoder::new(descriptor.bytes())?;
        for (id, length) in decoder.required_features() {
            let prefix = usize::from(id != 0);
            let mut bytes = vec![0; length + prefix];
            let mut count = bytes.len() as isize;
            let result =
                IOHIDDeviceGetReport(device, 2, id as isize, bytes.as_mut_ptr(), &mut count);
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
    };
    match parse() {
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
                    decoder,
                    error: None,
                },
            );
        }
        Err(error) => {
            let reason = format!("Unsupported HID input device: {error:#}");
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
        if let Err(error) = capture.send(&mut state, device.id, vec![RawEvent::Removed]) {
            drop(state);
            capture.fail(error);
            return;
        }
        if state.output.is_some()
            && (!state.devices.values().any(|d| d.decoder.mouse)
                || !state.devices.values().any(|d| d.decoder.keyboard))
        {
            drop(state);
            capture.fail("Raw mouse or keyboard disconnected".into());
        }
    }
}

unsafe extern "C" fn report(
    context: Ref,
    result: i32,
    sender: Ref,
    _: u32,
    id: u32,
    bytes: *mut u8,
    len: isize,
) {
    let capture = &*(context as *const HidCapture);
    if result != 0 || bytes.is_null() || !(1..=4096).contains(&len) || id > 255 {
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
            let reason = format!("HID input device {device_id}: {error:#}");
            device.error = Some(reason.clone());
            let active = state.output.is_some();
            drop(state);
            if active {
                capture.fail(reason);
            }
            return;
        }
    };
    if events.contains(&RawEvent::Key {
        code: 88,
        pressed: true,
    }) {
        let keys: std::collections::BTreeSet<_> = state
            .devices
            .values()
            .flat_map(|d| d.decoder.snapshot())
            .filter_map(|e| match e {
                RawEvent::Key {
                    code,
                    pressed: true,
                } => Some(code),
                _ => None,
            })
            .collect();
        if keys.contains(&29) && keys.contains(&56) {
            capture.tap.switch_target(crate::raw::shortcut::Stream::Hid);
        }
    }
    events.retain(|event| {
        if let RawEvent::Key { code, pressed } = event {
            let suppressed = capture.tap.shortcut_suppressed(*code);
            if !pressed {
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
        if let Err(error) = capture.send(&mut state, device_id, events) {
            drop(state);
            capture.fail(error);
        }
    }
}

unsafe extern "C" fn media(_: Ref, kind: u32, event: Ref, context: Ref) -> Ref {
    let capture = &*(context as *const HidCapture);
    if kind >= 0xffff_fffe {
        capture.state.lock().error =
            Some("Raw media-key suppression tap stopped; restart Splice".into());
        capture.fail("Raw media-key suppression tap stopped; restart Splice".into());
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

fn run(capture: &Arc<HidCapture>) -> anyhow::Result<()> {
    unsafe {
        let manager = IOHIDManagerCreate(std::ptr::null_mut(), 0);
        anyhow::ensure!(!manager.is_null(), "cannot create HID manager");
        let manager_owner = CFType::wrap_under_create_rule(manager as CFTypeRef);
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
        let matching = CFArray::from_CFTypes(&matches);
        IOHIDManagerSetDeviceMatchingMultiple(manager, matching.as_concrete_TypeRef());
        let context = Arc::as_ptr(capture) as Ref;
        IOHIDManagerRegisterDeviceMatchingCallback(manager, added, context);
        IOHIDManagerRegisterDeviceRemovalCallback(manager, removed, context);
        IOHIDManagerRegisterInputReportCallback(manager, report, context);
        let runloop = CFRunLoop::get_current();
        IOHIDManagerScheduleWithRunLoop(
            manager,
            runloop.as_concrete_TypeRef(),
            kCFRunLoopDefaultMode,
        );
        let result = IOHIDManagerOpen(manager, 0);
        if result != 0 {
            IOHIDManagerUnscheduleFromRunLoop(
                manager,
                runloop.as_concrete_TypeRef(),
                kCFRunLoopDefaultMode,
            );
            anyhow::bail!("HID access denied ({result:#x}); grant Splice Input Monitoring permission and restart it");
        }
        let tap = CGEventTapCreate(1, 0, 0, 1 << 14, media, context);
        if tap.is_null() {
            IOHIDManagerClose(manager, 0);
            IOHIDManagerUnscheduleFromRunLoop(
                manager,
                runloop.as_concrete_TypeRef(),
                kCFRunLoopDefaultMode,
            );
            anyhow::bail!("cannot suppress local media keys; grant Accessibility permission");
        }
        let tap = core_foundation::mach_port::CFMachPort::wrap_under_create_rule(tap);
        let source = core_foundation::runloop::CFRunLoopSource::wrap_under_create_rule(
            core_foundation::mach_port::CFMachPortCreateRunLoopSource(
                std::ptr::null(),
                tap.as_concrete_TypeRef(),
                0,
            ),
        );
        runloop.add_source(&source, kCFRunLoopDefaultMode);
        capture.state.lock().ready = true;
        while Arc::strong_count(capture) > 1 {
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_millis(20), false);
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
        runloop.remove_source(&source, kCFRunLoopDefaultMode);
        IOHIDManagerClose(manager, 0);
        IOHIDManagerUnscheduleFromRunLoop(
            manager,
            runloop.as_concrete_TypeRef(),
            kCFRunLoopDefaultMode,
        );
        drop(manager_owner);
        Ok(())
    }
}
