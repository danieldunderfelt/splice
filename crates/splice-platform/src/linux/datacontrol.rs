//! Clipboard through the data-control protocols (ext-data-control-v1, falling back to
//! wlr-data-control-unstable-v1): no portal session, no focused window, no prompt.
//! Available on KDE, the wlroots family, COSMIC and niri; mutter has neither.
//!
//! The two protocols are isomorphic, so one state machine drives both. There is no
//! "session_is_owner" flag here: our own SetSelection echoes back as a selection event,
//! which is recognised by the ownership flag set until the source is cancelled.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::calloop::channel::{self, Channel, Event as ChannelEvent};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use splice_proto::{CLIP_INLINE_TEXT_MAX, CLIP_MAX_TOTAL};
use tokio::sync::oneshot;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use super::{Shared, Stop};
use crate::{ClipFetch, Clipboard, ClipboardOffer, PlatformError, PlatformEvent, Result};

const TEXT_MIME: &str = "text/plain;charset=utf-8";
/// Private mime advertised on every selection we own; an offer carrying it is our own
/// echo (there is no owner flag in the protocol), and it is stripped from public lists.
const OWNER_MARKER_PREFIX: &str = "x-splice/owner-";
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const TEXT_ALIASES: &[&str] = &["text/plain", "UTF8_STRING", "STRING", "TEXT"];
const NOISE_MIMES: &[&str] = &["TIMESTAMP", "TARGETS", "MULTIPLE", "SAVE_TARGETS"];
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

enum Command {
    SetOffer { mimes: Vec<String>, fetch: Arc<dyn ClipFetch> },
    Read { mime: String, reply: oneshot::Sender<io::Result<Vec<u8>>> },
    Shutdown,
}

pub struct DataControlClipboard {
    cmd: channel::Sender<Command>,
}

#[async_trait::async_trait]
impl Clipboard for DataControlClipboard {
    async fn set_remote_offer(&self, offer: ClipboardOffer, fetch: Arc<dyn ClipFetch>) -> Result<()> {
        self.cmd
            .send(Command::SetOffer { mimes: offer.mimes, fetch })
            .map_err(|_| PlatformError::Unavailable("data-control clipboard stopped".into()))
    }

    async fn read_local(&self, mime: &str) -> Result<Vec<u8>> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Command::Read { mime: mime.to_string(), reply })
            .map_err(|_| PlatformError::Unavailable("data-control clipboard stopped".into()))?;
        match rx.await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(err)) => Err(PlatformError::Other(err.into())),
            Err(_) => Err(PlatformError::Unavailable("clipboard read dropped".into())),
        }
    }
}

pub fn create(shared: Arc<Shared>) -> Result<(Arc<dyn Clipboard>, Stop)> {
    let (cmd, cmd_rx) = channel::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let runtime = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("splice-clipboard".into())
        .spawn(move || run(shared, runtime, cmd_rx, ready_tx))
        .map_err(|e| PlatformError::Unavailable(format!("cannot start clipboard thread: {e}")))?;
    ready_rx
        .recv_timeout(SETUP_TIMEOUT)
        .map_err(|_| PlatformError::Unavailable("clipboard thread did not finish setup".into()))??;
    let stop = Stop::new({
        let cmd = cmd.clone();
        move || {
            let _ = cmd.send(Command::Shutdown);
        }
    });
    Ok((Arc::new(DataControlClipboard { cmd }), stop))
}

#[derive(Clone)]
enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

#[derive(Clone)]
enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone, PartialEq)]
enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

#[derive(Clone, PartialEq)]
enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl Device {
    fn destroy(&self) {
        match self {
            Device::Ext(d) => d.destroy(),
            Device::Wlr(d) => d.destroy(),
        }
    }
}

impl Offer {
    fn id(&self) -> ObjectId {
        match self {
            Offer::Ext(o) => o.id(),
            Offer::Wlr(o) => o.id(),
        }
    }

    fn receive(&self, mime: &str, fd: &OwnedFd) {
        match self {
            Offer::Ext(o) => o.receive(mime.to_string(), fd.as_fd()),
            Offer::Wlr(o) => o.receive(mime.to_string(), fd.as_fd()),
        }
    }

