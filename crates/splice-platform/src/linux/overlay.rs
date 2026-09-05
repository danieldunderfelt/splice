//! Capture without a portal: transparent layer-shell strips on the armed edges, a
//! pointer lock once the pointer pushes outward through a strip, relative-pointer
//! deltas while locked, an exclusive keyboard grab, and a hidden cursor.
//!
//! Works on every compositor with zwlr_layer_shell_v1 + pointer-constraints +
//! relative-pointer (KDE, wlroots family, COSMIC, niri); mutter has none of them.
//! Rules learned from lan-mouse (docs/research/linux-native-input.md): strips must be
//! thicker than 1 px (scaled outputs clamp the pointer just inside the edge), the lock
//! must be released with a position hint or the pointer stays stranded on the strip,
//! and entering a strip alone is not a crossing — the pointer has to push outward,
//! which is what a portal barrier requires too.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputInfo, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{self, Channel, Event as ChannelEvent};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{
    CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use smithay_client_toolkit::seat::pointer_constraints::{
    PointerConstraintsHandler, PointerConstraintsState,
};
use smithay_client_toolkit::seat::relative_pointer::{
    RelativeMotionEvent, RelativePointerHandler, RelativePointerState,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer,
    delegate_pointer_constraints, delegate_registry, delegate_relative_pointer, delegate_seat,
    delegate_shm, registry_handlers,
};
use splice_proto::{InputEvent, PointerButton, Vec2};
use tokio::sync::oneshot;
use wayland_client::globals::{registry_queue_init, GlobalList};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::Lifetime;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;

use super::backends::Driven;
use super::{PanicRelease, Shared, Stop};
use crate::{Capture, CaptureEvent, EdgeSide, EdgeSpec, PlatformError, PlatformEvent, Result};

/// Strip thickness in logical px. 1 px strips are unreachable on fractionally scaled
/// outputs on some compositors; 2 px is lan-mouse's field-tested value.
const STRIP_THICKNESS: u32 = 2;
/// After handing the pointer back, the strip stays disarmed until the pointer leaves
/// it, or this long: most compositors clamp the position hint to the strip itself.
const REARM_TIMEOUT: Duration = Duration::from_millis(750);
/// A lock request the compositor never activates (Hyprland main drops locks on layer
/// surfaces) is abandoned after this long so the session does not run uncaptured.
const LOCK_ACTIVATION_TIMEOUT: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(100);
const BTN_LEFT: u32 = 0x110;

enum Command {
    SetEdges(Vec<EdgeSpec>),
    EndCapture(Option<Vec2>),
    Panic,
    Shutdown,
    #[cfg(test)]
    Inspect(oneshot::Sender<(usize, usize)>),
}

pub struct OverlayCapture {
    cmd: channel::Sender<Command>,
}

#[async_trait::async_trait]
impl Capture for OverlayCapture {
    async fn set_edges(&self, edges: Vec<EdgeSpec>) -> Result<()> {
        self.cmd
            .send(Command::SetEdges(edges))
            .map_err(|_| PlatformError::Unavailable("overlay capture thread stopped".into()))
    }

    async fn begin_capture(&self) -> Result<()> {
        Ok(())
    }

    async fn end_capture(&self, warp_to: Option<Vec2>) -> Result<()> {
        self.cmd
            .send(Command::EndCapture(warp_to))
            .map_err(|_| PlatformError::Unavailable("overlay capture thread stopped".into()))
    }
}

pub async fn create(
    shared: Arc<Shared>,
    panic_chord: Vec<u32>,
    driven: Arc<Driven>,
) -> Result<(Arc<OverlayCapture>, PanicRelease, Stop)> {
    let (cmd, cmd_rx) = channel::channel();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
    std::thread::Builder::new()
        .name("splice-overlay".into())
        .spawn(move || run(shared, panic_chord, driven, cmd_rx, ready_tx))
        .map_err(|e| PlatformError::Unavailable(format!("cannot start overlay thread: {e}")))?;
    ready_rx
        .await
        .map_err(|_| PlatformError::Unavailable("overlay thread exited during setup".into()))??;
    let panic = PanicRelease::new({
        let cmd = cmd.clone();
        move || {
            let _ = cmd.send(Command::Panic);
        }
    });
    let stop = Stop::new({
        let cmd = cmd.clone();
        move || {
            let _ = cmd.send(Command::Shutdown);
        }
    });
    Ok((Arc::new(OverlayCapture { cmd }), panic, stop))
}

#[derive(Clone, Debug)]
struct StripGeom {
    edge: EdgeSpec,
    /// Global logical coordinates of the surface's top-left corner.
    origin: (i32, i32),
    size: (u32, u32),
}

