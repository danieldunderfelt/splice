//! Native drag surfaces for the continuous cross-edge file drag.
//!
//! `attach` is the destination half: before the carried button press is injected at
//! the entry point, an origin surface is mapped there so the press lands on it with a
//! serial the compositor honours in `wl_data_device.start_drag`. Once the drag runs the
//! origin must stop being a drop target. With a layer shell (KWin, wlroots) its input
//! region is emptied; without one (Mutter) the fullscreen toplevel is unmapped, which
//! keeps the drag alive because Mutter only ends a drag when the origin resource is
//! destroyed. Both were verified in `docs/research/drag-attach-spike-results.md`.
//!
//! `catch` is the source half where no edge strip can receive the drag: a fullscreen
//! transparent toplevel mapped while a foreign drag is in flight becomes its drop
//! target on the next motion, accepts `text/uri-list`, reads the selection at the drop
//! and finishes the offer so the source application ends its drag with a copy.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{DataSourceHandler, DragSource};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_data_device, delegate_layer, delegate_output,
    delegate_pointer, delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::{
    wl_data_device, wl_data_source, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, QueueHandle};

use crate::files::{encode_uri_list, parse_uri_list, MIME_PORTAL_FILETRANSFER, MIME_URI_LIST};
use crate::{PlatformError, Result};

const BTN_LEFT: u32 = 0x110;
const ORIGIN_SIZE: u32 = 200;
const TICK: Duration = Duration::from_millis(50);
const APP_ID: &str = "dev.splice.drag";

