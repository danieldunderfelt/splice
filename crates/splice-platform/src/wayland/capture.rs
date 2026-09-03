//! Capture side (this machine as source): InputCapture portal session + reis receiver.
//!
//! Load-bearing rules from docs/research/wayland-input.md:
//! - NEVER call `Disable()` (mutter bug #3908). GNOME also rejects barrier changes on an
//!   enabled session, and GNOME 50's backend (InputCapture v1) shows the consent dialog
//!   on every CreateSession. The session is therefore armed exactly once, with barriers
//!   on the whole outer boundary of the zone union, and never touched again for peer or
//!   layout changes: `set_edges` only updates the in-memory map that `Activated` is
//!   resolved against, and a hit on a boundary with no edge is released immediately.
//!   Only ZonesChanged (display hotplug) recreates the session.
//! - Restore tokens are single-use; the replacement is persisted on every Start.
//! - Capture activates only when the cursor hits a barrier; there is no way to force it,
//!   so `begin_capture` is a no-op and forwarding starts at `Activated`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use parking_lot::RwLock;
use reis::ei;
use reis::ei::button::ButtonState;
use reis::ei::keyboard::KeyState;
use reis::event::{DeviceCapability, EiEvent};
use splice_proto::{InputEvent, PointerButton, Vec2};
use tokio::sync::{mpsc, Notify};
use zbus::zvariant::{OwnedFd, Value};

use super::portal::{self, Options};
use super::tokens::{TokenKind, TokenStore};
use super::WaylandShared;
use crate::{Capture, CaptureEvent, EdgeSide, EdgeSpec, PlatformError, PlatformEvent, Result};

const IFACE: &str = "org.freedesktop.portal.InputCapture";
const CAP_KEYBOARD: u32 = 1;
const CAP_POINTER: u32 = 2;
/// persist_mode: persist until explicitly revoked.
const PERSIST: u32 = 2;
/// Session re-establishment: at least 1 s of backoff, at most one recreation per 5 s
/// (lan-mouse's 34-reconnects-per-2h fd leak is the failure mode this prevents).
const RECREATE_MIN_BACKOFF: Duration = Duration::from_secs(1);
const RECREATE_MIN_INTERVAL: Duration = Duration::from_secs(5);

const BTN_LEFT: u32 = 0x110;

enum Command {
    EndCapture { warp_to: Option<Vec2> },
    Panic,
}

/// A locally-owned emergency release path used by the physical evdev monitor.
/// It talks directly to the portal pump and does not depend on the engine/network.
#[derive(Clone)]
pub struct PanicRelease {
    cmd: mpsc::UnboundedSender<Command>,
    active: Arc<AtomicBool>,
}

impl PanicRelease {
    pub fn trigger(&self) {
        if self.active.load(Ordering::Acquire) {
            let _ = self.cmd.send(Command::Panic);
        }
    }
}

/// The engine's edge map, consulted on every `Activated`; `changed` wakes the session
/// bootstrap once the first edge appears.
#[derive(Default)]
struct EdgeMap {
    edges: RwLock<Vec<EdgeSpec>>,
    changed: Notify,
}

pub struct WaylandCapture {
    map: Arc<EdgeMap>,
    cmd: mpsc::UnboundedSender<Command>,
}

#[async_trait::async_trait]
impl Capture for WaylandCapture {
    async fn set_edges(&self, edges: Vec<EdgeSpec>) -> Result<()> {
        let mut current = self.map.edges.write();
        if *current == edges {
            return Ok(());
        }
        tracing::info!(edges = ?edges, "capture edge map changed");
        *current = edges;
        drop(current);
        self.map.changed.notify_one();
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
) -> (Arc<WaylandCapture>, PanicRelease) {
    let map = Arc::new(EdgeMap::default());
    let (cmd, cmd_rx) = mpsc::unbounded_channel();
    let active = Arc::new(AtomicBool::new(false));

    // reis's EiConvertEventStream is not Send (its converter holds boxed FnOnce
    // callbacks), so the portal/reis pump cannot be tokio::spawn'd. It runs on a
    // dedicated current-thread runtime where block_on needs no Send bound.
    let map_for_thread = map.clone();
    let active_for_thread = active.clone();
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
            rt.block_on(run(
                shared,
                tokens,
                conn,
                panic_chord,
                map_for_thread,
                active_for_thread,
                cmd_rx,
            ));
        });

    (
        Arc::new(WaylandCapture { map, cmd: cmd.clone() }),
        PanicRelease { cmd, active },
    )
}