struct Strip {
    geom: StripGeom,
    layer: LayerSurface,
    mapped: bool,
}

impl StripGeom {
    fn along(&self, local: (f64, f64)) -> f64 {
        let raw = match self.edge.side {
            EdgeSide::Left | EdgeSide::Right => f64::from(self.origin.1) + local.1,
            EdgeSide::Top | EdgeSide::Bottom => f64::from(self.origin.0) + local.0,
        };
        raw.clamp(f64::from(self.edge.from), f64::from((self.edge.to - 1).max(self.edge.from)))
    }

    fn outward(&self, delta: (f64, f64)) -> bool {
        match self.edge.side {
            EdgeSide::Left => delta.0 < 0.0,
            EdgeSide::Right => delta.0 > 0.0,
            EdgeSide::Top => delta.1 < 0.0,
            EdgeSide::Bottom => delta.1 > 0.0,
        }
    }

    /// Surface-local hint for handing the pointer back: the along-edge coordinate is
    /// honoured, the across coordinate is clamped into the strip because KWin and
    /// COSMIC reject hints outside the surface and sway ignores them entirely.
    fn hint(&self, warp_to: Option<Vec2>, fallback_along: f64) -> (f64, f64) {
        let (w, h) = (f64::from(self.size.0), f64::from(self.size.1));
        let (ox, oy) = (f64::from(self.origin.0), f64::from(self.origin.1));
        let along = match (self.edge.side, warp_to) {
            (EdgeSide::Left | EdgeSide::Right, Some(p)) => p.y - oy,
            (EdgeSide::Top | EdgeSide::Bottom, Some(p)) => p.x - ox,
            (_, None) => fallback_along,
        };
        match self.edge.side {
            EdgeSide::Left | EdgeSide::Right => (w / 2.0, along.clamp(0.5, h - 0.5)),
            EdgeSide::Top | EdgeSide::Bottom => (along.clamp(0.5, w - 0.5), h / 2.0),
        }
    }
}

/// Where the pointer is, tracked independently of the capture phase so a strip can
/// re-arm without a fresh wl_pointer.enter (the pointer never leaves the strip after
/// a release whose hint was clamped into it).
#[derive(Clone)]
struct Focus {
    surface: wl_surface::WlSurface,
    position: (f64, f64),
}

struct Locked {
    surface: wl_surface::WlSurface,
    lock: ZwpLockedPointerV1,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    requested: Instant,
    active: bool,
    edge_id: u32,
    along: f64,
    /// Surface-local along-edge coordinate where the crossing happened.
    along_local: f64,
}

struct State {
    shared: Arc<Shared>,
    panic_chord: Vec<u32>,
    driven: Arc<Driven>,
    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    constraints: PointerConstraintsState,
    relative: RelativePointerState,
    shortcuts: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<ThemedPointer>,
    relative_pointer: Option<ZwpRelativePointerV1>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    edges: Vec<EdgeSpec>,
    pending_edges: Option<Vec<EdgeSpec>>,
    strips: Vec<Strip>,
    focus: Option<Focus>,
    locked: Option<Locked>,
    pressed: HashSet<u32>,
    /// Strip disarmed after a release until the pointer leaves it or the deadline passes.
    rearm: Option<(wl_surface::WlSurface, Instant)>,
    running: bool,
}

/// Geometry of a strip covering `edge` on a display with logical `position` and
/// `size`, or None if the edge is not on this display's boundary: surface origin in
/// global coords, surface size, anchor, and (top, right, bottom, left) margins.
type StripGeometry = ((i32, i32), (u32, u32), Anchor, (i32, i32, i32, i32));

fn strip_geometry(edge: &EdgeSpec, position: (i32, i32), size: (i32, i32)) -> Option<StripGeometry> {
    let (lx, ly) = position;
    let (lw, lh) = size;
    let span = (edge.to - edge.from).max(1) as u32;
    let thick = STRIP_THICKNESS as i32;
    match edge.side {
        EdgeSide::Left | EdgeSide::Right => {
            let at_ok = match edge.side {
                EdgeSide::Left => edge.at == lx,
                _ => edge.at == lx + lw,
            };
            if !at_ok || edge.from < ly || edge.to > ly + lh {
                return None;
            }
            let top = edge.from - ly;
            let (anchor, origin_x) = match edge.side {
                EdgeSide::Left => (Anchor::LEFT | Anchor::TOP, lx),
                _ => (Anchor::RIGHT | Anchor::TOP, lx + lw - thick),
            };
            Some(((origin_x, edge.from), (STRIP_THICKNESS, span), anchor, (top, 0, 0, 0)))
        }
        EdgeSide::Top | EdgeSide::Bottom => {
            let at_ok = match edge.side {
                EdgeSide::Top => edge.at == ly,
                _ => edge.at == ly + lh,
            };
            if !at_ok || edge.from < lx || edge.to > lx + lw {
                return None;
            }
            let left = edge.from - lx;
            let (anchor, origin_y) = match edge.side {
                EdgeSide::Top => (Anchor::TOP | Anchor::LEFT, ly),
                _ => (Anchor::BOTTOM | Anchor::LEFT, ly + lh - thick),
            };
            Some(((edge.from, origin_y), (span, STRIP_THICKNESS), anchor, (0, 0, 0, left)))
        }
    }
}

