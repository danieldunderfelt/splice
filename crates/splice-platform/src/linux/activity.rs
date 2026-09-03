//! Physical-activity monitor: read-only evdev reader over /dev/input/event*.
//!
//! Portal-injected input never appears on /dev/input, so any EV_KEY/EV_REL seen here IS
//! physical input — the engine uses it for source arbitration
//! (docs/research/wayland-input.md). Devices are opened non-exclusively (no EVIOCGRAB);
//! an inotify watch on /dev/input handles hotplug. Reading evdev needs the Splice udev
//! rule (packaging/linux/70-splice.rules, installed by the packages) or `input` group
//! membership: on EACCES the health report names the fix once and everything else
//! keeps running.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use evdev::{Device, EventType};
use futures::StreamExt;
use inotify::{EventMask, EventOwned, EventStream, Inotify, WatchMask};
use tokio::sync::mpsc;

use super::{PanicRelease, Shared, VIRTUAL_DEVICE_PREFIX};
use crate::PlatformEvent;

const INPUT_DIR: &str = "/dev/input";
/// PhysicalActivity is contractually debounced to ≥50 ms (lib.rs).
const DEBOUNCE: Duration = Duration::from_millis(50);
/// udev applies device-node permissions after the node appears, so hotplug opens retry
/// a few times before an EACCES is believed.
const HOTPLUG_RETRIES: u32 = 3;
const HOTPLUG_RETRY_DELAY: Duration = Duration::from_millis(200);
/// Remapper re-emission latency is single-digit milliseconds; the window is generous
/// because a missed physical event only delays a source claim to the next one.
const ECHO_WINDOW: Duration = Duration::from_millis(150);

pub fn spawn(shared: Arc<Shared>, panic: PanicRelease, panic_chord: Vec<u32>) {
    let _ = tokio::spawn(run(shared, panic, panic_chord));
}

#[derive(Debug)]
enum PhysicalEvent {
    Activity(PathBuf),
    Key { device: PathBuf, code: u32, pressed: bool },
    DeviceGone(PathBuf),
    /// The node could not be opened; a later ATTRIB (ACL change) event retries it.
    OpenFailed(PathBuf),
    Opened,
}

async fn run(shared: Arc<Shared>, panic: PanicRelease, panic_chord: Vec<u32>) {
    let (activity_tx, mut activity_rx) = mpsc::channel::<PhysicalEvent>(64);
    let eacces_reported = Arc::new(AtomicBool::new(false));
    let mut known: HashSet<PathBuf> = HashSet::new();

    for path in enumerate() {
        known.insert(path.clone());
        spawn_reader(path, activity_tx.clone(), &shared, &eacces_reported, 0);
    }

    let mut hotplug = match watch_input_dir() {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::warn!(error = %err, "cannot watch {INPUT_DIR}; device hotplug unmonitored");
            None
        }
    };

    // Leading-edge debounce: the first event after a quiet period emits immediately,
    // bursts collapse into one event per window.
    let mut last_emit = Instant::now().checked_sub(DEBOUNCE).unwrap_or_else(Instant::now);
    let mut held: HashMap<PathBuf, HashSet<u32>> = HashMap::new();
    let mut panic_latched = false;
    loop {
        tokio::select! {
            event = activity_rx.recv() => {
                let Some(event) = event else { return };
                let last_device = match &event {
                    PhysicalEvent::Activity(device) | PhysicalEvent::Key { device, .. } => {
                        Some(device.clone())
                    }
                    _ => None,
                };
                match event {
                    PhysicalEvent::Activity(_) => {}
                    PhysicalEvent::Key { device, code, pressed } => {
                        let keys = held.entry(device).or_default();
                        if pressed {
                            keys.insert(code);
                        } else {
                            keys.remove(&code);
                        }
                        let chord_down = !panic_chord.is_empty()
                            && panic_chord.iter().all(|wanted| {
                                held.values().any(|keys| keys.contains(wanted))
                            });
                        if chord_down && !panic_latched {
                            panic_latched = true;
                            panic.trigger();
                        } else if !chord_down {
                            panic_latched = false;
                        }
                    }
                    PhysicalEvent::DeviceGone(device) => {
                        held.remove(&device);
                        known.remove(&device);
                        panic_latched = false;
                        continue;
                    }
                    PhysicalEvent::OpenFailed(device) => {
                        known.remove(&device);
                        continue;
                    }
                    PhysicalEvent::Opened => {
                        if eacces_reported.swap(false, Ordering::Relaxed) {
                            shared.set_health(|h| h.activity_monitor = None);
                        }
                        continue;
                    }
                }
                let now = Instant::now();
                if now.duration_since(last_emit) >= DEBOUNCE {
                    last_emit = now;
                    tracing::debug!(device = ?last_device, "physical activity");
                    shared.emit(PlatformEvent::PhysicalActivity);
                }
            }
            event = next_hotplug(&mut hotplug) => {
                match event {
                    Some(Ok(event)) => {
                        if let Some(name) = event.name {
                            if name.as_encoded_bytes().starts_with(b"event") {
                                let path = PathBuf::from(INPUT_DIR).join(name);
                                if event.mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
                                    known.remove(&path);
                                    held.remove(&path);
                                } else if known.insert(path.clone()) {
                                    spawn_reader(path, activity_tx.clone(), &shared, &eacces_reported, HOTPLUG_RETRIES);
                                }
                            }
                        }
                    }
                    Some(Err(err)) => tracing::warn!(error = %err, "inotify error"),
                    // Stream ended; disable the branch (pending forever) and keep readers.
                    None => hotplug = None,
                }
            }
        }
    }
}