    fn destroy(&self) {
        match self {
            Offer::Ext(o) => o.destroy(),
            Offer::Wlr(o) => o.destroy(),
        }
    }
}

impl Source {
    fn id(&self) -> ObjectId {
        match self {
            Source::Ext(s) => s.id(),
            Source::Wlr(s) => s.id(),
        }
    }

    fn offer(&self, mime: &str) {
        match self {
            Source::Ext(s) => s.offer(mime.to_string()),
            Source::Wlr(s) => s.offer(mime.to_string()),
        }
    }

    fn destroy(&self) {
        match self {
            Source::Ext(s) => s.destroy(),
            Source::Wlr(s) => s.destroy(),
        }
    }
}

struct Own {
    source: Source,
    mimes: Vec<String>,
    fetch: Arc<dyn ClipFetch>,
}

struct State {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Handle,
    manager: Manager,
    device: Option<Device>,
    seat: Option<wl_seat::WlSeat>,
    offers: HashMap<ObjectId, (Offer, Vec<String>)>,
    current: Option<(Offer, Vec<String>)>,
    own: Option<Own>,
    /// Superseded sources kept alive until `cancelled`, so a `send` already queued
    /// for them is still served.
    retired: HashMap<ObjectId, (Source, Arc<dyn ClipFetch>)>,
    marker: String,
    /// Bumped per selection; late inline reads for an older selection are dropped.
    generation: Arc<AtomicU64>,
    running: bool,
}

fn normalize_mimes(mimes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for mime in mimes {
        let normalized = if mime == TEXT_MIME || TEXT_ALIASES.contains(&mime.as_str()) {
            TEXT_MIME
        } else if NOISE_MIMES.contains(&mime.as_str()) || mime.starts_with(OWNER_MARKER_PREFIX) {
            continue;
        } else {
            mime.as_str()
        };
        if !out.iter().any(|m| m == normalized) {
            out.push(normalized.to_string());
        }
    }
    out
}

/// Mimes to advertise for a normalized offer: text gets every legacy alias so X11-era
/// and toolkit consumers can paste it.
fn advertised_mimes(mimes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for mime in mimes {
        if !out.contains(mime) {
            out.push(mime.clone());
        }
        if mime == TEXT_MIME {
            for alias in TEXT_ALIASES {
                if !out.iter().any(|m| m == alias) {
                    out.push((*alias).to_string());
                }
            }
        }
    }
    out
}

/// The normalized representation to fetch for a mime a local app asked for.
fn fetch_mime(mime: &str) -> &str {
    if TEXT_ALIASES.contains(&mime) {
        TEXT_MIME
    } else {
        mime
    }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait(fd: &OwnedFd, events: i16, deadline: Instant) -> io::Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "clipboard pipe timed out"));
    }
    let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events, revents: 0 };
    let n = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis().min(i32::MAX as u128) as i32) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "clipboard pipe timed out"));
    }
    Ok(())
}