/// Per-axis scroll translation. Wheel sources only count in whole detents: with the
/// v7 wl_pointer the compositor sends the smooth `axis` value for every hi-res
/// sub-step too, and forwarding both would double the scroll on the target.
fn scroll_events(
    horizontal: &smithay_client_toolkit::seat::pointer::AxisScroll,
    vertical: &smithay_client_toolkit::seat::pointer::AxisScroll,
    source: Option<wl_pointer::AxisSource>,
) -> Vec<InputEvent> {
    let mut out = Vec::with_capacity(2);
    let wheel = matches!(
        source,
        Some(wl_pointer::AxisSource::Wheel) | Some(wl_pointer::AxisSource::WheelTilt)
    );
    if horizontal.discrete != 0 || vertical.discrete != 0 {
        out.push(InputEvent::Scroll120 { dx: horizontal.discrete * 120, dy: vertical.discrete * 120 });
    } else if !wheel && (horizontal.absolute != 0.0 || vertical.absolute != 0.0) {
        out.push(InputEvent::ScrollPixels { dx: horizontal.absolute, dy: vertical.absolute });
    }
    if horizontal.stop || vertical.stop {
        out.push(InputEvent::ScrollStop { cancel: false });
    }
    out
}

impl State {
    fn strip_for(&self, surface: &wl_surface::WlSurface) -> Option<&Strip> {
        self.strips.iter().find(|s| s.layer.wl_surface() == surface)
    }

    fn rebuild_strips(&mut self, qh: &QueueHandle<Self>) {
        if self.locked.is_some() {
            return;
        }
        self.strips.clear();
        self.focus = None;
        self.rearm = None;
        let outputs: Vec<(wl_output::WlOutput, OutputInfo)> = self
            .outputs
            .outputs()
            .filter_map(|o| self.outputs.info(&o).map(|i| (o, i)))
            .collect();
        for edge in self.edges.clone() {
            let Some((output, info, geometry)) = outputs.iter().find_map(|(o, i)| {
                let position = i.logical_position?;
                let size = i.logical_size?;
                strip_geometry(&edge, position, size).map(|g| (o, i, g))
            }) else {
                tracing::warn!(?edge, "no output hosts this edge; not armed");
                continue;
            };
            let (origin, size, anchor, margin) = geometry;
            let surface = self.compositor.create_surface(qh);
            let layer = self.layer_shell.create_layer_surface(
                qh,
                surface,
                Layer::Overlay,
                Some("splice-edge"),
                Some(output),
            );
            layer.set_anchor(anchor);
            layer.set_size(size.0, size.1);
            layer.set_exclusive_zone(-1);
            layer.set_margin(margin.0, margin.1, margin.2, margin.3);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.commit();
            tracing::debug!(?edge, output = ?info.name, ?origin, ?size, "edge strip created");
            self.strips.push(Strip { geom: StripGeom { edge, origin, size }, layer, mapped: false });
        }
        self.update_edge_health();
    }

    fn update_edge_health(&self) {
        let armed = self.strips.iter().filter(|strip| strip.mapped).count();
        let expected = self.edges.len();
        self.shared.set_health(|h| {
            h.capture = (armed < expected).then(|| {
                format!(
                    "only {armed} of {expected} screen edges are armed; check display geometry and compositor support"
                )
            });
        });
    }

