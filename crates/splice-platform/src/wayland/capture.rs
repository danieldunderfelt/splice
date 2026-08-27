//! Capture side (this machine as source): InputCapture portal session + reis receiver.
//!
//! Load-bearing rules from docs/research/wayland-input.md:
//! - NEVER call `Disable()` (mutter bug #3908); barrier changes are SetPointerBarriers +
//!   Enable on the SAME session, and barrier updates already suspend the session.
//! - Restore tokens are single-use; the replacement is persisted on every Start.
//! - Capture activates only when the cursor hits a barrier; there is no way to force it,
//!   so `begin_capture` is a no-op and forwarding starts at `Activated`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use parking_lot::RwLock;
use reis::ei;
use reis::ei::button::ButtonState;
use reis::ei::keyboard::KeyState;
use reis::event::{DeviceCapability, EiEvent};
use splice_proto::{DisplayRect, InputEvent, PointerButton, Vec2};
use tokio::sync::{mpsc, Notify};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, Value};

use super::portal::{self, Options};
use super::tokens::{TokenKind, TokenStore};
use super::WaylandShared;
use crate::{Capture, CaptureEvent, EdgeSide, EdgeSpec, PlatformError, PlatformEvent, Result};

const IFACE: &str = "org.freedesktop.portal.InputCapture";
const CAP_KEYBOARD: u32 = 1;
const CAP_POINTER: u32 = 2;
/// persist_mode: persist until explicitly revoked.
const PERSIST: u32 = 2;
/// set_edges calls are batched; barriers are applied after this quiet period.
const EDGE_DEBOUNCE: Duration = Duration::from_millis(300);
/// Session re-establishment: at least 1 s of backoff, at most one recreation per 5 s
/// (lan-mouse's 34-reconnects-per-2h fd leak is the failure mode this prevents).
const RECREATE_MIN_BACKOFF: Duration = Duration::from_secs(1);
const RECREATE_MIN_INTERVAL: Duration = Duration::from_secs(5);

const BTN_LEFT: u32 = 0x110;

enum Command {
    ApplyEdges(Vec<EdgeSpec>),
    EndCapture { warp_to: Option<Vec2> },
}

pub struct WaylandCapture {
    edges: Arc<RwLock<Vec<EdgeSpec>>>,
    edges_dirty: Arc<Notify>,
    cmd: mpsc::UnboundedSender<Command>,
}

#[async_trait::async_trait]
impl Capture for WaylandCapture {
    async fn set_edges(&self, edges: Vec<EdgeSpec>) -> Result<()> {
        *self.edges.write() = edges;
        self.edges_dirty.notify_one();
        Ok(())
    }

    async fn begin_capture(&self) -> Result<()> {
        Ok(())
    }

    async fn end_capture(&self, warp_to: Option<Vec2>) -> Result<()> {
        let _ = self.cmd.send(Command::EndCapture { warp_to });
        Ok(())
    }
}

pub fn create(
    shared: Arc<WaylandShared>,
    tokens: Arc<TokenStore>,
    conn: zbus::Connection,
    panic_chord: Vec<u32>,
) -> Arc<WaylandCapture> {
    let edges = Arc::new(RwLock::new(Vec::new()));
    let edges_dirty = Arc::new(Notify::new());
    let (cmd, cmd_rx) = mpsc::unbounded_channel();

    {
        let edges = edges.clone();
        let edges_dirty = edges_dirty.clone();
        let cmd = cmd.clone();
        let _ = tokio::spawn(async move {
            loop {
                edges_dirty.notified().await;
                // Trailing debounce: keep waiting while more edge updates arrive.
                while tokio::time::timeout(EDGE_DEBOUNCE, edges_dirty.notified())
                    .await
                    .is_ok()
                {}
                let _ = cmd.send(Command::ApplyEdges(edges.read().clone()));
            }
        });
    }

    // reis's EiConvertEventStream is not Send (its converter holds boxed FnOnce
    // callbacks), so the portal/reis pump cannot be tokio::spawn'd. It runs on a
    // dedicated current-thread runtime where block_on needs no Send bound.
    let edges_handle = edges.clone();
    let _ = std::thread::Builder::new()
        .name("splice-capture".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::error!(error = %err, "cannot start capture runtime");
                    return;
                }
            };
            rt.block_on(run(shared, tokens, conn, panic_chord, edges, cmd_rx));
        });

    Arc::new(WaylandCapture { edges: edges_handle, edges_dirty, cmd })
}