#[derive(Clone, Copy, Debug)]
struct Zone {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Zone {
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

/// One armed pointer barrier: an outer boundary segment of the zone union. Portal
/// barrier ids are 1-based indexes into the session's barrier list. `from..to` is a
/// half-open span along the edge; `at` is the boundary coordinate on the crossing axis.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Barrier {
    side: EdgeSide,
    at: i32,
    from: i32,
    to: i32,
}

impl Barrier {
    /// Portal geometry: inclusive pixel endpoints, `at` used verbatim (origin for
    /// left/top, origin+extent for right/bottom).
    fn position(&self) -> (i32, i32, i32, i32) {
        let (lo, hi) = (self.from, self.to - 1);
        match self.side {
            EdgeSide::Left | EdgeSide::Right => (self.at, lo, self.at, hi),
            EdgeSide::Top | EdgeSide::Bottom => (lo, self.at, hi, self.at),
        }
    }

    fn along(&self, x: f64, y: f64) -> f64 {
        match self.side {
            EdgeSide::Left | EdgeSide::Right => y,
            EdgeSide::Top | EdgeSide::Bottom => x,
        }
    }

    fn distance(&self, x: f64, y: f64) -> f64 {
        let cross = match self.side {
            EdgeSide::Left | EdgeSide::Right => x,
            EdgeSide::Top | EdgeSide::Bottom => y,
        };
        let along = self.along(x, y);
        let clamped = along.clamp(self.from as f64, (self.to - 1).max(self.from) as f64);
        (cross - self.at as f64).abs() + (along - clamped).abs()
    }
}

/// The outer boundary of the zone union, split wherever another zone is flush against a
/// side. Compositors deny interior barriers, so these segments are exactly the set that
/// can ever be armed; arming all of them up front means peer changes never touch the
/// portal session.
fn outer_barriers(zones: &[Zone]) -> Vec<Barrier> {
    let mut barriers = Vec::new();
    for (i, z) in zones.iter().enumerate() {
        let (x0, y0, x1, y1) = (z.x, z.y, z.x + z.w as i32, z.y + z.h as i32);
        let sides = [
            (EdgeSide::Left, x0, y0, y1),
            (EdgeSide::Right, x1, y0, y1),
            (EdgeSide::Top, y0, x0, x1),
            (EdgeSide::Bottom, y1, x0, x1),
        ];
        for (side, at, from, to) in sides {
            let mut spans = vec![(from, to)];
            for o in zones.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, o)| o) {
                let (ox0, oy0, ox1, oy1) = (o.x, o.y, o.x + o.w as i32, o.y + o.h as i32);
                let (flush, lo, hi) = match side {
                    EdgeSide::Left => (ox1 == at, oy0, oy1),
                    EdgeSide::Right => (ox0 == at, oy0, oy1),
                    EdgeSide::Top => (oy1 == at, ox0, ox1),
                    EdgeSide::Bottom => (oy0 == at, ox0, ox1),
                };
                if flush {
                    spans = spans
                        .into_iter()
                        .flat_map(|span| subtract_span(span, (lo, hi)))
                        .collect();
                }
            }
            barriers.extend(
                spans
                    .into_iter()
                    .filter(|(a, b)| b > a)
                    .map(|(from, to)| Barrier { side, at, from, to }),
            );
        }
    }
    barriers
}

fn subtract_span((a, b): (i32, i32), (lo, hi): (i32, i32)) -> Vec<(i32, i32)> {
    if hi <= a || lo >= b {
        return vec![(a, b)];
    }
    let mut rest = Vec::new();
    if lo > a {
        rest.push((a, lo));
    }
    if hi < b {
        rest.push((hi, b));
    }
    rest
}

/// KDE sometimes reports barrier_id 0 on Activated: fall back to the armed barrier with
/// the smallest point-to-segment distance from the reported cursor position.
fn nearest_barrier(barriers: &[Barrier], x: f64, y: f64) -> Option<&Barrier> {
    barriers
        .iter()
        .min_by(|a, b| a.distance(x, y).total_cmp(&b.distance(x, y)))
}

/// The engine edge the cursor crossed: same side as the barrier, spanning the along-axis
/// position, closest boundary coordinate. None when nothing is mapped there.
fn edge_for(edges: &[EdgeSpec], barrier: &Barrier, along: f64) -> Option<EdgeSpec> {
    edges
        .iter()
        .filter(|e| e.side == barrier.side && along >= e.from as f64 && along < e.to as f64)
        .min_by_key(|e| (e.at - barrier.at).abs())
        .cloned()
}

