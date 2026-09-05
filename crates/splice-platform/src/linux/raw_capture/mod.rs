pub(super) mod control;
mod report;
#[cfg(test)]
mod tests;
mod witness;

use super::{Shared, VIRTUAL_DEVICE_PREFIX};
use crate::{
    Capture, CaptureEvent, EdgeSpec, PlatformError, PlatformEvent, Result,
    raw::{RawCapture, shortcut::Stream},
};
use anyhow::{Context, ensure};
use evdev::{KeyCode, RelativeAxisCode as Rel, raw_stream::RawDevice};
use parking_lot::Mutex;
use splice_proto::{
    Vec2,
    raw::{MAX_DEVICES, RawEvent, RawReport},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Weak, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub struct DeviceCapture {
    shared: Arc<Shared>,
    desktop: Arc<dyn Capture>,
    runtime: tokio::runtime::Handle,
    state: Mutex<State>,
    origin: Instant,
}

#[derive(Default)]
struct State {
    devices: BTreeMap<PathBuf, Device>,
    failures: BTreeMap<PathBuf, String>,
    fatal: Option<String>,
    ready: bool,
    next_device: u64,
    output: Option<Output>,
    sequence: u64,
    desktop_snapshot: bool,
}

struct Output {
    reports: mpsc::Sender<RawReport>,
    operation: Arc<crate::raw::RawOperation>,
}

struct Device {
    id: u64,
    input: RawDevice,
    reports: report::Reports,
    mouse: bool,
    keyboard: bool,
}

impl DeviceCapture {
    pub fn spawn(shared: Arc<Shared>, desktop: Arc<dyn Capture>) -> Result<Arc<Self>> {
        let capture = Arc::new(Self {
            shared,
            desktop,
            runtime: tokio::runtime::Handle::current(),
            state: Mutex::new(State::default()),
            origin: Instant::now(),
        });
        let weak = Arc::downgrade(&capture);
        std::thread::Builder::new()
            .name("splice-raw-evdev".into())
            .spawn(move || monitor(weak))
            .map_err(|error| PlatformError::Other(error.into()))?;
        Ok(capture)
    }

    fn check(&self, state: &State) -> Result<()> {
        if let Some(reason) = &state.fatal {
            return Err(PlatformError::Unavailable(reason.clone()));
        }
        if !state.ready {
            return Err(PlatformError::Unavailable(
                "Linux raw device discovery is still starting".into(),
            ));
        }
        if let Some(reason) = state.failures.values().next() {
            return Err(PlatformError::Unavailable(reason.clone()));
        }
        if !state.devices.values().any(|d| d.mouse) || !state.devices.values().any(|d| d.keyboard) {
            return Err(PlatformError::Unavailable("Raw input requires an accessible physical relative mouse and keyboard; install the Splice input-device udev rule".into()));
        }
        if !self.shared.capture_control.active.load(Ordering::SeqCst) {
            return Err(PlatformError::Unavailable("Start Linux raw input by crossing a screen edge. An active Wayland capture session is required for local input suppression.".into()));
        }
        Ok(())
    }

    fn send(&self, state: &mut State, id: u64, events: Vec<RawEvent>) -> anyhow::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let Some(output) = &state.output else {
            return Ok(());
        };
        let report = RawReport {
            device: id,
            sequence: state.sequence,
            captured_us: self.origin.elapsed().as_micros() as u64,
            events,
        };
        report.validate().map_err(anyhow::Error::msg)?;
        output
            .reports
            .try_send(report)
            .context("Raw input queue overflowed or the connection closed; capture released")?;
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("raw report sequence exhausted"))?;
        Ok(())
    }

    fn fail(&self, state: &mut State, reason: String) {
        let output = state.output.take();
        if let Some(Output { reports, operation }) = output {
            operation.fail(reason.clone());
            drop(reports);
            self.shared.capture_control.release();
            if let Err(error) = self.runtime.block_on(self.desktop.end_capture(None)) {
                operation.fail(format!("{reason}; local capture release failed: {error}"));
            }
            self.shared.emit(PlatformEvent::RawCaptureFailed(operation));
        }
    }

    fn read(&self, state: &mut State, path: &Path, forward: bool) -> anyhow::Result<()> {
        for _ in 0..64 {
            let device = state.devices.get_mut(path).expect("polled device exists");
            let events: Vec<_> = match device.input.fetch_events() {
                Ok(events) => events.collect(),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "Raw device {} disconnected or access was revoked",
                            path.display()
                        )
                    });
                }
            };
            ensure!(!events.is_empty(), "raw device stream ended");
            let id = device.id;
            for event in events {
                let Some(mut report) = state
                    .devices
                    .get_mut(path)
                    .expect("device exists")
                    .reports
                    .push(event)?
                else {
                    continue;
                };
                let held: BTreeSet<_> = state
                    .devices
                    .values()
                    .flat_map(|d| d.reports.held.iter().copied())
                    .collect();
                if forward {
                    for event in &report {
                        if let Some(kind) = witness::Kind::raw(event) {
                            self.shared.capture_control.observe(
                                Stream::Hid,
                                kind,
                                self.shared.epoch.elapsed().as_millis() as u64,
                            );
                        }
                    }
                }
                let mut switched = false;
                report.retain(|event| {
                    if let RawEvent::Key { code, pressed } = event {
                        let (switch, suppressed) = self.shared.capture_control.raw_key(
                            *code,
                            *pressed,
                            &held,
                            self.shared.epoch.elapsed().as_millis() as u64,
                        );
                        switched |= switch;
                        return !suppressed;
                    }
                    true
                });
                if switched {
                    self.shared.emit(PlatformEvent::SwitchTarget);
                }
                if forward && !self.shared.capture_control.switching.load(Ordering::SeqCst) {
                    self.send(state, id, report)?;
                }
            }
        }
        anyhow::bail!("Raw input reader could not drain the device queue")
    }
}