/// Reads a pipe to EOF (capped at `cap` bytes) with an overall timeout.
fn read_pipe(fd: OwnedFd, cap: usize, timeout: Duration) -> io::Result<Vec<u8>> {
    set_nonblocking(&fd)?;
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n == 0 {
            return Ok(out);
        }
        if n > 0 {
            let n = n as usize;
            let remaining = cap.saturating_sub(out.len());
            out.extend_from_slice(&buf[..n.min(remaining)]);
            if out.len() >= cap {
                return Ok(out);
            }
            continue;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            wait(&fd, libc::POLLIN, deadline)?;
            continue;
        }
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

/// Writes all of `data` to a pipe with a timeout; a consumer closing early (EPIPE) is
/// a normal outcome, not an error.
fn write_pipe(fd: OwnedFd, data: &[u8], timeout: Duration) -> io::Result<()> {
    set_nonblocking(&fd)?;
    let deadline = Instant::now() + timeout;
    let mut written = 0;
    while written < data.len() {
        let n = unsafe {
            libc::write(fd.as_raw_fd(), data[written..].as_ptr().cast(), data.len() - written)
        };
        if n >= 0 {
            written += n as usize;
            continue;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::WouldBlock => wait(&fd, libc::POLLOUT, deadline)?,
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::BrokenPipe => return Ok(()),
            _ => return Err(err),
        }
    }
    Ok(())
}

impl State {
    fn ensure_device(&mut self, qh: &QueueHandle<Self>) {
        if self.device.is_some() {
            return;
        }
        let Some(seat) = &self.seat else { return };
        let device = match &self.manager {
            Manager::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
            Manager::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
        };
        self.device = Some(device);
    }

    fn new_offer(&mut self, offer: Offer) {
        self.offers.insert(offer.id(), (offer, Vec::new()));
    }

    fn offer_mime(&mut self, id: ObjectId, mime: String) {
        if let Some((_, mimes)) = self.offers.get_mut(&id) {
            mimes.push(mime);
        }
    }

    fn selection(&mut self, id: Option<ObjectId>) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some((old, _)) = self.current.take() {
            old.destroy();
        }
        let Some(id) = id else {
            return;
        };
        let Some((offer, mimes)) = self.offers.remove(&id) else { return };
        if mimes.iter().any(|m| m == &self.marker) {
            offer.destroy();
            return;
        }
        let normalized = normalize_mimes(&mimes);
        if normalized.is_empty() {
            self.current = Some((offer, mimes));
            return;
        }
        let has_text = normalized.iter().any(|m| m == TEXT_MIME);
        let inline = if has_text {
            self.receive(&offer, &mimes, TEXT_MIME).ok()
        } else {
            None
        };
        self.current = Some((offer, mimes));
        let shared = self.shared.clone();
        let current = self.generation.clone();
        std::thread::spawn(move || {
            let inline_text = inline.and_then(|fd| {
                read_pipe(fd, CLIP_INLINE_TEXT_MAX + 1, READ_TIMEOUT)
                    .ok()
                    .filter(|bytes| bytes.len() <= CLIP_INLINE_TEXT_MAX)
                    .and_then(|bytes| String::from_utf8(bytes).ok())
            });
            if current.load(Ordering::Acquire) == generation {
                shared.emit(PlatformEvent::ClipboardChanged { mimes: normalized, inline_text });
            }
        });
    }

    /// Starts a receive for `wanted` (or a legacy alias the offer actually has) and
    /// returns the read end of the pipe.
    fn receive(&self, offer: &Offer, offered: &[String], wanted: &str) -> io::Result<OwnedFd> {
        let mut candidates = vec![wanted.to_string()];
        if wanted == TEXT_MIME {
            candidates.extend(TEXT_ALIASES.iter().map(|s| s.to_string()));
        }
        let Some(mime) = candidates.into_iter().find(|m| offered.contains(m)) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "representation not offered"));
        };
        let (read, write) = pipe()?;
        offer.receive(&mime, &write);
        drop(write);
        Ok(read)
    }

    fn set_offer(&mut self, qh: &QueueHandle<Self>, mimes: Vec<String>, fetch: Arc<dyn ClipFetch>) {
        if mimes.is_empty() {
            return;
        }
        let Some(device) = &self.device else { return };
        if let Some(old) = self.own.take() {
            self.retired.insert(old.source.id(), (old.source, old.fetch));
        }
        let source = match &self.manager {
            Manager::Ext(m) => Source::Ext(m.create_data_source(qh, ())),
            Manager::Wlr(m) => Source::Wlr(m.create_data_source(qh, ())),
        };
        for mime in advertised_mimes(&mimes) {
            source.offer(&mime);
        }
        source.offer(&self.marker);
        match (device, &source) {
            (Device::Ext(d), Source::Ext(s)) => d.set_selection(Some(s)),
            (Device::Wlr(d), Source::Wlr(s)) => d.set_selection(Some(s)),
            _ => {}
        }
        self.own = Some(Own { source, mimes, fetch });
    }

    fn republish(&mut self, qh: &QueueHandle<Self>) {
        if let Some(own) = self.own.take() {
            own.source.destroy();
            self.set_offer(qh, own.mimes, own.fetch);
        }
    }

    fn send(&self, source_id: ObjectId, mime: String, fd: OwnedFd) {
        if mime.starts_with(OWNER_MARKER_PREFIX) {
            return;
        }
        let fetch = match &self.own {
            Some(own) if own.source.id() == source_id => own.fetch.clone(),
            _ => match self.retired.get(&source_id) {
                Some((_, fetch)) => fetch.clone(),
                None => return,
            },
        };
        let runtime = self.runtime.clone();
        std::thread::spawn(move || {
            let wanted = fetch_mime(&mime).to_string();
            let data = runtime.block_on(async {
                tokio::time::timeout(FETCH_TIMEOUT, fetch.fetch(&wanted)).await.ok().flatten()
            });
            if let Some(bytes) = data {
                if let Err(err) = write_pipe(fd, &bytes, READ_TIMEOUT) {
                    tracing::debug!(error = %err, "clipboard write to local app failed");
                }
            }
        });
    }

    fn cancelled(&mut self, source_id: ObjectId) {
        if self.own.as_ref().is_some_and(|own| own.source.id() == source_id) {
            if let Some(own) = self.own.take() {
                own.source.destroy();
            }
        } else if let Some((source, _)) = self.retired.remove(&source_id) {
            source.destroy();
        }
    }

    fn device_finished(&mut self, qh: &QueueHandle<Self>) {
        if let Some(device) = self.device.take() {
            device.destroy();
        }
        if let Some((offer, _)) = self.current.take() {
            offer.destroy();
        }
        for (_, (offer, _)) in self.offers.drain() {
            offer.destroy();
        }
        self.ensure_device(qh);
        self.republish(qh);
    }

    fn read(&self, mime: String, reply: oneshot::Sender<io::Result<Vec<u8>>>) {
        let Some((offer, offered)) = &self.current else {
            let _ = reply.send(Err(io::Error::new(io::ErrorKind::NotFound, "clipboard is empty")));
            return;
        };
        match self.receive(offer, offered, &mime) {
            Ok(fd) => {
                std::thread::spawn(move || {
                    let _ = reply.send(read_pipe(fd, CLIP_MAX_TOTAL, READ_TIMEOUT));
                });
            }
            Err(err) => {
                let _ = reply.send(Err(err));
            }
        }
    }

    fn handle_command(&mut self, qh: &QueueHandle<Self>, cmd: Command) {
        match cmd {
            Command::SetOffer { mimes, fetch } => self.set_offer(qh, mimes, fetch),
            Command::Read { mime, reply } => self.read(mime, reply),
            Command::Shutdown => {
                self.generation.fetch_add(1, Ordering::AcqRel);
                if let Some(own) = self.own.take() {
                    own.source.destroy();
                }
                for (_, (source, _)) in self.retired.drain() {
                    source.destroy();
                }
                if let Some(device) = self.device.take() {
                    device.destroy();
                }
                self.running = false;
            }
        }
    }
}

