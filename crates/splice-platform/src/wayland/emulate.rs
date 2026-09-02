//! Emulation side (this machine as target): RemoteDesktop portal session + reis sender,
//! held-input ledger, ScreenSaver inhibit while entered, screen-lock drop.
//!
//! Load-bearing rules (docs/research/wayland-input.md, DESIGN 13/15/16):
//! - Keys are raw evdev codes; the compositor applies its own layout. Repeat is
//!   generated client-side by apps on the target — inject press/release edges only.
//! - Every batch of events needs a `frame()` or nothing happens; one frame per event.
//! - Scroll120 must accumulate to full ±120 detents (GNOME drops sub-120; trunc, not
//!   floor). ScrollPixels → scroll(), ScrollStop → scroll_stop(cancel).
//! - RequestClipboard must happen BEFORE Start; a restored session without the
//!   clipboard grant discards its token and is recreated exactly once.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use reis::ei;
use reis::ei::button::ButtonState;
use reis::ei::keyboard::KeyState;
use reis::event::{DeviceCapability, EiEvent};
use splice_proto::{InputEvent, PointerButton, Vec2};
use tokio::sync::{mpsc, watch, Notify};
use zbus::zvariant::{OwnedFd, Value};

use super::clipboard::ClipSession;
use super::portal::{self, Options};
use super::tokens::{TokenKind, TokenStore};
use super::WaylandShared;
use crate::{Emulate, PlatformError, Result};

const RD_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const CLIPBOARD_IFACE: &str = "org.freedesktop.portal.Clipboard";
const DEV_KEYBOARD: u32 = 1;
const DEV_POINTER: u32 = 2;
/// persist_mode: persist until explicitly revoked.
const PERSIST: u32 = 2;
const RECREATE_MIN_BACKOFF: Duration = Duration::from_secs(1);
const RECREATE_MIN_INTERVAL: Duration = Duration::from_secs(5);
const EIS_FLUSH_RETRY: Duration = Duration::from_millis(1);
const MAX_MOTION_BATCH_EVENTS: usize = 64;
const COMMAND_SEND_TIMEOUT: Duration = Duration::from_millis(100);

const BTN_LEFT: u32 = 0x110;

enum Command {
    Enter(Vec2),
    Inject(InputEvent),
    Leave,
    ReleaseAll,
}

pub struct WaylandEmulate {
    cmd: mpsc::Sender<Command>,
    abort: Arc<Notify>,
    screensaver: Arc<ScreenSaver>,
    live: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Emulate for WaylandEmulate {
    async fn enter(&self, pos: Vec2) -> Result<()> {
        self.ready()?;
        self.send(Command::Enter(pos)).await
    }

    async fn inject(&self, ev: InputEvent) -> Result<()> {
        self.ready()?;
        self.send(Command::Inject(ev)).await
    }

    async fn leave(&self) -> Result<()> {
        self.send(Command::Leave).await
    }

    async fn release_all(&self) -> Result<()> {
        self.send(Command::ReleaseAll).await
    }
}

impl WaylandEmulate {
    fn ready(&self) -> Result<()> {
        if !self.live.load(Ordering::Acquire) {
            return Err(PlatformError::Unavailable("no input emulation session".into()));
        }
        if self.screensaver.is_locked() {
            return Err(PlatformError::Unavailable("screen locked: remote input paused".into()));
        }
        Ok(())
    }