impl RawCapture for DeviceCapture {
    fn prepare(&self) -> Result<()> {
        let mut state = self.state.lock();
        self.check(&state)?;
        let _emission = self.shared.emission.lock();
        self.shared
            .capture_control
            .raw_hold
            .store(true, Ordering::SeqCst);
        state.desktop_snapshot = true;
        Ok(())
    }

    fn begin(
        &self,
        output: mpsc::Sender<RawReport>,
        _edge: Option<u32>,
        operation: Arc<crate::raw::RawOperation>,
    ) -> Result<()> {
        let mut state = self.state.lock();
        self.check(&state)?;
        if state.output.is_some() {
            return Err(PlatformError::Unavailable(
                "raw capture already active".into(),
            ));
        }
        for path in state.devices.keys().cloned().collect::<Vec<_>>() {
            if let Err(error) = self.read(&mut state, &path, false) {
                state.failures.insert(
                    path.clone(),
                    format!("Raw input needs a fresh device stream: {error:#}"),
                );
                state.devices.remove(&path);
                return Err(PlatformError::Other(error));
            }
        }
        self.shared
            .capture_control
            .switching
            .store(false, Ordering::SeqCst);
        {
            let _emission = self.shared.emission.lock();
            self.shared
                .capture_control
                .raw_hold
                .store(true, Ordering::SeqCst);
        }
        state.sequence = 0;
        state.desktop_snapshot = false;
        state.output = Some(Output {
            reports: output,
            operation,
        });
        self.shared.capture_control.raw_begin();
        let snapshots: anyhow::Result<Vec<_>> = state.devices.values().map(|d| Ok((d.id, d.reports.snapshot()?.into_iter().filter(|ev| !matches!(ev, RawEvent::Key { code, .. } if self.shared.capture_control.suppressed(*code))).collect()))).collect();
        let result = snapshots.and_then(|snapshots| {
            for (id, snapshot) in snapshots {
                self.send(&mut state, id, snapshot)?;
            }
            Ok(())
        });
        if let Err(error) = result {
            state.output = None;
            self.shared.capture_control.raw_end();
            return Err(PlatformError::Other(error));
        }
        Ok(())
    }

    fn end(&self) {
        let mut state = self.state.lock();
        if state.output.take().is_some() {
            state.desktop_snapshot = true;
        }
        self.shared.capture_control.raw_end();
    }
}

#[async_trait::async_trait]
impl Capture for DeviceCapture {
    async fn set_edges(&self, edges: Vec<EdgeSpec>) -> Result<()> {
        self.desktop.set_edges(edges).await
    }