#[derive(Clone, Copy)]
struct Zone {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Zone {
    fn display(&self, index: usize) -> DisplayRect {
        DisplayRect {
            id: index.to_string(),
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
            scale: 1.0,
        }
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        let (x0, y0) = (self.x as f64, self.y as f64);
        x >= x0 && x < x0 + self.w as f64 && y >= y0 && y < y0 + self.h as f64
    }

    fn clamp_point(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x.clamp(self.x as f64, (self.x + self.w as i32 - 1) as f64),
            y.clamp(self.y as f64, (self.y + self.h as i32 - 1) as f64),
        )
    }
}

/// Portal Release wants a position inside the zones; compositors ignore out-of-zone
/// suggestions, so clamp to the nearest zone point.
fn clamp_to_zones(zones: &[Zone], x: f64, y: f64) -> (f64, f64) {
    if zones.iter().any(|z| z.contains(x, y)) {
        return (x, y);
    }
    let mut best: Option<(f64, f64, f64)> = None;
    for z in zones {
        let (cx, cy) = z.clamp_point(x, y);
        let d = (cx - x).powi(2) + (cy - y).powi(2);
        if best.is_none_or(|(bd, _, _)| d < bd) {
            best = Some((d, cx, cy));
        }
    }
    best.map(|(_, cx, cy)| (cx, cy)).unwrap_or((x, y))
}

/// EdgeSpec → portal barrier geometry. `at` is the boundary coordinate (origin for
/// left/top edges, origin+extent for right/bottom) and is used verbatim; `from..to` is a
/// half-open span, so the inclusive barrier end coordinate is `to - 1`.
fn edge_barrier(edge: &EdgeSpec) -> Option<(i32, i32, i32, i32)> {
    if edge.to <= edge.from {
        return None;
    }
    let (lo, hi) = (edge.from, edge.to - 1);
    Some(match edge.side {
        EdgeSide::Left | EdgeSide::Right => (edge.at, lo, edge.at, hi),
        EdgeSide::Top | EdgeSide::Bottom => (lo, edge.at, hi, edge.at),
    })
}

/// KDE sometimes reports barrier_id 0 on Activated: fall back to the armed edge with the
/// smallest point-to-segment distance from the reported cursor position.
fn nearest_edge(edges: &[EdgeSpec], x: f64, y: f64) -> Option<EdgeSpec> {
    let dist = |e: &EdgeSpec| {
        let (cross, along) = match e.side {
            EdgeSide::Left | EdgeSide::Right => (x, y),
            EdgeSide::Top | EdgeSide::Bottom => (y, x),
        };
        let hi = (e.to - 1).max(e.from) as f64;
        let clamped = along.clamp(e.from as f64, hi);
        (cross - e.at as f64).abs() + (along - clamped).abs()
    };
    edges.iter().min_by(|a, b| dist(a).total_cmp(&dist(b))).cloned()
}

/// An ongoing capture activation. `forwarding` is cleared the moment we Release; the
/// entry stays so the matching Deactivated signal can be told apart from a compositor-
/// initiated deactivation (which is a session-level failure).
struct ActiveCapture {
    activation_id: u32,
    forwarding: bool,
    released: bool,
}

struct Session {
    ic: zbus::Proxy<'static>,
    session_path: String,
    session_opath: zbus::zvariant::ObjectPath<'static>,
    connection: reis::event::Connection,
    ei_stream: reis::tokio::EiConvertEventStream,
    zones: Vec<Zone>,
    zone_set: u32,
}