/// `None` hotplug stream means "no hotplug": pend forever so the select arm never fires.
async fn next_hotplug(
    stream: &mut Option<EventStream<[u8; 1024]>>,
) -> Option<io::Result<EventOwned>> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

fn watch_input_dir() -> io::Result<EventStream<[u8; 1024]>> {
    let inotify = Inotify::init()?;
    inotify
        .watches()
        .add(
            INPUT_DIR,
            WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::DELETE | WatchMask::MOVED_FROM | WatchMask::ATTRIB,
        )?;
    inotify.into_event_stream([0; 1024])
}

/// uinput-created devices live under /sys/devices/virtual/input; everything a remapper
/// or another injector emits comes from there.
fn is_software_device(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name() else { return false };
    let sys = std::path::Path::new("/sys/class/input").join(name);
    match std::fs::read_link(&sys) {
        Ok(target) => target.components().any(|c| c.as_os_str() == "virtual"),
        Err(_) => false,
    }
}

fn enumerate() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(INPUT_DIR) else {
        tracing::warn!("{INPUT_DIR} not readable; physical-activity monitor finds no devices");
        return out;
    };
    for entry in entries.flatten() {
        if entry.file_name().as_encoded_bytes().starts_with(b"event") {
            out.push(entry.path());
        }
    }
    out
}

/// One task per device: open read-only (evdev falls back from read+write), stream
/// events, and report EV_KEY/EV_REL as activity. `retries` tolerates the udev
/// permission race on hotplugged nodes.
fn spawn_reader(
    path: PathBuf,
    activity: mpsc::Sender<PhysicalEvent>,
    shared: &Arc<Shared>,
    eacces_reported: &Arc<AtomicBool>,
    retries: u32,
) {
    let shared = shared.clone();
    let eacces_reported = eacces_reported.clone();
    let _ = tokio::spawn(async move {
        let mut attempt = 0;
        let device = loop {
            match Device::open(&path) {
                Ok(device) => break device,
                Err(err) => {
                    if err.kind() == io::ErrorKind::PermissionDenied {
                        attempt += 1;
                        if attempt <= retries {
                            tokio::time::sleep(HOTPLUG_RETRY_DELAY).await;
                            continue;
                        }
                        // Report exactly once across all devices and hotplug events.
                        if !eacces_reported.swap(true, Ordering::Relaxed) {
                            shared.set_health(|h| {
                                h.activity_monitor = Some(format!(
                                    "cannot read {}: permission denied — install the Splice udev \
                                     rule (packaging/linux/70-splice.rules, see docs/linux-setup.md); \
                                     source auto-switching is limited on this machine",
                                    path.display()
                                ));
                            });
                        }
                    } else {
                        tracing::warn!(path = %path.display(), error = %err, "cannot open input device");
                    }
                    let _ = activity.send(PhysicalEvent::OpenFailed(path.clone())).await;
                    return;
                }
            }
        };
        if device.name().is_some_and(|name| name.starts_with(VIRTUAL_DEVICE_PREFIX)) {
            let _ = activity.send(PhysicalEvent::OpenFailed(path.clone())).await;
            return;
        }
        let _ = activity.send(PhysicalEvent::Opened).await;
        let software = is_software_device(&path);
        let mut stream = match device.into_event_stream() {
            Ok(stream) => stream,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "cannot stream input device");
                let _ = activity.send(PhysicalEvent::DeviceGone(path.clone())).await;
                return;
            }
        };
        // evdev's futures::Stream impl is behind the (disabled) stream-trait feature;
        // the inherent next_event() errors are fatal (device unplugged or revoked —
        // hotplug re-adds the node if it comes back).
        loop {
            match stream.next_event().await {
                Ok(ev) => {
                    if software
                        && matches!(ev.event_type(), EventType::KEY | EventType::RELATIVE)
                        && shared.since_injection().is_some_and(|since| since < ECHO_WINDOW)
                    {
                        continue;
                    }
                    match ev.event_type() {
                        EventType::KEY => {
                            // value 2 is autorepeat: activity, but not a state transition.
                            let event = match ev.value() {
                                0 => Some(PhysicalEvent::Key {
                                    device: path.clone(),
                                    code: ev.code() as u32,
                                    pressed: false,
                                }),
                                1 => Some(PhysicalEvent::Key {
                                    device: path.clone(),
                                    code: ev.code() as u32,
                                    pressed: true,
                                }),
                                _ => Some(PhysicalEvent::Activity(path.clone())),
                            };
                            if let Some(event) = event {
                                if activity.send(event).await.is_err() {
                                    return;
                                }
                            }
                        }
                        EventType::RELATIVE => {
                            match activity.try_send(PhysicalEvent::Activity(path.clone())) {
                                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                                Err(mpsc::error::TrySendError::Closed(_)) => return,
                            }
                        }
                        _ => {}
                    }
                }
                Err(_) => {
                    let _ = activity.send(PhysicalEvent::DeviceGone(path.clone())).await;
                    return;
                }
            }
        }
    });
}