    async fn send(&self, command: Command) -> Result<()> {
        match tokio::time::timeout(COMMAND_SEND_TIMEOUT, self.cmd.send(command)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(crate::PlatformError::Unavailable(
                "input emulation stopped".into(),
            )),
            Err(_) => {
                self.abort.notify_one();
                Err(crate::PlatformError::Unavailable(
                    "input emulation command queue stalled".into(),
                ))
            }
        }
    }
}

pub fn create(
    shared: Arc<WaylandShared>,
    tokens: Arc<TokenStore>,
    conn: zbus::Connection,
) -> (Arc<WaylandEmulate>, watch::Receiver<Option<ClipSession>>) {
    let (cmd, cmd_rx) = mpsc::channel(64);
    let abort = Arc::new(Notify::new());
    let live = Arc::new(AtomicBool::new(false));
    let (clip_tx, clip_rx) = watch::channel(None);
    let screensaver = Arc::new(ScreenSaver::new(conn.clone()));

    // reis's EiConvertEventStream is not Send (its converter holds boxed FnOnce
    // callbacks), so the portal/reis pump cannot be tokio::spawn'd. It runs on a
    // dedicated current-thread runtime where block_on needs no Send bound.
    let _ = std::thread::Builder::new()
        .name("splice-emulate".into())
        .spawn({
            let abort = abort.clone();
            let live = live.clone();
            let screensaver = screensaver.clone();
            move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        tracing::error!(error = %err, "cannot start emulate runtime");
                        return;
                    }
                };
                rt.block_on(run(shared, tokens, conn, cmd_rx, clip_tx, screensaver, abort, live));
            }
        });

    (Arc::new(WaylandEmulate { cmd, abort, screensaver, live }), clip_rx)
}

/// org.freedesktop.ScreenSaver keep-awake + lock detection. GNOME exposes the interface
/// at /org/freedesktop/ScreenSaver, KDE at /ScreenSaver; the first working path wins.
struct ScreenSaver {
    conn: zbus::Connection,
    locked: AtomicBool,
    cookie: AtomicU32,
}

impl ScreenSaver {
    fn new(conn: zbus::Connection) -> Self {
        Self { conn, locked: AtomicBool::new(false), cookie: AtomicU32::new(0) }
    }

    async fn proxy(&self) -> Option<zbus::Proxy<'static>> {
        for path in ["/org/freedesktop/ScreenSaver", "/ScreenSaver"] {
            let Ok(proxy) = zbus::Proxy::new(
                &self.conn,
                "org.freedesktop.ScreenSaver",
                path,
                "org.freedesktop.ScreenSaver",
            )
            .await
            else {
                continue;
            };
            if proxy.call::<_, _, bool>("GetActive", &()).await.is_ok() {
                return Some(proxy);
            }
        }
        None
    }

    fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    async fn inhibit(&self) {
        let Some(proxy) = self.proxy().await else { return };
        match proxy
            .call::<_, _, u32>("Inhibit", &("splice", "Remote input active"))
            .await
        {
            Ok(cookie) => self.cookie.store(cookie, Ordering::Relaxed),
            Err(err) => tracing::warn!(error = %err, "ScreenSaver.Inhibit failed"),
        }
    }

    async fn uninhibit(&self) {
        let cookie = self.cookie.swap(0, Ordering::Relaxed);
        if cookie == 0 {
            return;
        }
        let Some(proxy) = self.proxy().await else { return };
        if let Err(err) = proxy.call::<_, _, ()>("UnInhibit", &(cookie,)).await {
            tracing::warn!(error = %err, "ScreenSaver.UnInhibit failed");
        }
    }

    /// Tracks GetActive + ActiveChanged and drives the "screen locked" health note.
    /// While locked, RemoteDesktop injection errors out, so injections are dropped.
    async fn monitor(self: Arc<Self>, shared: Arc<WaylandShared>) {
        let Some(proxy) = self.proxy().await else { return };
        let active = proxy.call::<_, _, bool>("GetActive", &()).await.unwrap_or(false);
        self.locked.store(active, Ordering::Relaxed);
        shared.set_health(|h| {
            h.emulate = active.then(|| "screen locked: remote input paused".to_string())
        });
        let mut changes = match proxy.receive_signal("ActiveChanged").await {
            Ok(s) => s,
            Err(_) => return,
        };
        while let Some(msg) = changes.next().await {
            let Ok((active,)) = msg.body().deserialize::<(bool,)>() else { continue };
            self.locked.store(active, Ordering::Relaxed);
            shared.set_health(|h| {
                h.emulate = active.then(|| "screen locked: remote input paused".to_string())
            });
        }
    }
}