    async fn begin_capture(&self) -> Result<()> {
        if !self.shared.capture_control.active.load(Ordering::SeqCst) {
            return Err(PlatformError::Unavailable(
                "Wayland capture was released before the destination was ready".into(),
            ));
        }
        self.desktop.begin_capture().await?;
        self.shared
            .capture_control
            .switching
            .store(false, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            {
                let mut state = self.state.lock();
                let _emission = self.shared.emission.lock();
                if !self.shared.capture_control.active.load(Ordering::SeqCst) {
                    return Err(PlatformError::Unavailable(
                        "Wayland capture was released during the Desktop handoff".into(),
                    ));
                }
                if !state.desktop_snapshot {
                    return Ok(());
                }
                let mut keys = BTreeSet::new();
                let mut physical_button_held = false;
                for device in state.devices.values() {
                    let held = device
                        .input
                        .get_key_state()
                        .map_err(|error| PlatformError::Other(error.into()))?;
                    for key in held.iter() {
                        match report::key(key.0, true).map_err(PlatformError::Other)? {
                            RawEvent::Key { code, .. } => {
                                keys.insert(u32::from(code));
                            }
                            RawEvent::Button { .. } => physical_button_held = true,
                            _ => unreachable!(),
                        }
                    }
                }
                if let Some(events) = self
                    .shared
                    .capture_control
                    .desktop_snapshot(keys, physical_button_held)
                {
                    for event in events {
                        let _ = self
                            .shared
                            .tx
                            .send(PlatformEvent::Capture(CaptureEvent::Input(event)));
                    }
                    state.desktop_snapshot = false;
                    self.shared
                        .capture_control
                        .raw_hold
                        .store(false, Ordering::SeqCst);
                    return Ok(());
                }
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(PlatformError::Unavailable(
                        "Mouse button state did not synchronize with Wayland within 500 ms. Release buttons and cross the edge again before switching to Desktop".into(),
                    ));
                }
                _ = self.shared.capture_control.buttons_changed.notified() => {}
            }
        }
    }

    async fn end_capture(&self, pos: Option<Vec2>) -> Result<()> {
        self.shared.capture_control.release();
        RawCapture::end(self);
        self.state.lock().desktop_snapshot = false;
        self.desktop.end_capture(pos).await
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    sys: PathBuf,
    inode: u64,
}

fn physical_sys_path(sys: &Path) -> bool {
    !sys.starts_with("/sys/devices/virtual/input")
        && (!sys.starts_with("/sys/devices/virtual")
            || sys.starts_with("/sys/devices/virtual/misc/uhid"))
}

fn capability(bits: &str, code: usize) -> anyhow::Result<bool> {
    let words: Vec<_> = bits
        .split_whitespace()
        .rev()
        .map(|word| u64::from_str_radix(word, 16))
        .collect::<std::result::Result<_, _>>()?;
    Ok(words
        .get(code / 64)
        .is_some_and(|word| word & (1 << (code % 64)) != 0))
}

fn candidate(keys: &str, axes: &str) -> anyhow::Result<bool> {
    Ok((capability(axes, 0)? && capability(axes, 1)?)
        || (capability(keys, KeyCode::KEY_A.0 as usize)?
            && capability(keys, KeyCode::KEY_Z.0 as usize)?)
        || capability(keys, KeyCode::KEY_VOLUMEUP.0 as usize)?
        || capability(keys, KeyCode::KEY_PLAYPAUSE.0 as usize)?)
}

fn physical_paths() -> anyhow::Result<BTreeMap<PathBuf, Identity>> {
    let mut paths = BTreeMap::new();
    for entry in std::fs::read_dir("/dev/input")? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.as_encoded_bytes().starts_with(b"event") {
            continue;
        }
        let sys = std::fs::canonicalize(Path::new("/sys/class/input").join(&name))?;
        if !physical_sys_path(&sys) {
            continue;
        }
        let caps = sys.join("device/capabilities");
        if !candidate(
            &std::fs::read_to_string(caps.join("key"))?,
            &std::fs::read_to_string(caps.join("rel"))?,
        )? {
            continue;
        }
        paths.insert(
            entry.path(),
            Identity {
                sys,
                inode: entry.metadata()?.ino(),
            },
        );
    }
    Ok(paths)
}