async fn establish(
    conn: &zbus::Connection,
    tokens: &TokenStore,
    edges: &[EdgeSpec],
) -> Result<Session> {
    let ic = portal::proxy(conn, IFACE).await?;
    let version = portal::version(&ic).await;
    if version < 2 {
        return Err(PlatformError::Unavailable(format!(
            "InputCapture portal v2 required (CreateSession2), found v{version}"
        )));
    }

    let created = portal::request(conn, &ic, "CreateSession2", |token| {
        let mut opts = Options::new();
        opts.insert("handle_token", Value::new(token.to_owned()));
        opts.insert("session_handle_token", Value::new(token.to_owned()));
        (opts,)
    })
    .await?;
    let session_path = portal::get::<OwnedObjectPath>(&created, "session_handle")
        .ok_or_else(|| PlatformError::Unavailable("CreateSession2 returned no session_handle".into()))?
        .to_string();
    let session_opath = portal::object_path(&session_path)?;

    let restore_token = tokens.get(TokenKind::InputCapture);
    let started = portal::request(conn, &ic, "Start", |token| {
        let mut opts = Options::new();
        opts.insert("handle_token", Value::new(token.to_owned()));
        opts.insert("capabilities", Value::new(CAP_KEYBOARD | CAP_POINTER));
        opts.insert("persist_mode", Value::new(PERSIST));
        if let Some(restore) = restore_token {
            opts.insert("restore_token", Value::new(restore));
        }
        (session_opath.clone(), "", opts)
    })
    .await?;
    if let Some(token) = portal::get::<String>(&started, "restore_token") {
        tokens.set(TokenKind::InputCapture, token);
    }
    let capabilities = portal::get::<u32>(&started, "capabilities").unwrap_or(0);
    if capabilities & (CAP_KEYBOARD | CAP_POINTER) != CAP_KEYBOARD | CAP_POINTER {
        return Err(PlatformError::Permission(format!(
            "InputCapture granted capabilities {capabilities:#x}, need keyboard+pointer"
        )));
    }

    let eis_fd: OwnedFd = ic
        .call("ConnectToEIS", &(session_opath.clone(), Options::new()))
        .await
        .map_err(portal::err_ctx("ConnectToEIS"))?;
    let stream = std::os::unix::net::UnixStream::from(std::os::fd::OwnedFd::from(eis_fd));
    let context = ei::Context::new(stream)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("ei context: {e}")))?;
    let (connection, ei_stream) = context
        .handshake_tokio("splice", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("ei handshake: {e}")))?;

    let zones_result = portal::request(conn, &ic, "GetZones", |token| {
        let mut opts = Options::new();
        opts.insert("handle_token", Value::new(token.to_owned()));
        (session_opath.clone(), opts)
    })
    .await?;
    let zones: Vec<Zone> = portal::get::<Vec<(u32, u32, i32, i32)>>(&zones_result, "zones")
        .unwrap_or_default()
        .iter()
        .map(|&(w, h, x, y)| Zone { x, y, w, h })
        .collect();
    let zone_set = portal::get::<u32>(&zones_result, "zone_set").unwrap_or(0);

    let mut session = Session {
        ic,
        session_path,
        session_opath,
        connection,
        ei_stream,
        zones,
        zone_set,
    };
    apply_barriers(conn, &mut session, edges).await?;
    Ok(session)
}

/// Sets barriers and re-arms capture on the SAME session (SetPointerBarriers suspends
/// the session; Enable re-arms). Barrier ids are 1-based indexes into `edges`.
async fn apply_barriers(
    conn: &zbus::Connection,
    session: &mut Session,
    edges: &[EdgeSpec],
) -> Result<()> {
    let mut barriers: HashMap<u32, (i32, i32, i32, i32)> = HashMap::new();
    for (i, edge) in edges.iter().enumerate() {
        if let Some(pos) = edge_barrier(edge) {
            barriers.insert(i as u32 + 1, pos);
        }
    }
    let session_opath = session.session_opath.clone();
    let zone_set = session.zone_set;
    let response =
        portal::request(conn, &session.ic, "SetPointerBarriers", |token| {
            let mut opts = Options::new();
            opts.insert("handle_token", Value::new(token.to_owned()));
            (session_opath, opts, barriers, zone_set)
        })
        .await?;
    let failed = portal::get::<Vec<u32>>(&response, "failed_barriers").unwrap_or_default();
    if !failed.is_empty() {
        tracing::warn!(failed = ?failed, "pointer barriers denied by compositor");
    }
    session
        .ic
        .call::<_, _, ()>("Enable", &(session.session_opath.clone(), Options::new()))
        .await
        .map_err(portal::err_ctx("Enable"))?;
    Ok(())
}