/// Absolute-pointer region (x, y, w, h) in logical pixels; reis Regions are not Clone.
type Region = (u32, u32, u32, u32);

#[derive(Default)]
struct Devices {
    /// Every event device and its EIS lifecycle state. A newly advertised device is
    /// paused; sending start_emulating, input, or frame before DeviceResumed is a
    /// protocol violation and makes GNOME disconnect the whole EIS connection.
    all: Vec<TrackedDevice>,
    keyboard: Option<(ei::Device, ei::Keyboard)>,
    pointer: Option<(ei::Device, ei::Pointer)>,
    pointer_abs: Option<(ei::Device, ei::PointerAbsolute, Vec<Region>)>,
    scroll: Option<(ei::Device, ei::Scroll)>,
    button: Option<(ei::Device, ei::Button)>,
}

struct TrackedDevice {
    device: reis::event::Device,
    resumed: bool,
    emulating: bool,
    resume_serial: u32,
}

impl Devices {
    fn add(&mut self, device: reis::event::Device) {
        if device.has_capability(DeviceCapability::Keyboard) {
            if let Some(iface) = device.interface::<ei::Keyboard>() {
                self.keyboard = Some((device.device().clone(), iface));
            }
        }
        if device.has_capability(DeviceCapability::Pointer) {
            if let Some(iface) = device.interface::<ei::Pointer>() {
                self.pointer = Some((device.device().clone(), iface));
            }
        }
        if device.has_capability(DeviceCapability::PointerAbsolute) {
            if let Some(iface) = device.interface::<ei::PointerAbsolute>() {
                let regions = device
                    .regions()
                    .iter()
                    .map(|r| (r.x, r.y, r.width, r.height))
                    .collect();
                self.pointer_abs = Some((device.device().clone(), iface, regions));
            }
        }
        if device.has_capability(DeviceCapability::Scroll) {
            if let Some(iface) = device.interface::<ei::Scroll>() {
                self.scroll = Some((device.device().clone(), iface));
            }
        }
        if device.has_capability(DeviceCapability::Button) {
            if let Some(iface) = device.interface::<ei::Button>() {
                self.button = Some((device.device().clone(), iface));
            }
        }
        self.all.push(TrackedDevice {
            device,
            resumed: false,
            emulating: false,
            resume_serial: 0,
        });
    }

    fn remove(&mut self, device: &reis::event::Device) {
        fn drop_if<T>(entry: &mut Option<(ei::Device, T)>, gone: &ei::Device) {
            if entry.as_ref().is_some_and(|(d, _)| d == gone) {
                *entry = None;
            }
        }
        let gone = device.device();
        drop_if(&mut self.keyboard, gone);
        drop_if(&mut self.pointer, gone);
        drop_if(&mut self.scroll, gone);
        drop_if(&mut self.button, gone);
        if let Some(entry) = &mut self.pointer_abs {
            if &entry.0 == gone {
                self.pointer_abs = None;
            }
        }
        self.all.retain(|d| d.device != *device);
    }

    fn resume(&mut self, device: &reis::event::Device, serial: u32, sequence: Option<u32>) {
        let Some(tracked) = self.all.iter_mut().find(|d| d.device == *device) else {
            return;
        };
        tracked.resumed = true;
        tracked.resume_serial = serial;
        if let Some(sequence) = sequence {
            tracked.device.device().start_emulating(serial, sequence);
            tracked.emulating = true;
        }
    }

    fn pause(&mut self, device: &reis::event::Device) {
        if let Some(tracked) = self.all.iter_mut().find(|d| d.device == *device) {
            tracked.resumed = false;
            tracked.emulating = false;
        }
    }

    fn start_resumed(&mut self, sequence: u32) {
        for tracked in &mut self.all {
            if tracked.resumed && !tracked.emulating {
                // The most recently received EIS serial is the serial from the
                // corresponding DeviceResumed event.
                tracked
                    .device
                    .device()
                    .start_emulating(tracked.resume_serial, sequence);
                tracked.emulating = true;
            }
        }
    }