fn run(
    shared: Arc<Shared>,
    runtime: tokio::runtime::Handle,
    cmd_rx: Channel<Command>,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
) {
    let unavailable = |what: &str, e: &dyn std::fmt::Display| {
        PlatformError::Unavailable(format!("data-control clipboard: {what}: {e}"))
    };
    let setup = (|| -> Result<(EventLoop<'static, State>, State, Connection, QueueHandle<State>)> {
        let conn = Connection::connect_to_env().map_err(|e| unavailable("connect", &e))?;
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(|e| unavailable("registry", &e))?;
        let qh = queue.handle();
        let manager = if let Ok(m) = globals.bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
            Manager::Ext(m)
        } else if let Ok(m) = globals.bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
            Manager::Wlr(m)
        } else {
            return Err(PlatformError::Unavailable(
                "data-control clipboard: no ext/wlr data-control global".into(),
            ));
        };
        let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 2..=7, ()).ok();
        if seat.is_none() {
            return Err(PlatformError::Unavailable("data-control clipboard: no wl_seat".into()));
        }
        let event_loop: EventLoop<State> = EventLoop::try_new().map_err(|e| unavailable("event loop", &e))?;
        WaylandSource::new(conn.clone(), queue)
            .insert(event_loop.handle())
            .map_err(|e| unavailable("event source", &e))?;
        let mut state = State {
            shared,
            runtime,
            manager,
            device: None,
            seat,
            offers: HashMap::new(),
            current: None,
            own: None,
            retired: HashMap::new(),
            marker: format!("{OWNER_MARKER_PREFIX}{}", std::process::id()),
            generation: Arc::new(AtomicU64::new(0)),
            running: true,
        };
        state.ensure_device(&qh);
        Ok((event_loop, state, conn, qh))
    })();
    let (mut event_loop, mut state, conn, qh) = match setup {
        Ok(parts) => parts,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let qh_cmd = qh.clone();
    if let Err(err) = event_loop.handle().insert_source(cmd_rx, move |event, _, state| {
        if let ChannelEvent::Msg(cmd) = event {
            state.handle_command(&qh_cmd, cmd);
        } else {
            state.running = false;
        }
    }) {
        let _ = ready.send(Err(unavailable("command source", &err)));
        return;
    }
    let _ = ready.send(Ok(()));
    state.shared.set_health(|h| h.clipboard = None);
    while state.running {
        if let Err(err) = event_loop.dispatch(None, &mut state) {
            tracing::warn!(error = %err, "clipboard event loop failed");
            state.shared.set_health(|h| h.clipboard = Some(format!("data-control clipboard stopped: {err}")));
            return;
        }
    }
    let _ = conn.flush();
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ExtDataControlManagerV1, _: <ExtDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrDataControlManagerV1, _: <ZwlrDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(state: &mut Self, _: &ExtDataControlDeviceV1, event: ext_data_control_device_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => state.new_offer(Offer::Ext(id)),
            ext_data_control_device_v1::Event::Selection { id } => state.selection(id.map(|o| o.id())),
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                state.offers.remove(&offer.id());
                offer.destroy();
            }
            ext_data_control_device_v1::Event::Finished => state.device_finished(qh),
            _ => {}
        }
    }
    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(state: &mut Self, _: &ZwlrDataControlDeviceV1, event: zwlr_data_control_device_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => state.new_offer(Offer::Wlr(id)),
            zwlr_data_control_device_v1::Event::Selection { id } => state.selection(id.map(|o| o.id())),
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                state.offers.remove(&offer.id());
                offer.destroy();
            }
            zwlr_data_control_device_v1::Event::Finished => state.device_finished(qh),
            _ => {}
        }
    }
    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(state: &mut Self, offer: &ExtDataControlOfferV1, event: ext_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(state: &mut Self, offer: &ZwlrDataControlOfferV1, event: zwlr_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(state: &mut Self, source: &ExtDataControlSourceV1, event: ext_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => state.send(source.id(), mime_type, fd),
            ext_data_control_source_v1::Event::Cancelled => state.cancelled(source.id()),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(state: &mut Self, source: &ZwlrDataControlSourceV1, event: zwlr_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => state.send(source.id(), mime_type, fd),
            zwlr_data_control_source_v1::Event::Cancelled => state.cancelled(source.id()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_aliases_collapse_and_expand() {
        let offered = vec![
            "UTF8_STRING".to_string(),
            "TARGETS".to_string(),
            "image/png".to_string(),
            format!("{OWNER_MARKER_PREFIX}123"),
        ];
        assert_eq!(normalize_mimes(&offered), vec![TEXT_MIME.to_string(), "image/png".to_string()]);
        let advertised = advertised_mimes(&[TEXT_MIME.to_string()]);
        assert!(advertised.iter().any(|m| m == "STRING"));
        assert_eq!(fetch_mime("TEXT"), TEXT_MIME);
        assert_eq!(fetch_mime("image/png"), "image/png");
    }

    #[test]
    fn pipes_round_trip_with_timeouts() {
        let (read, write) = pipe().unwrap();
        let payload = vec![7u8; 300_000];
        let writer = std::thread::spawn(move || write_pipe(write, &payload, Duration::from_secs(2)));
        let got = read_pipe(read, CLIP_MAX_TOTAL, Duration::from_secs(2)).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(got.len(), 300_000);
    }
}