    fn paint(&mut self, index: usize) {
        let (w, h) = self.strips[index].geom.size;
        let stride = w as i32 * 4;
        let Ok((buffer, canvas)) =
            self.pool.create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("cannot allocate strip buffer");
            self.shared.set_health(|h| h.capture = Some("cannot allocate edge strip buffer".into()));
            return;
        };
        canvas.fill(0);
        let strip = &self.strips[index];
        let surface = strip.layer.wl_surface();
        if buffer.attach_to(surface).is_err() {
            tracing::warn!("cannot attach strip buffer");
            self.shared.set_health(|h| h.capture = Some("cannot attach edge strip buffer".into()));
            return;
        }
        surface.damage_buffer(0, 0, w as i32, h as i32);
        strip.layer.commit();
        self.strips[index].mapped = true;
        self.update_edge_health();
    }

    /// Whether outward motion over the focused strip may lock right now.
    fn armed_strip(&self) -> Option<(StripGeom, LayerSurface, (f64, f64))> {
        let focus = self.focus.as_ref()?;
        if self.locked.is_some() || self.driven.suppressed() {
            return None;
        }
        if self
            .rearm
            .as_ref()
            .is_some_and(|(s, deadline)| *s == focus.surface && Instant::now() < *deadline)
        {
            return None;
        }
        let strip = self.strip_for(&focus.surface)?;
        Some((strip.geom.clone(), strip.layer.clone(), focus.position))
    }

    /// KWin, sway and COSMIC only activate a lock on a surface that already holds
    /// keyboard focus, so the exclusive-interactivity commit must precede the lock.
    fn lock(&mut self, qh: &QueueHandle<Self>, geom: StripGeom, layer: LayerSurface, local: (f64, f64)) {
        let Some(themed) = &self.pointer else { return };
        let pointer = themed.pointer().clone();
        let surface = layer.wl_surface().clone();
        if let Err(err) = themed.hide_cursor() {
            tracing::debug!(error = ?err, "cannot hide cursor");
        }
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        let lock = match self.constraints.lock_pointer(&surface, &pointer, None, Lifetime::Persistent, qh) {
            Ok(lock) => lock,
            Err(err) => {
                tracing::warn!(error = %err, "pointer lock request failed");
                self.shared.set_health(|h| h.capture = Some(format!("pointer lock request failed: {err}")));
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                layer.commit();
                self.restore_cursor();
                return;
            }
        };
        let inhibitor = match (&self.shortcuts, &self.seat) {
            (Some(manager), Some(seat)) => Some(manager.inhibit_shortcuts(&surface, seat, qh, ())),
            _ => None,
        };
        surface.commit();
        self.pressed.clear();
        let along = geom.along(local);
        let along_local = match geom.edge.side {
            EdgeSide::Left | EdgeSide::Right => local.1,
            EdgeSide::Top | EdgeSide::Bottom => local.0,
        };
        self.locked = Some(Locked {
            surface,
            lock,
            inhibitor,
            requested: Instant::now(),
            active: false,
            edge_id: geom.edge.id,
            along,
            along_local,
        });
        tracing::debug!(edge_id = geom.edge.id, along, "edge strip crossed");
    }

    fn restore_cursor(&self) {
        if let Some(themed) = &self.pointer {
            if let Err(err) = themed.set_cursor_conn(CursorIcon::Default) {
                tracing::debug!(error = ?err, "cannot restore cursor");
            }
        }
    }

    fn release(&mut self, qh: &QueueHandle<Self>, warp_to: Option<Vec2>) {
        let Some(locked) = self.locked.take() else { return };
        if let Some(inhibitor) = locked.inhibitor {
            inhibitor.destroy();
        }
        if let Some(strip) = self.strip_for(&locked.surface) {
            let hint = strip.geom.hint(warp_to, locked.along_local);
            locked.lock.set_cursor_position_hint(hint.0, hint.1);
            strip.layer.wl_surface().commit();
            locked.lock.destroy();
            strip.layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            strip.layer.commit();
        } else {
            locked.lock.destroy();
        }
        self.restore_cursor();
        self.pressed.clear();
        self.rearm = Some((locked.surface, Instant::now() + REARM_TIMEOUT));
        self.apply_pending_edges(qh);
    }

    fn broken(&mut self, qh: &QueueHandle<Self>, reason: &str) {
        tracing::warn!(reason, "overlay capture broke");
        self.release(qh, None);
        self.shared.emit(PlatformEvent::Capture(CaptureEvent::Broken { reason: reason.into() }));
    }

    fn apply_pending_edges(&mut self, qh: &QueueHandle<Self>) {
        if self.locked.is_some() {
            return;
        }
        if let Some(edges) = self.pending_edges.take() {
            if edges != self.edges {
                self.edges = edges;
                self.rebuild_strips(qh);
            }
        }
    }

    fn tick(&mut self, qh: &QueueHandle<Self>) {
        if self.rearm.as_ref().is_some_and(|(_, deadline)| Instant::now() >= *deadline) {
            self.rearm = None;
        }
        let stale = self
            .locked
            .as_ref()
            .is_some_and(|l| !l.active && l.requested.elapsed() > LOCK_ACTIVATION_TIMEOUT);
        if stale {
            self.shared.set_health(|h| {
                h.capture = Some("the compositor did not activate the pointer lock on the edge strip".into())
            });
            self.broken(qh, "pointer lock never activated");
        }
        self.apply_pending_edges(qh);
    }

    fn forward(&self, ev: InputEvent) {
        self.shared.emit(PlatformEvent::Capture(CaptureEvent::Input(ev)));
    }

    fn handle_command(&mut self, qh: &QueueHandle<Self>, cmd: Command) {
        match cmd {
            #[cfg(test)]
            Command::Inspect(reply) => {
                let _ = reply
                    .send((self.outputs.outputs().count(), self.strips.iter().filter(|strip| strip.mapped).count()));
            }
            Command::SetEdges(edges) => {
                if self.locked.is_some() {
                    self.pending_edges = Some(edges);
                } else {
                    self.pending_edges = None;
                    if edges != self.edges {
                        self.edges = edges;
                        self.rebuild_strips(qh);
                    }
                }
            }
            Command::EndCapture(warp) => self.release(qh, warp),
            Command::Panic => {
                if self.locked.is_some() {
                    tracing::warn!("panic chord pressed");
                    self.release(qh, None);
                    self.shared.emit(PlatformEvent::Capture(CaptureEvent::Panic));
                }
            }
            Command::Shutdown => {
                self.release(qh, None);
                self.strips.clear();
                self.focus = None;
                self.running = false;
            }
        }
    }
}