    fn stop_emulating(&mut self, serial: u32) {
        for tracked in &mut self.all {
            if tracked.emulating {
                tracked.device.device().stop_emulating(serial);
                tracked.emulating = false;
            }
        }
    }

    fn is_emulating(&self, device: &ei::Device) -> bool {
        self.all.iter().any(|tracked| tracked.device.device() == device && tracked.emulating)
    }

    fn abs_region_containing(&self, x: f64, y: f64) -> bool {
        let Some((_, _, regions)) = &self.pointer_abs else { return false };
        regions.iter().any(|&(rx, ry, rw, rh)| {
            x >= rx as f64 && x < (rx + rw) as f64 && y >= ry as f64 && y < (ry + rh) as f64
        })
    }
}

fn now_micros() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

struct Session {
    session_path: String,
    connection: reis::event::Connection,
    ei_stream: reis::tokio::EiConvertEventStream,
    clipboard_enabled: bool,
}

/// Creates and starts a RemoteDesktop session. The bool reports whether a restore token
/// was used, which gates the discard-token-and-retry-once clipboard recovery.
async fn establish(conn: &zbus::Connection, tokens: &TokenStore) -> Result<(Session, bool)> {
    let rd = portal::proxy(conn, RD_IFACE).await?;

    let created = portal::request(conn, &rd, "CreateSession", |token| {
        let mut opts = Options::new();
        opts.insert("handle_token", Value::new(token.to_owned()));
        opts.insert("session_handle_token", Value::new(token.to_owned()));
        (opts,)
    })
    .await?;
    let session_path = portal::session_handle(&created)
        .ok_or_else(|| PlatformError::Unavailable("CreateSession returned no valid session_handle".into()))?;
    let session_opath = portal::object_path(&session_path)?;

    // RequestClipboard must be called before Start or the grant can never happen.
    let clipboard = portal::proxy(conn, CLIPBOARD_IFACE).await?;
    if let Err(err) = clipboard
        .call::<_, _, ()>("RequestClipboard", &(session_opath.clone(), Options::new()))
        .await
    {
        tracing::warn!(error = %err, "RequestClipboard failed; clipboard sync disabled");
    }

    let restore_token = tokens.get(TokenKind::RemoteDesktop);
    let used_token = restore_token.is_some();
    portal::request(conn, &rd, "SelectDevices", |token| {
        let mut opts = Options::new();
        opts.insert("handle_token", Value::new(token.to_owned()));
        opts.insert("types", Value::new(DEV_KEYBOARD | DEV_POINTER));
        opts.insert("persist_mode", Value::new(PERSIST));
        if let Some(restore) = restore_token {
            opts.insert("restore_token", Value::new(restore));
        }
        (session_opath.clone(), opts)
    })
    .await?;

    let started = portal::request(conn, &rd, "Start", |token| {
        let mut opts = Options::new();
        opts.insert("handle_token", Value::new(token.to_owned()));
        (session_opath.clone(), "", opts)
    })
    .await?;
    if let Some(token) = portal::get::<String>(&started, "restore_token") {
        tokens.set(TokenKind::RemoteDesktop, token);
    }
    let clipboard_enabled =
        portal::get::<bool>(&started, "clipboard_enabled").unwrap_or(false);

    let eis_fd: OwnedFd = rd
        .call("ConnectToEIS", &(session_opath.clone(), Options::new()))
        .await
        .map_err(portal::err_ctx("ConnectToEIS"))?;
    let stream = std::os::unix::net::UnixStream::from(std::os::fd::OwnedFd::from(eis_fd));
    let context = ei::Context::new(stream)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("ei context: {e}")))?;
    let (connection, ei_stream) = context
        .handshake_tokio("splice", ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("ei handshake: {e}")))?;

    Ok((
        Session {
            session_path,
            connection,
            ei_stream,
            clipboard_enabled,
        },
        used_token,
    ))
}