/// What the destination side needs to turn the carried press into a native drag.
#[derive(Clone, Debug)]
pub struct AttachRequest {
    /// Logical global coordinates where the carried press will be injected.
    pub entry: (i32, i32),
    /// Local paths the drag offers, normally the roots of a deferred FUSE view.
    pub uris: Vec<PathBuf>,
    pub portal_key: Option<String>,
    /// How long to wait for the press after the origin surface is mapped.
    pub press_timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginKind {
    LayerShell,
    FullscreenToplevel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachEvent {
    /// The origin surface is mapped under the entry point; inject the press now.
    Armed(OriginKind),
    /// `start_drag` was sent with the press serial.
    Started,
    /// A destination accepted the drop; the view's read gate may open.
    Dropped,
    /// The destination finished with the offer.
    Finished,
    Cancelled(String),
}

/// What the source side needs to catch a foreign drag with no edge strip.
#[derive(Clone, Debug)]
pub struct CatchRequest {
    /// A logical global point on the output that should host the catcher.
    pub near: (i32, i32),
    /// How long to wait for a drag to enter after the catcher is mapped.
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatchEvent {
    Mapped,
    Entered { mime_types: Vec<String> },
    Dropped { uris: Vec<PathBuf>, portal_key: Option<String> },
    Cancelled(String),
}

/// A running attach or catch; dropping it cancels and joins the worker thread.
pub struct Session {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn attach(request: AttachRequest, events: impl Fn(AttachEvent) + Send + 'static) -> Result<Session> {
    let role = Role::Attach {
        request,
        events: Box::new(events),
        source: None,
        dropped: false,
        finished: false,
    };
    spawn(role)
}

pub fn catch(request: CatchRequest, events: impl Fn(CatchEvent) + Send + 'static) -> Result<Session> {
    let role = Role::Catch {
        request,
        events: Box::new(events),
        entered: false,
    };
    spawn(role)
}

fn spawn(role: Role) -> Result<Session> {
    let cancel = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let worker_cancel = Arc::clone(&cancel);
    let thread = std::thread::Builder::new()
        .name("splice-drag-attach".into())
        .spawn(move || run(role, worker_cancel, ready_tx))
        .map_err(|e| PlatformError::Unavailable(format!("drag attach thread: {e}")))?;
    ready_rx
        .recv()
        .map_err(|_| PlatformError::Unavailable("drag attach setup ended early".into()))??;
    Ok(Session {
        cancel,
        thread: Some(thread),
    })
}

enum Role {
    Attach {
        request: AttachRequest,
        events: Box<dyn Fn(AttachEvent) + Send>,
        source: Option<DragSource>,
        dropped: bool,
        finished: bool,
    },
    Catch {
        request: CatchRequest,
        events: Box<dyn Fn(CatchEvent) + Send>,
        entered: bool,
    },
}

impl Role {
    fn point(&self) -> (i32, i32) {
        match self {
            Role::Attach { request, .. } => request.entry,
            Role::Catch { request, .. } => request.near,
        }
    }

    fn deadline(&self) -> Duration {
        match self {
            Role::Attach { request, .. } => request.press_timeout,
            Role::Catch { request, .. } => request.timeout,
        }
    }

    fn cancelled(&self, reason: String) {
        match self {
            Role::Attach { events, .. } => events(AttachEvent::Cancelled(reason)),
            Role::Catch { events, .. } => events(CatchEvent::Cancelled(reason)),
        }
    }
}

enum Origin {
    Layer(LayerSurface),
    Toplevel(Window),
}

impl Origin {
    fn wl_surface(&self) -> &wl_surface::WlSurface {
        match self {
            Origin::Layer(layer) => layer.wl_surface(),
            Origin::Toplevel(window) => window.wl_surface(),
        }
    }

    fn kind(&self) -> OriginKind {
        match self {
            Origin::Layer(_) => OriginKind::LayerShell,
            Origin::Toplevel(_) => OriginKind::FullscreenToplevel,
        }
    }
}

struct State {
    role: Role,
    cancel: Arc<AtomicBool>,
    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    data_device_manager: DataDeviceManagerState,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    data_device: Option<DataDevice>,
    origin: Option<Origin>,
    size: (u32, u32),
    chosen_size: (u32, u32),
    mapped: bool,
    deadline: Option<Instant>,
    running: bool,
}

fn run(role: Role, cancel: Arc<AtomicBool>, ready: std::sync::mpsc::Sender<Result<()>>) {
    let unavailable = |what: &str, e: &dyn std::fmt::Display| {
        PlatformError::Unavailable(format!("drag attach: {what}: {e}"))
    };
    let setup = (|| -> Result<(EventLoop<'static, State>, State, Connection, QueueHandle<State>)> {
        let conn = Connection::connect_to_env().map_err(|e| unavailable("connect", &e))?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| unavailable("registry", &e))?;
        let qh = queue.handle();
        let compositor =
            CompositorState::bind(&globals, &qh).map_err(|e| unavailable("wl_compositor", &e))?;
        let shm = Shm::bind(&globals, &qh).map_err(|e| unavailable("wl_shm", &e))?;
        let pool = SlotPool::new(64 * 1024, &shm).map_err(|e| unavailable("shm pool", &e))?;
        let data_device_manager = DataDeviceManagerState::bind(&globals, &qh)
            .map_err(|e| unavailable("wl_data_device_manager", &e))?;
        let xdg = XdgShell::bind(&globals, &qh).map_err(|e| unavailable("xdg_wm_base", &e))?;
        let layer_shell = LayerShell::bind(&globals, &qh).ok();
        let mut state = State {
            role,
            cancel,
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            seats: SeatState::new(&globals, &qh),
            compositor,
            shm,
            pool,
            data_device_manager,
            seat: None,
            pointer: None,
            data_device: None,
            origin: None,
            size: (ORIGIN_SIZE, ORIGIN_SIZE),
            chosen_size: (ORIGIN_SIZE, ORIGIN_SIZE),
            mapped: false,
            deadline: None,
            running: true,
        };
        for seat in state.seats.seats().collect::<Vec<_>>() {
            state.new_seat(&conn, &qh, seat);
        }
        for _ in 0..2 {
            queue.roundtrip(&mut state).map_err(|e| unavailable("roundtrip", &e))?;
        }
        if state.data_device.is_none() {
            return Err(unavailable("seat", &"no wl_seat with a data device"));
        }
        let point = state.role.point();
        let (output, position, logical) = state
            .outputs
            .outputs()
            .find_map(|output| {
                let info = state.outputs.info(&output)?;
                let position = info.logical_position?;
                let size = info.logical_size?;
                let inside = point.0 >= position.0
                    && point.0 < position.0 + size.0
                    && point.1 >= position.1
                    && point.1 < position.1 + size.1;
                inside.then_some((output, position, size))
            })
            .ok_or_else(|| {
                unavailable("output", &format!("no output contains {},{}", point.0, point.1))
            })?;
        let surface = state.compositor.create_surface(&qh);
        let origin = match (&state.role, layer_shell) {
            (Role::Attach { .. }, Some(layer_shell)) => {
                let layer = layer_shell.create_layer_surface(
                    &qh,
                    surface,
                    Layer::Overlay,
                    Some("splice-drag-origin"),
                    Some(&output),
                );
                let half = ORIGIN_SIZE as i32 / 2;
                let clamp = |v: i32, extent: i32| v.clamp(0, (extent - ORIGIN_SIZE as i32).max(0));
                let left = clamp(point.0 - position.0 - half, logical.0);
                let top = clamp(point.1 - position.1 - half, logical.1);
                layer.set_anchor(Anchor::TOP | Anchor::LEFT);
                layer.set_size(ORIGIN_SIZE, ORIGIN_SIZE);
                layer.set_exclusive_zone(-1);
                layer.set_margin(top, 0, 0, left);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                layer.commit();
                Origin::Layer(layer)
            }
            _ => {
                state.chosen_size = (logical.0 as u32, logical.1 as u32);
                let window = xdg.create_window(surface, WindowDecorations::RequestClient, &qh);
                window.set_app_id(APP_ID);
                window.set_title("Splice drag");
                window.set_fullscreen(Some(&output));
                window.commit();
                Origin::Toplevel(window)
            }
        };
        state.origin = Some(origin);
        let event_loop: EventLoop<State> =
            EventLoop::try_new().map_err(|e| unavailable("event loop", &e))?;
        WaylandSource::new(conn.clone(), queue)
            .insert(event_loop.handle())
            .map_err(|e| unavailable("event source", &e))?;
        event_loop
            .handle()
            .insert_source(Timer::from_duration(TICK), |_, _, state: &mut State| {
                state.tick();
                TimeoutAction::ToDuration(TICK)
            })
            .map_err(|e| unavailable("timer", &e))?;
        Ok((event_loop, state, conn, qh))
    })();
    let (mut event_loop, mut state, conn, _qh) = match setup {
        Ok(parts) => parts,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    while state.running {
        if let Err(err) = event_loop.dispatch(None, &mut state) {
            state.finish(Some(format!("event loop failed: {err}")));
        }
    }
    let _ = conn.flush();
}

impl State {
    fn tick(&mut self) {
        if self.cancel.load(Ordering::Acquire) {
            self.finish(Some("cancelled".into()));
            return;
        }
        let waiting = match &self.role {
            Role::Attach { source, .. } => source.is_none(),
            Role::Catch { entered, .. } => !entered,
        };
        if waiting && self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let what = match &self.role {
                Role::Attach { .. } => "no press landed on the origin surface in time",
                Role::Catch { .. } => "no drag entered the catcher surface in time",
            };
            self.finish(Some(what.into()));
        }
    }

    fn finish(&mut self, cancelled: Option<String>) {
        if !self.running {
            return;
        }
        self.running = false;
        if let Some(reason) = cancelled {
            self.role.cancelled(reason);
        }
        self.origin = None;
    }

    fn paint(&mut self) {
        let Some(origin) = &self.origin else {
            return;
        };
        let (w, h) = self.size;
        let stride = w as i32 * 4;
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
        else {
            self.finish(Some("cannot allocate the origin buffer".into()));
            return;
        };
        canvas.fill(0);
        let surface = origin.wl_surface();
        if buffer.attach_to(surface).is_err() {
            self.finish(Some("cannot attach the origin buffer".into()));
            return;
        }
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        if !self.mapped {
            self.mapped = true;
            self.deadline = Some(Instant::now() + self.role.deadline());
            match &self.role {
                Role::Attach { events, .. } => events(AttachEvent::Armed(origin.kind())),
                Role::Catch { events, .. } => events(CatchEvent::Mapped),
            }
        }
    }

    fn start_drag(&mut self, qh: &QueueHandle<Self>, serial: u32) {
        let Some(device) = &self.data_device else {
            return;
        };
        let Some(origin) = &self.origin else {
            return;
        };
        let Role::Attach {
            request,
            events,
            source,
            ..
        } = &mut self.role
        else {
            return;
        };
        if source.is_some() {
            return;
        }
        let mut mimes = vec![MIME_URI_LIST];
        if request.portal_key.is_some() {
            mimes.push(MIME_PORTAL_FILETRANSFER);
        }
        let drag = self
            .data_device_manager
            .create_drag_and_drop_source(qh, mimes, DndAction::Copy);
        drag.start_drag(device, origin.wl_surface(), None, serial);
        *source = Some(drag);
        events(AttachEvent::Started);
        match origin {
            Origin::Layer(layer) => match Region::new(&self.compositor) {
                Ok(region) => {
                    layer.wl_surface().set_input_region(Some(region.wl_region()));
                    layer.wl_surface().commit();
                }
                Err(err) => self.finish(Some(format!("cannot empty the origin input region: {err}"))),
            },
            Origin::Toplevel(window) => {
                window.wl_surface().attach(None, 0, 0);
                window.wl_surface().commit();
            }
        }
    }

    fn accept_offer(&mut self, offer: &DragOffer) {
        let Role::Catch {
            events, entered, ..
        } = &mut self.role
        else {
            return;
        };
        let mime_types = offer.with_mime_types(|m| m.to_vec());
        if !mime_types.iter().any(|m| m == MIME_URI_LIST) {
            offer.accept_mime_type(offer.serial, None);
            if !*entered {
                *entered = true;
                events(CatchEvent::Entered {
                    mime_types: mime_types.clone(),
                });
            }
            self.finish(Some("the drag offers no file list".into()));
            return;
        }
        offer.accept_mime_type(offer.serial, Some(MIME_URI_LIST.to_owned()));
        offer.set_actions(DndAction::Copy, DndAction::Copy);
        if !*entered {
            *entered = true;
            events(CatchEvent::Entered { mime_types });
        }
    }

    fn drag_offer(&self) -> Option<DragOffer> {
        self.data_device.as_ref().and_then(|d| d.data().drag_offer())
    }
}

fn read_offer(conn: &Connection, offer: &DragOffer, mime: &str) -> std::io::Result<Vec<u8>> {
    let pipe = offer.receive(mime.to_owned())?;
    let _ = conn.flush();
    let mut file = std::fs::File::from(std::os::fd::OwnedFd::from(pipe));
    let mut body = Vec::new();
    file.read_to_end(&mut body)?;
    Ok(body)
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.finish(Some("the compositor closed the origin surface".into()));
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        let (w, h) = configure.new_size;
        if w > 0 && h > 0 {
            self.size = (w, h);
        }
        if !self.mapped {
            self.paint();
        }
    }
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.finish(Some("the compositor closed the origin window".into()));
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window, configure: WindowConfigure, _: u32) {
        self.size = (
            configure.new_size.0.map_or(self.chosen_size.0, |w| w.get()),
            configure.new_size.1.map_or(self.chosen_size.1, |h| h.get()),
        );
        tracing::debug!(size = ?self.size, states = ?configure.state, "drag surface configured");
        if !self.mapped {
            self.paint();
        }
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_none() {
            self.data_device = Some(self.data_device_manager.get_data_device(qh, &seat));
            self.seat = Some(seat);
        }
    }
    fn new_capability(&mut self, conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if self.seat.is_none() {
            self.new_seat(conn, qh, seat.clone());
        }
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seats.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer {
            self.pointer = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let ours = self.origin.as_ref().is_some_and(|o| o.wl_surface() == &event.surface);
            if !ours {
                continue;
            }
            if let PointerEventKind::Press { button, serial, .. } = event.kind {
                if button == BTN_LEFT {
                    self.start_drag(qh, serial);
                }
            }
        }
    }
}

impl DataDeviceHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice, _: f64, _: f64, _: &wl_surface::WlSurface) {
        if let Some(offer) = self.drag_offer() {
            self.accept_offer(&offer);
        }
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {
        if matches!(self.role, Role::Catch { entered: true, .. }) {
            self.finish(Some("the drag left the catcher surface".into()));
        }
    }
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice, _: f64, _: f64) {
        if let Some(offer) = self.drag_offer() {
            self.accept_offer(&offer);
        }
    }
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {}
    fn drop_performed(&mut self, conn: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {
        let Role::Catch { .. } = self.role else {
            return;
        };
        let Some(offer) = self.drag_offer() else {
            self.finish(Some("drop without an offer".into()));
            return;
        };
        let list = match read_offer(conn, &offer, MIME_URI_LIST) {
            Ok(list) => list,
            Err(err) => {
                self.finish(Some(format!("cannot read the dropped file list: {err}")));
                return;
            }
        };
        let has_portal = offer.with_mime_types(|m| m.iter().any(|m| m == MIME_PORTAL_FILETRANSFER));
        let portal_key = has_portal
            .then(|| read_offer(conn, &offer, MIME_PORTAL_FILETRANSFER).ok())
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|key| key.trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_owned())
            .filter(|key| !key.is_empty());
        offer.finish();
        let uris = parse_uri_list(&list);
        if let Role::Catch { events, .. } = &self.role {
            events(CatchEvent::Dropped { uris, portal_key });
        }
        self.finish(None);
    }
}

impl DataOfferHandler for State {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl DataSourceHandler for State {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource, _: Option<String>) {}
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource, mime: String, fd: WritePipe) {
        let Role::Attach { request, .. } = &self.role else {
            return;
        };
        let body = match mime.as_str() {
            MIME_URI_LIST => encode_uri_list(&request.uris).into_bytes(),
            MIME_PORTAL_FILETRANSFER => request.portal_key.clone().unwrap_or_default().into_bytes(),
            _ => return,
        };
        let mut file = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
        let _ = file.write_all(&body).and_then(|_| file.flush());
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {
        if let Role::Attach { finished: false, .. } = self.role {
            self.finish(Some("the compositor cancelled the drag".into()));
        }
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {
        if let Role::Attach { events, dropped, .. } = &mut self.role {
            if !*dropped {
                *dropped = true;
                events(AttachEvent::Dropped);
            }
        }
    }
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {
        if let Role::Attach { events, finished, .. } = &mut self.role {
            if !*finished {
                *finished = true;
                events(AttachEvent::Finished);
            }
        }
        self.finish(None);
    }
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource, _: DndAction) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_pointer!(State);
delegate_layer!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_data_device!(State);
delegate_registry!(State);