/// Portal Release hands the cursor back locally; the capture entry is kept (with
/// forwarding off and `released` set) until the matching Deactivated arrives.
async fn release(session: &Session, capture: &mut Option<ActiveCapture>, warp_to: Option<Vec2>) {
    let Some(active) = capture.as_mut() else { return };
    if active.released {
        return;
    }
    active.forwarding = false;
    active.released = true;
    let mut opts = Options::new();
    opts.insert("activation_id", Value::new(active.activation_id));
    if let Some(pos) = warp_to {
        let (x, y) = clamp_to_zones(&session.zones, pos.x, pos.y);
        opts.insert("cursor_position", Value::new((x, y)));
    }
    if let Err(err) = session
        .ic
        .call::<_, _, ()>("Release", &(session.session_opath.clone(), opts))
        .await
    {
        tracing::warn!(error = %err, "portal Release failed");
    }
}

async fn run(
    shared: Arc<WaylandShared>,
    tokens: Arc<TokenStore>,
    conn: zbus::Connection,
    panic_chord: Vec<u32>,
    edges: Arc<RwLock<Vec<EdgeSpec>>>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    // First attempt is immediate; subsequent ones are rate-limited.
    let mut last_recreate = Instant::now() - RECREATE_MIN_INTERVAL;
    loop {
        let since = last_recreate.elapsed();
        if since < RECREATE_MIN_INTERVAL {
            tokio::time::sleep((RECREATE_MIN_INTERVAL - since).max(RECREATE_MIN_BACKOFF)).await;
        }
        last_recreate = Instant::now();

        let current_edges = edges.read().clone();
        match establish(&conn, &tokens, &current_edges).await {
            Ok(session) => {
                shared.set_health(|h| h.capture = None);
                let displays: Vec<DisplayRect> =
                    session.zones.iter().enumerate().map(|(i, z)| z.display(i)).collect();
                shared.emit(PlatformEvent::DisplaysChanged { displays });
                run_session(&shared, &conn, session, &panic_chord, &edges, &mut cmd_rx).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "input capture session setup failed");
                shared.set_health(|h| h.capture = Some(format!("{err}")));
            }
        }
        shared.emit(PlatformEvent::Capture(CaptureEvent::Broken {
            reason: "capture session ended".into(),
        }));
    }
}