#[derive(Default)]
struct Active {
    emulating: bool,
    sequence: u32,
    pending_position: Option<Vec2>,
    held_keys: HashSet<u32>,
    held_buttons: HashSet<u32>,
    scroll_rem_x: i32,
    scroll_rem_y: i32,
}

static EMULATE_SEQ: AtomicU32 = AtomicU32::new(1);

#[allow(clippy::too_many_arguments)]
async fn run(
    shared: Arc<WaylandShared>,
    tokens: Arc<TokenStore>,
    conn: zbus::Connection,
    mut cmd_rx: mpsc::Receiver<Command>,
    clip_tx: watch::Sender<Option<ClipSession>>,
    screensaver: Arc<ScreenSaver>,
    abort: Arc<Notify>,
    live: Arc<AtomicBool>,
) {
    {
        let shared = shared.clone();
        let screensaver = screensaver.clone();
        let _ = tokio::spawn(async move { screensaver.monitor(shared).await });
    }

    // Survives session death: a session that was entered is re-entered after recreation.
    let mut entered: Option<Vec2> = None;
    let mut clip_retried = false;
    let mut last_recreate = Instant::now() - RECREATE_MIN_INTERVAL;
    loop {
        let since = last_recreate.elapsed();
        if since < RECREATE_MIN_INTERVAL {
            tokio::time::sleep((RECREATE_MIN_INTERVAL - since).max(RECREATE_MIN_BACKOFF)).await;
        }
        last_recreate = Instant::now();

        match establish(&conn, &tokens).await {
            Ok((session, used_token)) => {
                // A restored session that lost the clipboard grant is recreated once
                // with a fresh prompt (Deskflow's trick).
                if !session.clipboard_enabled && used_token && !clip_retried {
                    clip_retried = true;
                    tokens.clear(TokenKind::RemoteDesktop);
                    continue;
                }
                if !session.clipboard_enabled {
                    shared.set_health(|h| {
                        h.clipboard = Some("clipboard not granted by portal".into());
                    });
                }
                if !screensaver.is_locked() {
                    shared.set_health(|h| h.emulate = None);
                }
                let _ = clip_tx.send(Some(ClipSession {
                    path: session.session_path.clone(),
                    enabled: session.clipboard_enabled,
                }));
                let resume = entered.take();
                live.store(true, Ordering::Release);
                run_session(
                    &shared,
                    &conn,
                    session,
                    &mut cmd_rx,
                    resume,
                    RunState {
                        screensaver: &screensaver,
                        entered: &mut entered,
                        abort: &abort,
                    },
                )
                .await;
                live.store(false, Ordering::Release);
                let _ = clip_tx.send(None);
                screensaver.uninhibit().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "remote desktop session setup failed");
                shared.set_health(|h| h.emulate = Some(format!("{err}")));
            }
        }
    }
}

struct RunState<'a> {
    screensaver: &'a Arc<ScreenSaver>,
    entered: &'a mut Option<Vec2>,
    abort: &'a Notify,
}