trait SetCursorConn {
    fn set_cursor_conn(&self, icon: CursorIcon) -> std::result::Result<(), smithay_client_toolkit::seat::pointer::PointerThemeError>;
}

impl SetCursorConn for ThemedPointer {
    fn set_cursor_conn(&self, icon: CursorIcon) -> std::result::Result<(), smithay_client_toolkit::seat::pointer::PointerThemeError> {
        let conn = CONN.with(|c| c.borrow().clone());
        match conn {
            Some(conn) => self.set_cursor(&conn, icon),
            None => Err(smithay_client_toolkit::seat::pointer::PointerThemeError::MissingEnterSerial),
        }
    }
}

thread_local! {
    static CONN: std::cell::RefCell<Option<Connection>> = const { std::cell::RefCell::new(None) };
}

fn run(
    shared: Arc<Shared>,
    panic_chord: Vec<u32>,
    driven: Arc<Driven>,
    cmd_rx: Channel<Command>,
    ready: oneshot::Sender<Result<()>>,
) {
    let setup = (|| -> Result<(EventLoop<'static, State>, State, Connection, QueueHandle<State>)> {
        let unavailable = |what: &str, e: &dyn std::fmt::Display| {
            PlatformError::Unavailable(format!("overlay capture: {what}: {e}"))
        };
        let conn = Connection::connect_to_env().map_err(|e| unavailable("connect", &e))?;
        CONN.with(|c| *c.borrow_mut() = Some(conn.clone()));
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(|e| unavailable("registry", &e))?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).map_err(|e| unavailable("wl_compositor", &e))?;
        let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| unavailable("zwlr_layer_shell_v1", &e))?;
        let shm = Shm::bind(&globals, &qh).map_err(|e| unavailable("wl_shm", &e))?;
        let pool = SlotPool::new(64 * 1024, &shm).map_err(|e| unavailable("shm pool", &e))?;
        let constraints = PointerConstraintsState::bind(&globals, &qh);
        let relative = RelativePointerState::bind(&globals, &qh);
        let shortcuts = bind_shortcuts_inhibit(&globals, &qh);
        let event_loop: EventLoop<State> =
            EventLoop::try_new().map_err(|e| unavailable("event loop", &e))?;
        WaylandSource::new(conn.clone(), queue)
            .insert(event_loop.handle())
            .map_err(|e| unavailable("event source", &e))?;
        let state = State {
            shared: shared.clone(),
            panic_chord,
            driven,
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            seats: SeatState::new(&globals, &qh),
            compositor,
            layer_shell,
            shm,
            pool,
            constraints,
            relative,
            shortcuts,
            seat: None,
            pointer: None,
            relative_pointer: None,
            keyboard: None,
            edges: Vec::new(),
            pending_edges: None,
            strips: Vec::new(),
            focus: None,
            locked: None,
            pressed: HashSet::new(),
            rearm: None,
            running: true,
        };
        Ok((event_loop, state, conn, qh))
    })();
    let (mut event_loop, mut state, conn, qh) = match setup {
        Ok(parts) => parts,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let handle: LoopHandle<State> = event_loop.handle();
    let qh_cmd = qh.clone();
    if let Err(err) = handle.insert_source(cmd_rx, move |event, _, state| {
        if let ChannelEvent::Msg(cmd) = event {
            state.handle_command(&qh_cmd, cmd);
        } else {
            state.running = false;
        }
    }) {
        let _ = ready.send(Err(PlatformError::Unavailable(format!("overlay command source: {err}"))));
        return;
    }
    let qh_tick = qh.clone();
    let _ = handle.insert_source(Timer::from_duration(TICK), move |_, _, state| {
        state.tick(&qh_tick);
        TimeoutAction::ToDuration(TICK)
    });
    let _ = ready.send(Ok(()));
    shared.set_health(|h| h.capture = None);
    while state.running {
        if let Err(err) = event_loop.dispatch(None, &mut state) {
            tracing::warn!(error = %err, "overlay event loop failed");
            shared.set_health(|h| h.capture = Some(format!("overlay capture stopped: {err}")));
            shared.emit(PlatformEvent::Capture(CaptureEvent::Broken {
                reason: "overlay capture event loop failed".into(),
            }));
            return;
        }
    }
    let _ = conn.flush();
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
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.rebuild_strips(qh);
    }
    fn update_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.rebuild_strips(qh);
    }
    fn output_destroyed(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        if self.locked.is_some() {
            self.broken(qh, "output removed while capturing");
        }
        self.rebuild_strips(qh);
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let surface = layer.wl_surface().clone();
        if self.locked.as_ref().is_some_and(|l| l.surface == surface) {
            self.broken(qh, "edge strip closed by compositor");
        }
        self.strips.retain(|s| &s.layer != layer);
        if self.focus.as_ref().is_some_and(|f| f.surface == surface) {
            self.focus = None;
        }
        self.rebuild_strips(qh);
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        let Some(index) = self.strips.iter().position(|s| &s.layer == layer) else { return };
        let (w, h) = configure.new_size;
        if w > 0 && h > 0 {
            self.strips[index].geom.size = (w, h);
        }
        self.paint(index);
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_none() {
            self.seat = Some(seat);
        }
    }
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        match capability {
            Capability::Pointer if self.pointer.is_none() => {
                let cursor_surface = self.compositor.create_surface(qh);
                match self.seats.get_pointer_with_theme(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    cursor_surface,
                    ThemeSpec::default(),
                ) {
                    Ok(themed) => {
                        match self.relative.get_relative_pointer(themed.pointer(), qh) {
                            Ok(rel) => self.relative_pointer = Some(rel),
                            Err(err) => tracing::warn!(error = %err, "no relative pointer"),
                        }
                        self.pointer = Some(themed);
                    }
                    Err(err) => tracing::warn!(error = %err, "cannot get pointer"),
                }
            }
            Capability::Keyboard if self.keyboard.is_none() => {
                self.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            _ => {}
        }
    }
    fn remove_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        match capability {
            Capability::Pointer => {
                if self.locked.is_some() {
                    self.broken(qh, "pointer capability removed");
                }
                if let Some(rel) = self.relative_pointer.take() {
                    rel.destroy();
                }
                self.pointer = None;
                self.focus = None;
            }
            Capability::Keyboard => {
                if let Some(keyboard) = self.keyboard.take() {
                    keyboard.release();
                }
            }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.as_ref() == Some(&seat) {
            if self.locked.is_some() {
                self.broken(qh, "seat removed");
            }
            self.seat = None;
            self.pointer = None;
            self.relative_pointer = None;
            self.keyboard = None;
            self.focus = None;
        }
    }
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            match &event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    if self.strip_for(&event.surface).is_some() {
                        self.focus = Some(Focus { surface: event.surface.clone(), position: event.position });
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.locked.as_ref().is_some_and(|l| l.surface == event.surface) {
                        self.broken(qh, "pointer left the locked edge strip");
                    }
                    if self.rearm.as_ref().is_some_and(|(s, _)| *s == event.surface) {
                        self.rearm = None;
                    }
                    if self.focus.as_ref().is_some_and(|f| f.surface == event.surface) {
                        self.focus = None;
                    }
                }
                PointerEventKind::Press { button, .. } | PointerEventKind::Release { button, .. } => {
                    if self.locked.is_none() {
                        continue;
                    }
                    let pressed = matches!(event.kind, PointerEventKind::Press { .. });
                    let mapped = match *button {
                        0x110 => Some(PointerButton::Left),
                        0x111 => Some(PointerButton::Right),
                        0x112 => Some(PointerButton::Middle),
                        0x113 => Some(PointerButton::Back),
                        0x114 => Some(PointerButton::Forward),
                        code @ 0x110..=0x20f => Some(PointerButton::Other((code - BTN_LEFT) as u8)),
                        _ => None,
                    };
                    if let Some(button) = mapped {
                        self.forward(InputEvent::Button { button, pressed });
                    }
                }
                PointerEventKind::Axis { horizontal, vertical, source, .. } => {
                    if self.locked.is_none() {
                        continue;
                    }
                    for ev in scroll_events(horizontal, vertical, *source) {
                        self.forward(ev);
                    }
                }
            }
        }
    }
}