async fn run_session(
    shared: &Arc<WaylandShared>,
    conn: &zbus::Connection,
    mut session: Session,
    panic_chord: &[u32],
    edges: &Arc<RwLock<Vec<EdgeSpec>>>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
) {
    let session_proxy = match portal::session_proxy(conn, &session.session_path).await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "cannot watch capture session");
            return;
        }
    };
    let mut closed = match session_proxy.receive_signal("Closed").await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "cannot subscribe to Session.Closed");
            return;
        }
    };
    let (mut activated, mut deactivated, mut zones_changed) = match futures::future::try_join3(
        session.ic.receive_signal("Activated"),
        session.ic.receive_signal("Deactivated"),
        session.ic.receive_signal("ZonesChanged"),
    )
    .await
    {
        Ok(streams) => streams,
        Err(err) => {
            tracing::warn!(error = %err, "cannot subscribe to InputCapture signals");
            return;
        }
    };

    let mut capture: Option<ActiveCapture> = None;
    let mut pressed: std::collections::HashSet<u32> = std::collections::HashSet::new();

    loop {
        tokio::select! {
            msg = activated.next() => {
                let Some(msg) = msg else { return };
                let Some((path, opts)) = portal::session_signal(&msg) else { continue };
                if path != session.session_path {
                    continue;
                }
                let edge_list = edges.read().clone();
                // cursor_position usually overshoots past the barrier; only the
                // along-axis coordinate is used, clamped to the armed span.
                let pos = portal::get::<(f64, f64)>(&opts, "cursor_position").unwrap_or((0.0, 0.0));
                let edge = match portal::get::<u32>(&opts, "barrier_id").filter(|id| *id != 0) {
                    Some(id) => edge_list.get(id as usize - 1).cloned(),
                    None => nearest_edge(&edge_list, pos.0, pos.1),
                };
                let Some(edge) = edge else {
                    tracing::warn!("activated with no matching barrier");
                    continue;
                };
                let along = match edge.side {
                    EdgeSide::Left | EdgeSide::Right => pos.1,
                    EdgeSide::Top | EdgeSide::Bottom => pos.0,
                }
                .clamp(edge.from as f64, (edge.to - 1).max(edge.from) as f64);
                capture = Some(ActiveCapture {
                    activation_id: portal::get::<u32>(&opts, "activation_id").unwrap_or(0),
                    forwarding: true,
                    released: false,
                });
                pressed.clear();
                shared.emit(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: edge.id, along }));
            }
            msg = deactivated.next() => {
                let Some(msg) = msg else { return };
                let Some((path, opts)) = portal::session_signal(&msg) else { continue };
                if path != session.session_path {
                    continue;
                }
                let activation_id = portal::get::<u32>(&opts, "activation_id");
                let was_released = capture
                    .as_ref()
                    .is_some_and(|c| c.released && Some(c.activation_id) == activation_id);
                capture = None;
                pressed.clear();
                if !was_released {
                    shared.emit(PlatformEvent::Capture(CaptureEvent::Broken {
                        reason: "capture deactivated by compositor".into(),
                    }));
                    return;
                }
            }
            msg = zones_changed.next() => {
                if msg.is_none() {
                    return;
                }
                let session_opath = session.session_opath.clone();
                let zones_result =
                    portal::request(conn, &session.ic, "GetZones", |token| {
                        let mut opts = Options::new();
                        opts.insert("handle_token", Value::new(token.to_owned()));
                        (session_opath, opts)
                    })
                    .await;
                match zones_result {
                    Ok(result) => {
                        session.zone_set = portal::get::<u32>(&result, "zone_set").unwrap_or(0);
                        session.zones = portal::get::<Vec<(u32, u32, i32, i32)>>(&result, "zones")
                            .unwrap_or_default()
                            .iter()
                            .map(|&(w, h, x, y)| Zone { x, y, w, h })
                            .collect();
                        let displays: Vec<DisplayRect> =
                            session.zones.iter().enumerate().map(|(i, z)| z.display(i)).collect();
                        shared.emit(PlatformEvent::DisplaysChanged { displays });
                        let current = edges.read().clone();
                        if let Err(err) = apply_barriers(conn, &mut session, &current).await {
                            tracing::warn!(error = %err, "re-applying barriers after zone change failed");
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "GetZones after ZonesChanged failed");
                        return;
                    }
                }
            }
            _ = closed.next() => {
                tracing::warn!("capture session closed by portal");
                return;
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    Command::ApplyEdges(new_edges) => {
                        *edges.write() = new_edges.clone();
                        if let Err(err) = apply_barriers(conn, &mut session, &new_edges).await {
                            tracing::warn!(error = %err, "SetPointerBarriers failed");
                            return;
                        }
                    }
                    Command::EndCapture { warp_to } => {
                        release(&session, &mut capture, warp_to).await;
                        pressed.clear();
                    }
                }
            }
            event = session.ei_stream.next() => {
                match event {
                    None => {
                        tracing::warn!("ei stream ended");
                        return;
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "ei stream error");
                        return;
                    }
                    Some(Ok(event)) => {
                        if handle_ei_event(shared, &session, event, &mut capture, &mut pressed, panic_chord).await {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Returns true when the session must be torn down and re-established.
async fn handle_ei_event(
    shared: &Arc<WaylandShared>,
    session: &Session,
    event: EiEvent,
    capture: &mut Option<ActiveCapture>,
    pressed: &mut std::collections::HashSet<u32>,
    panic_chord: &[u32],
) -> bool {
    let forwarding = capture.as_ref().is_some_and(|c| c.forwarding);
    match event {
        EiEvent::SeatAdded(added) => {
            added.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll,
            );
            if let Err(err) = session.connection.flush() {
                tracing::warn!(error = %err, "ei flush failed");
                return true;
            }
        }
        EiEvent::DeviceStartEmulating(start) => {
            // The EIS start_emulating sequence equals the Activated activation_id; the
            // D-Bus and EIS streams are independently ordered, this is the correlation.
            match capture.as_ref() {
                Some(active) if active.activation_id == start.sequence => {}
                _ => tracing::debug!(sequence = start.sequence, "start_emulating for unknown activation"),
            }
        }
        EiEvent::DevicePaused(_) => {
            // Protocol-defined reset: all keys are lifted; capture cannot continue.
            shared.emit(PlatformEvent::Capture(CaptureEvent::Broken {
                reason: "ei device paused".into(),
            }));
            return true;
        }
        EiEvent::Disconnected(disconnected) => {
            tracing::warn!(reason = ?disconnected.reason, "ei disconnected");
            return true;
        }
        EiEvent::KeyboardKey(key) => {
            let is_press = key.state == KeyState::Press;
            if is_press {
                pressed.insert(key.key);
            } else {
                pressed.remove(&key.key);
            }
            if !panic_chord.is_empty()
                && panic_chord.iter().all(|code| pressed.contains(code))
                && capture.as_ref().is_some_and(|c| c.forwarding)
            {
                tracing::warn!("panic chord pressed");
                release(session, capture, None).await;
                pressed.clear();
                shared.emit(PlatformEvent::Capture(CaptureEvent::Panic));
                return false;
            }
            if forwarding {
                shared.emit(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Key {
                    code: key.key,
                    pressed: is_press,
                })));
            }
        }
        EiEvent::PointerMotion(motion) => {
            if forwarding {
                shared.emit(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Motion {
                    dx: motion.dx as f64,
                    dy: motion.dy as f64,
                })));
            }
        }
        EiEvent::Button(button) => {
            if forwarding {
                let pressed = button.state == ButtonState::Press;
                // Other(n) carries the evdev offset from BTN_LEFT, the same numbering
                // macOS uses for Other; the value round-trips on both backends.
                let mapped = match button.button {
                    0x110 => Some(PointerButton::Left),
                    0x111 => Some(PointerButton::Right),
                    0x112 => Some(PointerButton::Middle),
                    0x113 => Some(PointerButton::Back),
                    0x114 => Some(PointerButton::Forward),
                    code @ 0x110..=0x20f => Some(PointerButton::Other((code - BTN_LEFT) as u8)),
                    _ => None,
                };
                if let Some(button) = mapped {
                    shared.emit(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Button {
                        button,
                        pressed,
                    })));
                }
            }
        }
        EiEvent::ScrollDelta(scroll) => {
            if forwarding {
                shared.emit(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::ScrollPixels {
                    dx: scroll.dx as f64,
                    dy: scroll.dy as f64,
                })));
            }
        }
        EiEvent::ScrollDiscrete(scroll) => {
            if forwarding {
                shared.emit(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Scroll120 {
                    dx: scroll.discrete_dx,
                    dy: scroll.discrete_dy,
                })));
            }
        }
        EiEvent::ScrollStop(_) => {
            if forwarding {
                shared.emit(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::ScrollStop {
                    cancel: false,
                })));
            }
        }
        EiEvent::ScrollCancel(_) => {
            if forwarding {
                shared.emit(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::ScrollStop {
                    cancel: true,
                })));
            }
        }
        _ => {}
    }
    false
}