struct ActiveCapture {
    activation_id: u32,
}

struct Session {
    ic: zbus::Proxy<'static>,
    session_path: String,
    session_opath: zbus::zvariant::ObjectPath<'static>,
    connection: reis::event::Connection,
    ei_stream: reis::tokio::EiConvertEventStream,
    zones: Vec<Zone>,
    zone_set: u32,
    barriers: Vec<Barrier>,
}

async fn establish(conn: &zbus::Connection, tokens: &TokenStore) -> Result<Session> {
    let ic = portal::proxy(conn, IFACE).await?;
    let version = portal::version(&ic).await;
    let (created, already_started) = if version >= 2 {
        // CreateSession2 returns its results directly; it does not emit Request::Response.
        let mut opts = Options::new();
        opts.insert("session_handle_token", Value::new(portal::next_token()));
        let results: portal::Results = ic
            .call("CreateSession2", &(opts,))
            .await
            .map_err(portal::err_ctx("CreateSession2"))?;
        (results, false)
    } else {
        // Version 1 creates and starts the session in one request. Start is v2-only.
        let results = portal::request(conn, &ic, "CreateSession", |token| {
            let mut opts = Options::new();
            opts.insert("handle_token", Value::new(token.to_owned()));
            opts.insert("session_handle_token", Value::new(token.to_owned()));
            opts.insert("capabilities", Value::new(CAP_KEYBOARD | CAP_POINTER));
            ("", opts)
        })
        .await?;
        (results, true)
    };
    let session_path = portal::session_handle(&created)
        .ok_or_else(|| PlatformError::Unavailable("CreateSession returned no valid session_handle".into()))?;
    let session_opath = portal::object_path(&session_path)?;

    let granted = if already_started {
        created
    } else {
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
        started
    };
    let capabilities = portal::get::<u32>(&granted, "capabilities").unwrap_or(0);
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
    tracing::info!(zone_set, zones = ?zones, "input capture zones discovered");

    let mut session = Session {
        ic,
        session_path,
        session_opath,
        connection,
        ei_stream,
        zones,
        zone_set,
        barriers: Vec::new(),
    };
    arm_barriers(conn, &mut session).await?;
    Ok(session)
}

/// Arms the whole outer boundary of the zone union and enables capture. Barriers the
/// compositor denies are dropped from the id map; at least one must survive.
async fn arm_barriers(conn: &zbus::Connection, session: &mut Session) -> Result<()> {
    let candidates = outer_barriers(&session.zones);
    if candidates.is_empty() {
        return Err(PlatformError::Unavailable("input capture zones have no outer edges".into()));
    }
    let barriers: Vec<Options> = candidates
        .iter()
        .enumerate()
        .map(|(i, barrier)| {
            let mut opts = Options::new();
            opts.insert("barrier_id", Value::new(i as u32 + 1));
            opts.insert("position", Value::new(barrier.position()));
            opts
        })
        .collect();
    tracing::debug!(zone_set = session.zone_set, barriers = ?candidates, "arming pointer barriers");
    let session_opath = session.session_opath.clone();
    let zone_set = session.zone_set;
    let response = portal::request(conn, &session.ic, "SetPointerBarriers", |token| {
        let mut opts = Options::new();
        opts.insert("handle_token", Value::new(token.to_owned()));
        (session_opath, opts, barriers, zone_set)
    })
    .await?;
    let failed = portal::get::<Vec<u32>>(&response, "failed_barriers").unwrap_or_default();
    if !failed.is_empty() {
        tracing::warn!(failed = ?failed, "pointer barriers denied by compositor");
    }
    if failed.len() >= candidates.len() {
        return Err(PlatformError::Unavailable(format!(
            "compositor rejected every pointer barrier: {failed:?}"
        )));
    }
    session
        .ic
        .call::<_, _, ()>("Enable", &(session.session_opath.clone(), Options::new()))
        .await
        .map_err(portal::err_ctx("Enable"))?;
    session.barriers = candidates;
    Ok(())
}