impl RelativePointerHandler for State {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &ZwpRelativePointerV1,
        _: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        if let Some(locked) = &self.locked {
            if locked.active {
                self.forward(InputEvent::Motion { dx: event.delta.0, dy: event.delta.1 });
            }
            return;
        }
        if let Some((geom, layer, local)) = self.armed_strip() {
            if geom.outward(event.delta) {
                self.lock(qh, geom, layer, local);
            }
        }
    }
}

impl PointerConstraintsHandler for State {
    fn confined(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ZwpConfinedPointerV1, _: &wl_surface::WlSurface, _: &wl_pointer::WlPointer) {}
    fn unconfined(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ZwpConfinedPointerV1, _: &wl_surface::WlSurface, _: &wl_pointer::WlPointer) {}
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, locked: &ZwpLockedPointerV1, _: &wl_surface::WlSurface, _: &wl_pointer::WlPointer) {
        if let Some(l) = &mut self.locked {
            if &l.lock == locked && !l.active {
                l.active = true;
                self.shared.emit(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: l.edge_id, along: l.along }));
                for event in crate::keymap::held_key_presses(self.pressed.iter().copied()) {
                    self.forward(event);
                }
            }
        }
        self.update_edge_health();
    }
    fn unlocked(&mut self, _: &Connection, qh: &QueueHandle<Self>, locked: &ZwpLockedPointerV1, _: &wl_surface::WlSurface, _: &wl_pointer::WlPointer) {
        if self.locked.as_ref().is_some_and(|l| &l.lock == locked) {
            self.broken(qh, "pointer lock released by compositor");
        }
    }
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

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Key { key, state: key_state, .. } => {
                let pressed = matches!(key_state, wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed));
                if pressed {
                    state.pressed.insert(key);
                } else {
                    state.pressed.remove(&key);
                }
                if state.locked.is_none() {
                    return;
                }
                if !state.panic_chord.is_empty() && state.panic_chord.iter().all(|c| state.pressed.contains(c)) {
                    tracing::warn!("panic chord pressed");
                    state.release(qh, None);
                    state.shared.emit(PlatformEvent::Capture(CaptureEvent::Panic));
                    return;
                }
                state.forward(InputEvent::Key { code: key, pressed });
            }
            wl_keyboard::Event::Enter { keys, .. } => {
                state.pressed.clear();
                for chunk in keys.as_chunks::<4>().0 {
                    state.pressed.insert(u32::from_ne_bytes(*chunk));
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                state.pressed.clear();
                if state.locked.is_some() {
                    state.broken(qh, "keyboard focus left the edge strip while capturing");
                }
            }
            _ => {}
        }
    }
}