fn open(path: &Path, id: u64) -> anyhow::Result<Option<Device>> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path).with_context(|| format!("Cannot read {}. Install packaging/linux/70-splice.rules and check device permissions", path.display()))?;
    let input = RawDevice::from_fd(file.into())?;
    if input
        .name()
        .is_some_and(|name| name.starts_with(VIRTUAL_DEVICE_PREFIX))
    {
        return Ok(None);
    }
    let axes = input.supported_relative_axes();
    let mouse = axes.is_some_and(|a| a.contains(Rel::REL_X) && a.contains(Rel::REL_Y));
    let keys = input.supported_keys();
    let keyboard = keys.is_some_and(|k| k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_Z));
    let consumer = keys
        .is_some_and(|k| k.contains(KeyCode::KEY_VOLUMEUP) || k.contains(KeyCode::KEY_PLAYPAUSE));
    if !mouse && !keyboard && !consumer {
        return Ok(None);
    }
    let high_resolution = [Rel::REL_HWHEEL_HI_RES, Rel::REL_WHEEL_HI_RES]
        .map(|axis| axes.is_some_and(|a| a.contains(axis)));
    let held = input.get_key_state()?.iter().map(|key| key.0).collect();
    Ok(Some(Device {
        id,
        input,
        reports: report::Reports::new(held, high_resolution),
        mouse,
        keyboard,
    }))
}

fn monitor(weak: Weak<DeviceCapture>) {
    let mut known = BTreeMap::new();
    let mut scan_at = Instant::now();
    loop {
        let Some(capture) = weak.upgrade() else {
            return;
        };
        if Arc::strong_count(&capture) == 1 {
            return;
        }
        if Instant::now() >= scan_at {
            let result = physical_paths();
            let mut state = capture.state.lock();
            match result {
                Ok(paths) => {
                    let changed = paths != known;
                    let active = state.output.is_some();
                    state.devices.retain(|p, _| {
                        paths
                            .get(p)
                            .is_some_and(|identity| known.get(p) == Some(identity))
                    });
                    state.failures.retain(|p, _| {
                        paths
                            .get(p)
                            .is_some_and(|identity| known.get(p) == Some(identity))
                    });
                    for (path, identity) in &paths {
                        if state.devices.contains_key(path)
                            || (known.get(path) == Some(identity)
                                && !state.failures.contains_key(path))
                        {
                            continue;
                        }
                        state.next_device += 1;
                        match open(path, state.next_device) {
                            Ok(Some(device)) if state.devices.len() < MAX_DEVICES => {
                                state.devices.insert(path.clone(), device);
                                state.failures.remove(path);
                            }
                            Ok(Some(_)) => {
                                state
                                    .failures
                                    .insert(path.clone(), "Too many raw input devices".into());
                            }
                            Ok(None) => {
                                state.failures.remove(path);
                            }
                            Err(error) => {
                                state.failures.insert(
                                    path.clone(),
                                    format!("Raw input unavailable: {error:#}"),
                                );
                            }
                        }
                    }
                    known = paths;
                    state.ready = true;
                    state.fatal = None;
                    if changed && active {
                        capture.fail(&mut state,"Input devices changed; raw capture released. Retry after devices are ready.".into());
                    }
                }
                Err(error) => {
                    state.fatal = Some(format!("Cannot discover raw input devices: {error:#}"));
                    capture.fail(&mut state, format!("Raw input discovery failed: {error:#}"));
                }
            }
            scan_at = Instant::now() + Duration::from_millis(250);
        }
        let mut state = capture.state.lock();
        let mut descriptors: Vec<_> = state
            .devices
            .iter()
            .map(|(p, d)| {
                (
                    p.clone(),
                    libc::pollfd {
                        fd: d.input.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                )
            })
            .collect();
        drop(state);
        let mut poll: Vec<_> = descriptors.iter().map(|(_, p)| *p).collect();
        let result = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, 50) };
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            let mut state = capture.state.lock();
            state.fatal = Some("Raw input device polling failed".into());
            capture.fail(&mut state, "Raw input device polling failed".into());
            return;
        }
        state = capture.state.lock();
        let forward =
            state.output.is_some() && capture.shared.capture_control.active.load(Ordering::SeqCst);
        let mut failed = None;
        for ((path, _), ready) in descriptors.drain(..).zip(poll) {
            if ready.revents == 0
                || !state
                    .devices
                    .get(&path)
                    .is_some_and(|d| d.input.as_raw_fd() == ready.fd)
            {
                continue;
            }
            if let Err(error) = capture.read(&mut state, &path, forward) {
                let reason = format!("Raw input stopped: {error:#}");
                state.failures.insert(path.clone(), reason.clone());
                state.devices.remove(&path);
                failed = Some(reason);
                break;
            }
        }
        if forward && failed.is_none() {
            failed = capture
                .shared
                .capture_control
                .failure(capture.shared.epoch.elapsed().as_millis() as u64);
        }
        if let Some(reason) = failed {
            capture.fail(&mut state, reason);
        }
    }
}