async fn release(session: &Session, capture: &mut Option<ActiveCapture>, warp_to: Option<Vec2>) {
    let Some(active) = capture.take() else { return };
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
    map: Arc<EdgeMap>,
    active: Arc<AtomicBool>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) {
    // First attempt is immediate; subsequent ones are rate-limited.
    let mut last_recreate = Instant::now() - RECREATE_MIN_INTERVAL;
    loop {
        // Do not create an InputCapture session until a real adjacent machine produces
        // an edge: on GNOME v1, CreateSession itself presents consent. Once the session
        // exists it stays up regardless of how the edge map changes afterwards.
        while map.edges.read().is_empty() {
            tokio::select! {
                _ = map.changed.notified() => {}
                cmd = cmd_rx.recv() => {
                    if cmd.is_none() {
                        return;
                    }
                }
            }
        }
        let since = last_recreate.elapsed();
        if since < RECREATE_MIN_INTERVAL {
            tokio::time::sleep((RECREATE_MIN_INTERVAL - since).max(RECREATE_MIN_BACKOFF)).await;
        }
        last_recreate = Instant::now();

        match establish(&conn, &tokens).await {
            Ok(session) => {
                shared.set_health(|h| h.capture = None);
                let end = run_session(
                    &shared,
                    &conn,
                    session,
                    &panic_chord,
                    &map,
                    active.clone(),
                    &mut cmd_rx,
                )
                .await;
                if matches!(end, SessionEnd::Reconfigure) {
                    continue;
                }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionEnd {
    Reconfigure,
    Broken,
}

async fn close_session(session_proxy: &zbus::Proxy<'_>) {
    if let Err(err) = session_proxy.call::<_, _, ()>("Close", &()).await {
        tracing::warn!(error = %err, "cannot close capture session for reconfiguration");
    }
}

async fn run_session(
    shared: &Arc<WaylandShared>,
    conn: &zbus::Connection,
    mut session: Session,
    panic_chord: &[u32],
    map: &EdgeMap,
    active_flag: Arc<AtomicBool>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
) -> SessionEnd {
    struct ClearActive(Arc<AtomicBool>);
    impl Drop for ClearActive {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let _clear_active = ClearActive(active_flag.clone());
    let session_proxy = match portal::session_proxy(conn, &session.session_path).await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "cannot watch capture session");
            return SessionEnd::Broken;
        }
    };
    let mut closed = match session_proxy.receive_signal("Closed").await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "cannot subscribe to Session.Closed");
            return SessionEnd::Broken;
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
            return SessionEnd::Broken;
        }
    };

    let mut capture: Option<ActiveCapture> = None;
    let mut pressed: std::collections::HashSet<u32> = std::collections::HashSet::new();

    loop {
        tokio::select! {
            msg = activated.next() => {
                let Some(msg) = msg else { return SessionEnd::Broken };
                let Some((path, opts)) = portal::session_signal(&msg) else { continue };
                if path != session.session_path {
                    continue;
                }
                let activation_id = portal::get::<u32>(&opts, "activation_id").unwrap_or(0);
                // cursor_position usually overshoots past the barrier; only the
                // along-axis coordinate is used, clamped to the edge's span.
                let pos = portal::get::<(f64, f64)>(&opts, "cursor_position").unwrap_or((0.0, 0.0));
                let barrier = match portal::get::<u32>(&opts, "barrier_id").filter(|id| *id != 0) {
                    Some(id) => session.barriers.get(id as usize - 1),
                    None => nearest_barrier(&session.barriers, pos.0, pos.1),
                };
                let edge = barrier.and_then(|b| edge_for(&map.edges.read(), b, b.along(pos.0, pos.1)));
                capture = Some(ActiveCapture {
                    activation_id,
                });
                active_flag.store(true, Ordering::Release);
                let Some(edge) = edge else {
                    // Nothing mapped on this stretch of the boundary: hand the cursor
                    // straight back. This is the price of never re-arming the session.
                    release(&session, &mut capture, None).await;
                    active_flag.store(false, Ordering::Release);
                    continue;
                };
                let along = barrier
                    .map(|b| b.along(pos.0, pos.1))
                    .unwrap_or(0.0)
                    .clamp(edge.from as f64, (edge.to - 1).max(edge.from) as f64);
                pressed.clear();
                shared.emit(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: edge.id, along }));
            }
            msg = deactivated.next() => {
                let Some(msg) = msg else { return SessionEnd::Broken };
                let Some((path, opts)) = portal::session_signal(&msg) else { continue };
                if path != session.session_path {
                    continue;
                }
                let activation_id = portal::get::<u32>(&opts, "activation_id");
                let was_active = capture
                    .as_ref()
                    .is_some_and(|c| activation_id.is_none_or(|id| c.activation_id == id));
                if was_active {
                    active_flag.store(false, Ordering::Release);
                    pressed.clear();
                    shared.emit(PlatformEvent::Capture(CaptureEvent::Broken {
                        reason: "capture deactivated by compositor".into(),
                    }));
                    return SessionEnd::Broken;
                }
            }
            msg = zones_changed.next() => {
                if msg.is_none() {
                    return SessionEnd::Broken;
                }
                // GNOME requires a disabled session to replace barriers. Recreate it
                // instead of using Disable, whose re-enable path is broken on affected
                // Mutter versions. This is the one remaining consent prompt after launch.
                close_session(&session_proxy).await;
                return SessionEnd::Reconfigure;
            }
            _ = closed.next() => {
                tracing::warn!("capture session closed by portal");
                return SessionEnd::Broken;
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return SessionEnd::Broken };
                match cmd {
                    Command::EndCapture { warp_to } => {
                        release(&session, &mut capture, warp_to).await;
                        active_flag.store(false, Ordering::Release);
                        pressed.clear();
                    }
                    Command::Panic => {
                        if capture.is_some() {
                            tracing::warn!("panic chord pressed");
                            release(&session, &mut capture, None).await;
                            active_flag.store(false, Ordering::Release);
                            pressed.clear();
                            shared.emit(PlatformEvent::Capture(CaptureEvent::Panic));
                        }
                    }
                }
            }
            event = session.ei_stream.next() => {
                match event {
                    None => {
                        tracing::warn!("ei stream ended");
                        return SessionEnd::Broken;
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "ei stream error");
                        return SessionEnd::Broken;
                    }
                    Some(Ok(event)) => {
                        if handle_ei_event(shared, &session, event, &mut capture, &mut pressed, panic_chord).await {
                            return SessionEnd::Broken;
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
    let forwarding = capture.is_some();
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
                && capture.is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(x: i32, y: i32, w: u32, h: u32) -> Zone {
        Zone { x, y, w, h }
    }

    fn barrier(side: EdgeSide, at: i32, from: i32, to: i32) -> Barrier {
        Barrier { side, at, from, to }
    }

    #[test]
    fn single_zone_arms_all_four_outer_edges_with_inclusive_endpoints() {
        let barriers = outer_barriers(&[zone(0, 0, 1920, 1080)]);
        let positions: Vec<_> = barriers.iter().map(Barrier::position).collect();
        assert_eq!(
            positions,
            vec![(0, 0, 0, 1079), (1920, 0, 1920, 1079), (0, 0, 1919, 0), (0, 1080, 1919, 1080)]
        );
    }

    #[test]
    fn flush_neighbours_remove_interior_barriers() {
        let barriers = outer_barriers(&[zone(0, 0, 1920, 1080), zone(1920, 0, 1920, 1080)]);
        assert!(!barriers.iter().any(|b| b.side == EdgeSide::Right && b.at == 1920));
        assert!(!barriers.iter().any(|b| b.side == EdgeSide::Left && b.at == 1920));
        assert!(barriers.contains(&barrier(EdgeSide::Right, 3840, 0, 1080)));
        assert_eq!(barriers.len(), 6);
    }

    #[test]
    fn offset_neighbours_leave_partial_outer_segments() {
        let barriers = outer_barriers(&[zone(0, 0, 1920, 1080), zone(1920, 200, 1920, 1080)]);
        assert!(barriers.contains(&barrier(EdgeSide::Right, 1920, 0, 200)));
        assert!(barriers.contains(&barrier(EdgeSide::Left, 1920, 1080, 1280)));
        assert!(barriers.contains(&barrier(EdgeSide::Bottom, 1080, 0, 1920)));
        assert!(barriers.contains(&barrier(EdgeSide::Top, 200, 1920, 3840)));
    }

    #[test]
    fn activation_resolves_to_mapped_edge_only() {
        let edges = vec![EdgeSpec {
            id: 7,
            side: EdgeSide::Left,
            at: 0,
            from: 108,
            to: 1080,
        }];
        let left = barrier(EdgeSide::Left, 0, 0, 1080);
        assert_eq!(edge_for(&edges, &left, 500.0).map(|e| e.id), Some(7));
        assert_eq!(edge_for(&edges, &left, 50.0), None);
        let right = barrier(EdgeSide::Right, 1920, 0, 1080);
        assert_eq!(edge_for(&edges, &right, 500.0), None);
    }

    #[test]
    fn nearest_barrier_falls_back_by_perpendicular_distance() {
        let barriers = outer_barriers(&[zone(0, 0, 1920, 1080)]);
        let hit = nearest_barrier(&barriers, 1935.0, 400.0).unwrap();
        assert_eq!(hit.side, EdgeSide::Right);
    }
}