fn bind_shortcuts_inhibit(globals: &GlobalList, qh: &QueueHandle<State>) -> Option<ZwpKeyboardShortcutsInhibitManagerV1> {
    globals.bind(qh, 1..=1, ()).ok()
}

delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitManagerV1);

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(_: &mut Self, _: &ZwpKeyboardShortcutsInhibitorV1, _: zwp_keyboard_shortcuts_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_pointer!(State);
delegate_relative_pointer!(State);
delegate_pointer_constraints!(State);
delegate_layer!(State);
delegate_registry!(State);

#[cfg(test)]
mod tests {
    use super::*;
    use smithay_client_toolkit::seat::pointer::AxisScroll;

    #[tokio::test]
    #[ignore = "requires a running Wayland compositor with layer-shell"]
    async fn live_overlay_arms_both_edges_after_startup() {
        let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            capture_control: Default::default(),
            emission: parking_lot::Mutex::new(()),
            tx,
            health: parking_lot::Mutex::new(crate::HealthReport::default()),
            displays: parking_lot::RwLock::new(Vec::new()),
            epoch: Instant::now(),
            last_injection: std::sync::atomic::AtomicU64::new(0),
            injected_keys: parking_lot::Mutex::new(std::collections::VecDeque::new()),
        });
        let displays = super::super::displays::spawn(shared.clone()).unwrap();
        let display = &displays[0];
        let edges = vec![
            EdgeSpec { id: 0, side: EdgeSide::Left, at: display.x, from: display.y, to: display.y + display.h as i32 },
            EdgeSpec {
                id: 1,
                side: EdgeSide::Right,
                at: display.x + display.w as i32,
                from: display.y,
                to: display.y + display.h as i32,
            },
        ];
        let (capture, _, stop) = create(shared.clone(), vec![42, 54, 1], Arc::new(Driven::default())).await.unwrap();
        capture.set_edges(edges).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (tx, rx) = oneshot::channel();
        capture.cmd.send(Command::Inspect(tx)).unwrap();
        let (_, strips) = tokio::time::timeout(Duration::from_secs(2), rx).await.unwrap().unwrap();
        assert_eq!(strips, 2, "both KDE edges must have an input surface");
        capture
            .set_edges(vec![EdgeSpec {
                id: 2,
                side: EdgeSide::Right,
                at: display.x + display.w as i32 + 20,
                from: display.y,
                to: display.y + display.h as i32,
            }])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let reported = shared.health.lock().capture.is_some();
        stop.stop();
        assert!(reported, "an unarmed edge must be visible in capture health");
    }

    #[test]
    fn strip_along_outward_and_hint_follow_the_edge_side() {
        let edge = EdgeSpec { id: 1, side: EdgeSide::Left, at: 0, from: 100, to: 1000 };
        let strip = StripGeom { edge, origin: (0, 100), size: (2, 900) };
        assert_eq!(strip.along((1.0, 50.5)), 150.5);
        assert_eq!(strip.along((1.0, 5000.0)), 999.0);
        assert!(strip.outward((-0.5, 3.0)));
        assert!(!strip.outward((0.5, -3.0)));
        assert_eq!(strip.hint(Some(Vec2 { x: 40.0, y: 400.0 }), 7.0), (1.0, 300.0));
        assert_eq!(strip.hint(None, 7.0), (1.0, 7.0));
        assert_eq!(strip.hint(Some(Vec2 { x: 40.0, y: -50.0 }), 7.0), (1.0, 0.5));
    }

    #[test]
    fn strip_geometry_matches_edges_to_display_boundaries() {
        let right = EdgeSpec { id: 1, side: EdgeSide::Right, at: 1920, from: 100, to: 1000 };
        let ((ox, oy), (w, h), _, margin) = strip_geometry(&right, (0, 0), (1920, 1080)).unwrap();
        assert_eq!((ox, oy), (1918, 100));
        assert_eq!((w, h), (2, 900));
        assert_eq!(margin, (100, 0, 0, 0));
        assert!(strip_geometry(&right, (1920, 0), (1920, 1080)).is_none());
        let bottom = EdgeSpec { id: 2, side: EdgeSide::Bottom, at: 1080, from: 1920, to: 3840 };
        let ((ox, oy), (w, h), _, margin) = strip_geometry(&bottom, (1920, 0), (1920, 1080)).unwrap();
        assert_eq!((ox, oy), (1920, 1078));
        assert_eq!((w, h), (1920, 2));
        assert_eq!(margin, (0, 0, 0, 0));
    }

    #[test]
    fn wheel_sub_steps_are_not_double_counted() {
        let sub = AxisScroll { absolute: 2.0, discrete: 0, stop: false };
        let none = AxisScroll::default();
        assert!(scroll_events(&none, &sub, Some(wl_pointer::AxisSource::Wheel)).is_empty());
        let detent = AxisScroll { absolute: 15.0, discrete: 1, stop: false };
        assert_eq!(
            scroll_events(&none, &detent, Some(wl_pointer::AxisSource::Wheel)),
            vec![InputEvent::Scroll120 { dx: 0, dy: 120 }]
        );
        let finger = AxisScroll { absolute: 3.5, discrete: 0, stop: false };
        assert_eq!(
            scroll_events(&none, &finger, Some(wl_pointer::AxisSource::Finger)),
            vec![InputEvent::ScrollPixels { dx: 0.0, dy: 3.5 }]
        );
        let stop = AxisScroll { absolute: 0.0, discrete: 0, stop: true };
        assert_eq!(
            scroll_events(&stop, &finger, Some(wl_pointer::AxisSource::Finger)),
            vec![InputEvent::ScrollPixels { dx: 0.0, dy: 3.5 }, InputEvent::ScrollStop { cancel: false }]
        );
    }
}