/// Runs one live session. `resume` re-enters a session that was active when the previous
/// portal session died.
async fn run_session(
    shared: &Arc<WaylandShared>,
    conn: &zbus::Connection,
    mut session: Session,
    cmd_rx: &mut mpsc::Receiver<Command>,
    resume: Option<Vec2>,
    state: RunState<'_>,
) {
    let session_proxy = match portal::session_proxy(conn, &session.session_path).await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "cannot watch remote desktop session");
            shared.set_health(|h| {
                h.emulate = Some(format!("cannot watch remote desktop session: {err}"))
            });
            return;
        }
    };
    let mut closed = match session_proxy.receive_signal("Closed").await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "cannot subscribe to Session.Closed");
            shared.set_health(|h| {
                h.emulate = Some(format!("cannot watch remote desktop closure: {err}"))
            });
            return;
        }
    };

    let mut devices = Devices::default();
    let mut active = Active::default();
    let mut flush_pending = false;

    if let Some(pos) = resume {
        do_enter(&session, &mut devices, &mut active, state.screensaver, pos);
        flush_pending = true;
    }

    loop {
        tokio::select! {
            _ = state.abort.notified() => {
                *state.entered = None;
                let _ = session_proxy.call::<_, _, ()>("Close", &()).await;
                return;
            }
            _ = closed.next() => {
                tracing::warn!("remote desktop session closed by portal");
                shared.set_health(|h| {
                    h.emulate = Some("remote desktop session closed by portal".into())
                });
                return;
            }
            cmd = async {
                if flush_pending {
                    std::future::pending().await
                } else {
                    cmd_rx.recv().await
                }
            } => {
                let Some(mut command) = cmd else { return };
                loop {
                    let following = match command {
                        Command::Enter(pos) => {
                            do_enter(&session, &mut devices, &mut active, state.screensaver, pos);
                            *state.entered = Some(pos);
                            None
                        }
                        Command::Inject(InputEvent::Motion { mut dx, mut dy }) => {
                            let mut following = None;
                            for _ in 1..MAX_MOTION_BATCH_EVENTS {
                                let Ok(next) = cmd_rx.try_recv() else {
                                    break;
                                };
                                match next {
                                    Command::Inject(InputEvent::Motion {
                                        dx: next_dx,
                                        dy: next_dy,
                                    }) => {
                                        dx += next_dx;
                                        dy += next_dy;
                                    }
                                    next => {
                                        following = Some(next);
                                        break;
                                    }
                                }
                            }
                            if active.emulating && !state.screensaver.is_locked() {
                                inject(
                                    &session,
                                    &devices,
                                    &mut active,
                                    InputEvent::Motion { dx, dy },
                                );
                            }
                            following
                        }
                        Command::Inject(ev) => {
                            if active.emulating && !state.screensaver.is_locked() {
                                inject(&session, &devices, &mut active, ev);
                            }
                            None
                        }
                        Command::Leave => {
                            do_leave(&session, &mut devices, &mut active, state.screensaver);
                            *state.entered = None;
                            None
                        }
                        Command::ReleaseAll => {
                            release_held(&session, &devices, &mut active);
                            None
                        }
                    };
                    let Some(next) = following else {
                        break;
                    };
                    command = next;
                }
                match flush_eis(&session) {
                    Ok(pending) => flush_pending = pending,
                    Err(err) => {
                        tracing::warn!(error = %err, "input emulation transport write failed");
                        shared.set_health(|h| {
                            h.emulate = Some(format!("input emulation transport write failed: {err}"))
                        });
                        return;
                    }
                }
            }
            _ = async {
                if flush_pending {
                    tokio::time::sleep(EIS_FLUSH_RETRY).await;
                } else {
                    std::future::pending().await
                }
            } => {
                match flush_eis(&session) {
                    Ok(pending) => flush_pending = pending,
                    Err(err) => {
                        tracing::warn!(error = %err, "input emulation transport write failed");
                        shared.set_health(|h| {
                            h.emulate = Some(format!("input emulation transport write failed: {err}"))
                        });
                        return;
                    }
                }
            }
            event = session.ei_stream.next() => {
                match event {
                    None => {
                        tracing::warn!("ei stream ended");
                        shared.set_health(|h| {
                            h.emulate = Some("input emulation transport ended".into())
                        });
                        return;
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "ei stream error");
                        shared.set_health(|h| {
                            h.emulate = Some(format!("input emulation transport error: {err}"))
                        });
                        return;
                    }
                    Some(Ok(EiEvent::SeatAdded(added))) => {
                        added.seat.bind_capabilities(
                            DeviceCapability::Pointer
                                | DeviceCapability::PointerAbsolute
                                | DeviceCapability::Keyboard
                                | DeviceCapability::Button
                                | DeviceCapability::Scroll,
                        );
                        if session.connection.flush().is_err() {
                            shared.set_health(|h| {
                                h.emulate = Some("input emulation transport write failed".into())
                            });
                            return;
                        }
                    }
                    Some(Ok(EiEvent::DeviceAdded(added))) => {
                        devices.add(added.device);
                    }
                    Some(Ok(EiEvent::DeviceRemoved(removed))) => {
                        devices.remove(&removed.device);
                    }
                    Some(Ok(EiEvent::DeviceResumed(resumed))) => {
                        tracing::debug!(device = ?resumed.device, "input emulation device resumed");
                        devices.resume(
                            &resumed.device,
                            resumed.serial,
                            active.emulating.then_some(active.sequence),
                        );
                        if active.emulating {
                            send_pending_position(&session, &devices, &mut active);
                        }
                        match flush_eis(&session) {
                            Ok(pending) => flush_pending = pending,
                            Err(err) => {
                                tracing::warn!(error = %err, "input emulation transport write failed");
                                shared.set_health(|h| {
                                    h.emulate = Some(format!("input emulation transport write failed: {err}"))
                                });
                                return;
                            }
                        }
                    }
                    Some(Ok(EiEvent::DevicePaused(paused))) => {
                        tracing::debug!(device = ?paused.device, "input emulation device paused");
                        devices.pause(&paused.device);
                        // EIS resets a paused device's input state. Drop our ledger
                        // too so a later resume cannot emit stale release events.
                        active.held_keys.clear();
                        active.held_buttons.clear();
                        active.scroll_rem_x = 0;
                        active.scroll_rem_y = 0;
                    }
                    Some(Ok(EiEvent::Disconnected(disconnected))) => {
                        tracing::warn!(
                            reason = ?disconnected.reason,
                            explanation = ?disconnected.explanation,
                            "ei disconnected"
                        );
                        let explanation = disconnected
                            .explanation
                            .as_deref()
                            .map(|text| format!(": {text}"))
                            .unwrap_or_default();
                        shared.set_health(|h| {
                            h.emulate = Some(format!(
                                "input emulation disconnected ({:?}){explanation}",
                                disconnected.reason
                            ))
                        });
                        return;
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

fn do_enter(
    session: &Session,
    devices: &mut Devices,
    active: &mut Active,
    screensaver: &Arc<ScreenSaver>,
    pos: Vec2,
) {
    if active.emulating {
        do_leave_inner(session, devices, active);
    }
    active.emulating = true;
    active.held_keys.clear();
    active.held_buttons.clear();
    active.scroll_rem_x = 0;
    active.scroll_rem_y = 0;

    // One sequence per enter, shared by all devices; monotonic across sessions.
    let seq = EMULATE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    active.sequence = seq;
    active.pending_position = Some(pos);
    devices.start_resumed(seq);
    send_pending_position(session, devices, active);

    let screensaver = screensaver.clone();
    let _ = tokio::spawn(async move { screensaver.inhibit().await });
}

fn do_leave(
    session: &Session,
    devices: &mut Devices,
    active: &mut Active,
    screensaver: &Arc<ScreenSaver>,
) {
    do_leave_inner(session, devices, active);
    let screensaver = screensaver.clone();
    let _ = tokio::spawn(async move { screensaver.uninhibit().await });
}

fn do_leave_inner(session: &Session, devices: &mut Devices, active: &mut Active) {
    release_held(session, devices, active);
    if active.emulating {
        devices.stop_emulating(session.connection.serial());
    }
    active.emulating = false;
    active.pending_position = None;
}

fn flush_eis(session: &Session) -> std::result::Result<bool, String> {
    match session.connection.flush() {
        Ok(()) => Ok(false),
        Err(err) if err.raw_os_error() == libc::EAGAIN => Ok(true),
        Err(err) => Err(err.to_string()),
    }
}

fn release_held(session: &Session, devices: &Devices, active: &mut Active) {
    let keys: Vec<_> = active.held_keys.drain().collect();
    if let Some((device, keyboard)) = &devices.keyboard {
        if devices.is_emulating(device) && !keys.is_empty() {
            for key in keys {
                keyboard.key(key, KeyState::Released);
            }
            device.frame(session.connection.serial(), now_micros());
        }
    }
    let buttons: Vec<_> = active.held_buttons.drain().collect();
    if let Some((device, button)) = &devices.button {
        if devices.is_emulating(device) && !buttons.is_empty() {
            for code in buttons {
                button.button(code, ButtonState::Released);
            }
            device.frame(session.connection.serial(), now_micros());
        }
    }
}

fn send_pending_position(session: &Session, devices: &Devices, active: &mut Active) {
    let Some(pos) = active.pending_position else { return };
    let Some((device, iface, _)) = &devices.pointer_abs else { return };
    if !devices.is_emulating(device) {
        return;
    }
    // Absolute positioning requires the point inside a device region;
    // out-of-region coordinates are silently discarded.
    if devices.abs_region_containing(pos.x, pos.y) {
        iface.motion_absolute(pos.x as f32, pos.y as f32);
        device.frame(session.connection.serial(), now_micros());
    }
    active.pending_position = None;
}

fn inject(session: &Session, devices: &Devices, active: &mut Active, ev: InputEvent) {
    let serial = session.connection.serial();
    match ev {
        InputEvent::Motion { dx, dy } => {
            if let Some((device, pointer)) = &devices.pointer {
                if devices.is_emulating(device) {
                    pointer.motion_relative(dx as f32, dy as f32);
                    device.frame(serial, now_micros());
                }
            }
        }
        InputEvent::Button { button, pressed } => {
            if let Some((device, iface)) = &devices.button {
                if !devices.is_emulating(device) {
                    return;
                }
                let code = match button {
                    PointerButton::Left => BTN_LEFT,
                    PointerButton::Right => BTN_LEFT + 1,
                    PointerButton::Middle => BTN_LEFT + 2,
                    PointerButton::Back => BTN_LEFT + 3,
                    PointerButton::Forward => BTN_LEFT + 4,
                    // Other(n) carries the evdev offset from BTN_LEFT (capture side).
                    PointerButton::Other(n) => BTN_LEFT + n as u32,
                };
                let state = if pressed { ButtonState::Press } else { ButtonState::Released };
                iface.button(code, state);
                if pressed {
                    active.held_buttons.insert(code);
                } else {
                    active.held_buttons.remove(&code);
                }
                device.frame(serial, now_micros());
            }
        }
        InputEvent::Key { code, pressed } => {
            if let Some((device, keyboard)) = &devices.keyboard {
                if !devices.is_emulating(device) {
                    return;
                }
                let state = if pressed { KeyState::Press } else { KeyState::Released };
                keyboard.key(code, state);
                if pressed {
                    active.held_keys.insert(code);
                } else {
                    active.held_keys.remove(&code);
                }
                device.frame(serial, now_micros());
            }
        }
        InputEvent::ScrollPixels { dx, dy } => {
            if let Some((device, scroll)) = &devices.scroll {
                if devices.is_emulating(device) {
                    scroll.scroll(dx as f32, dy as f32);
                    device.frame(serial, now_micros());
                }
            }
        }
        InputEvent::Scroll120 { dx, dy } => {
            let Some((device, scroll)) = &devices.scroll else { return };
            if !devices.is_emulating(device) {
                return;
            }
            // GNOME silently drops sub-120 remainders; accumulate and emit whole
            // detents only (i32 division is trunc-toward-zero, as required).
            active.scroll_rem_x += dx;
            active.scroll_rem_y += dy;
            let steps_x = active.scroll_rem_x / 120;
            let steps_y = active.scroll_rem_y / 120;
            if steps_x != 0 || steps_y != 0 {
                scroll.scroll_discrete(steps_x * 120, steps_y * 120);
                device.frame(serial, now_micros());
                active.scroll_rem_x -= steps_x * 120;
                active.scroll_rem_y -= steps_y * 120;
            }
        }
        InputEvent::ScrollStop { cancel } => {
            if let Some((device, scroll)) = &devices.scroll {
                if devices.is_emulating(device) {
                    scroll.scroll_stop(1, 1, cancel as u32);
                    device.frame(serial, now_micros());
                }
            }
        }
    }
    let _ = session.connection.flush();
}
