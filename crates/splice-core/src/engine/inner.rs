//! Engine task internals: single task owning all mutable state. Driven by platform
//! events, peer session events (net layer), discovery ticks and UI commands; drives
//! capture/emulation and publishes UiState snapshots (debounced to <=10 Hz).

use crate::engine::Command;
use crate::layout::{self, EdgeLink, MachineGeom};
use crate::ledger::HeldLedger;
use crate::net::{self, NetControl, NetOpts, PeerEvent, TsApi};
use crate::ui_state::{UiConnection, UiEdge, UiFocus, UiMachine, UiState};
use crate::config;
use splice_platform::{CaptureEvent, ClipboardOffer, EdgeSide, HealthReport, PlatformEvent};
use splice_proto::{
    caps, DisplayRect, Frame, InputEvent, LayoutDoc, LeaveReason, MachineId, MachineInfo,
    MachinePlacement, Os, Stamp, Vec2, Vec2I, CLIP_CHUNK, CLIP_MAX_TOTAL,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// UiState bursts are coalesced to one publish per this window (DESIGN: <=10 Hz).
const UI_DEBOUNCE: Duration = Duration::from_millis(100);
/// Config writes are debounced this long after the last change.
const CFG_DEBOUNCE: Duration = Duration::from_secs(1);
/// A lazy clipboard pull gives up after this much silence from the origin.
const CLIP_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Snap tolerance for SetPlacement (DESIGN/UI: 8 px magnetism).
const SNAP_TOLERANCE: i32 = 8;
const MAX_PLATFORM_BATCH_EVENTS: usize = 64;

#[derive(Clone, PartialEq)]
enum Focus {
    Local,
    Remote(MachineId),
    Driven(MachineId),
}

#[derive(Default)]
struct Peer {
    info: Option<MachineInfo>,
    caps: Vec<String>,
    connected: bool,
    degraded: bool,
    ts_online: bool,
    /// Tailscale reports a direct path (CurAddr non-empty) vs DERP relay.
    direct: bool,
    rtt_ms: Option<f64>,
}

struct PendingFetch {
    buf: Vec<u8>,
    done: oneshot::Sender<Option<Vec<u8>>>,
}

type PendingFetches = Arc<parking_lot::Mutex<HashMap<(u64, String), PendingFetch>>>;

/// Engine-side ClipFetch handed to clipboard backends: pulls one representation from
/// the offering peer over the peer session, reassembling ClipChunks in the engine.
struct RemoteFetch {
    net: NetControl,
    origin: MachineId,
    id: u64,
    pending: PendingFetches,
}

#[async_trait::async_trait]
impl splice_platform::ClipFetch for RemoteFetch {
    async fn fetch(&self, mime: &str) -> Option<Vec<u8>> {
        let key = (self.id, mime.to_string());
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .insert(key.clone(), PendingFetch { buf: Vec::new(), done: tx });
        if !self
            .net
            .send_to(&self.origin, Frame::ClipRequest { id: self.id, mime: mime.to_string() })
        {
            self.pending.lock().remove(&key);
            return None;
        }
        match tokio::time::timeout(CLIP_FETCH_TIMEOUT, rx).await {
            Ok(Ok(data)) => data,
            _ => {
                self.pending.lock().remove(&key);
                None
            }
        }
    }
}

pub struct Inner {
    self_info: MachineInfo,
    initial_displays: Vec<DisplayRect>,
    capture: Arc<dyn splice_platform::Capture>,
    emulate: Arc<dyn splice_platform::Emulate>,
    clipboard: Arc<dyn splice_platform::Clipboard>,
    platform_events: mpsc::UnboundedReceiver<PlatformEvent>,
    ts: Arc<dyn TsApi>,
    data_dir: PathBuf,
    net_opts: NetOpts,
    poll_interval: Duration,
    cmd: mpsc::UnboundedReceiver<Command>,
    ui_tx: watch::Sender<UiState>,
    ready_tx: watch::Sender<Option<SocketAddr>>,

    net: Option<NetControl>,
    net_events: Option<mpsc::UnboundedReceiver<PeerEvent>>,
    cfg: config::Config,
    peers: HashMap<MachineId, Peer>,
    layout: Option<LayoutDoc>,
    layout_lamport: u64,
    claim: Option<Stamp>,
    claim_lamport: u64,
    focus: Focus,
    session: u64,
    active_session: u64,
    virtual_pos: Vec2,
    active_sensitivity: f64,
    /// Local cursor position when capture began; warp target for orderly teardowns.
    last_local_pos: Vec2,
    source_ledger: HeldLedger,
    target_ledger: HeldLedger,
    /// Links over enabled+reachable machines (arming/crossing decisions).
    links: Vec<EdgeLink>,
    /// Geometric links over every placed machine, independent of temporary
    /// enabled/connectivity state. These keep OS capture barriers stable.
    geo_links: Vec<EdgeLink>,
    /// Outgoing geometric links, index-aligned with the armed EdgeSpec ids.
    armed: Vec<EdgeLink>,
    health: HealthReport,
    tailscale_error: Option<String>,
    ui_deadline: Option<Instant>,
    cfg_deadline: Option<Instant>,
    platform_batch: Vec<PlatformEvent>,

    clip_lamport: u64,
    clip_seen: Option<Stamp>,
    last_applied_inline: Option<String>,
    offer_id: u64,
    live_offer: Option<(u64, Vec<String>)>,
    pending_fetches: PendingFetches,
}

impl Inner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        platform: splice_platform::Platform,
        ts: Arc<dyn TsApi>,
        data_dir: PathBuf,
        net_opts: NetOpts,
        poll_interval: Duration,
        cmd: mpsc::UnboundedReceiver<Command>,
        ui_tx: watch::Sender<UiState>,
        ready_tx: watch::Sender<Option<SocketAddr>>,
    ) -> Self {
        let cfg = config::load(&data_dir);
        let layout_lamport = cfg.layout.as_ref().map(|d| d.stamp.lamport).unwrap_or(0);
        let splice_platform::Platform { capture, emulate, clipboard, displays, events } = platform;
        Inner {
            self_info: MachineInfo {
                id: MachineId(String::new()),
                hostname: String::new(),
                os: Os::Other,
                displays: displays.clone(),
            },
            initial_displays: displays,
            capture,
            emulate,
            clipboard,
            platform_events: events,
            ts,
            data_dir,
            net_opts,
            poll_interval,
            cmd,
            ui_tx,
            ready_tx,
            net: None,
            net_events: None,
            layout: cfg.layout.clone(),
            layout_lamport,
            cfg,
            peers: HashMap::new(),
            claim: None,
            claim_lamport: 0,
            focus: Focus::Local,
            session: 0,
            active_session: 0,
            virtual_pos: Vec2 { x: 0.0, y: 0.0 },
            active_sensitivity: 1.0,
            last_local_pos: Vec2 { x: 0.0, y: 0.0 },
            source_ledger: HeldLedger::default(),
            target_ledger: HeldLedger::default(),
            links: Vec::new(),
            geo_links: Vec::new(),
            armed: Vec::new(),
            health: HealthReport::default(),
            tailscale_error: None,
            ui_deadline: None,
            cfg_deadline: None,
            platform_batch: Vec::with_capacity(16),
            clip_lamport: 0,
            clip_seen: None,
            last_applied_inline: None,
            offer_id: 0,
            live_offer: None,
            pending_fetches: PendingFetches::default(),
        }
    }

    pub async fn run(mut self) {
        self.bootstrap().await;
        self.ensure_doc();
        self.discover().await;
        self.recompute().await;
        self.publish_ui();

        let mut net_events = self.net_events.take();
        let mut discovery = tokio::time::interval_at(
            tokio::time::Instant::now() + self.poll_interval,
            self.poll_interval,
        );
        let mut platform_open = true;
        loop {
            let ui_at = self.ui_deadline;
            let cfg_at = self.cfg_deadline;
            tokio::select! {
                cmd = self.cmd.recv() => match cmd {
                    Some(cmd) => {
                        self.on_command(cmd).await;
                        self.recompute().await;
                    }
                    // Handle dropped: shut sessions down gracefully, then exit.
                    None => break,
                },
                ev = async {
                    if platform_open {
                        self.platform_events.recv().await
                    } else {
                        std::future::pending().await
                    }
                } => match ev {
                    Some(first) => {
                        if self.on_platform_batch(first).await {
                            self.recompute().await;
                        }
                    }
                    None => platform_open = false,
                },
                ev = async {
                    match &mut net_events {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match ev {
                    Some(ev) => {
                        let recompute = matches!(
                            &ev,
                            PeerEvent::Connected { .. }
                                | PeerEvent::Degraded(_)
                                | PeerEvent::Healthy(_, _)
                                | PeerEvent::Disconnected(_, _)
                                | PeerEvent::Frame(
                                    _,
                                    Frame::LayoutSync(_) | Frame::MachineUpdate(_)
                                )
                        );
                        self.on_peer_event(ev).await;
                        if recompute {
                            self.recompute().await;
                        }
                    }
                    None => net_events = None,
                },
                _ = discovery.tick() => {
                    self.discover().await;
                    self.recompute().await;
                }
                _ = async {
                    match ui_at {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.ui_deadline = None;
                    self.publish_ui();
                }
                _ = async {
                    match cfg_at {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.cfg_deadline = None;
                    self.save_config();
                }
            }
        }

        // Graceful exit: close peer sessions so the other side sees Disconnected
        // instead of a half-open socket.
        if let Some(net) = &self.net {
            net.update_dial_targets(Vec::new());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Learn self from tailscale (retrying while LocalAPI is unreachable) and bring
    /// up the net layer. Runs inside the engine task so spawn returns immediately.
    async fn bootstrap(&mut self) {
        let retry = self.poll_interval.min(Duration::from_secs(5));
        loop {
            let status = match self.ts.status().await {
                Ok(status) => status,
                Err(e) => {
                    self.tailscale_error = Some(format!("tailscale unreachable: {e}"));
                    self.publish_ui();
                    tokio::time::sleep(retry).await;
                    continue;
                }
            };
            let Some(bind_ip) = status.self_node.ips.iter().find(|ip| ip.is_ipv4()).copied()
            else {
                self.tailscale_error = Some("tailscale up, no IPv4 address yet".into());
                self.publish_ui();
                tokio::time::sleep(retry).await;
                continue;
            };
            self.tailscale_error = None;
            self.self_info = MachineInfo {
                id: MachineId(status.self_node.stable_id.clone()),
                hostname: if status.self_node.hostname.is_empty() {
                    status.self_node.dns_name.trim_end_matches('.').to_string()
                } else {
                    status.self_node.hostname.clone()
                },
                os: parse_os(&status.self_node.os),
                displays: self.initial_displays.clone(),
            };
            // Loopback bind means an in-process test rig: several engines share the
            // IP, so take an ephemeral port. Production peers dial SPLICE_PORT.
            let port = if bind_ip.is_loopback() { 0 } else { splice_proto::SPLICE_PORT };
            match net::NetManager::spawn_with(
                self.self_info.clone(),
                SocketAddr::new(bind_ip, port),
                self.ts.clone(),
                self.net_opts.clone(),
            )
            .await
            {
                Ok((mgr, control)) => {
                    let _ = self.ready_tx.send(Some(mgr.local_addr));
                    self.net = Some(control);
                    self.net_events = Some(mgr.events);
                    return;
                }
                Err(e) => {
                    self.tailscale_error = Some(format!("listener bind failed: {e}"));
                    self.publish_ui();
                    tokio::time::sleep(retry).await;
                }
            }
        }
    }

    // ----- discovery -----

    async fn discover(&mut self) {
        match self.ts.status().await {
            Ok(status) => {
                self.tailscale_error = None;
                let self_user = status.self_node.user_id;
                let self_stable = status.self_node.stable_id.as_str();
                let mut targets = Vec::new();
                for node in &status.peers {
                    if node.stable_id == self_stable {
                        continue;
                    }
                    let id = MachineId(node.stable_id.clone());
                    let peer = self.peers.entry(id.clone()).or_default();
                    if !node.online || node.user_id != self_user {
                        peer.ts_online = false;
                        continue;
                    }
                    peer.ts_online = true;
                    peer.direct = !node.cur_addr.is_empty();
                    if let Some(ip) = node.ips.iter().find(|ip| ip.is_ipv4()) {
                        targets.push((id, *ip));
                    }
                }
                let listed: HashSet<&str> =
                    status.peers.iter().map(|n| n.stable_id.as_str()).collect();
                for (id, peer) in self.peers.iter_mut() {
                    if !listed.contains(id.0.as_str()) {
                        peer.ts_online = false;
                    }
                }
                // LocalAPI errors keep the last known targets; success replaces them.
                if let Some(net) = &self.net {
                    net.update_dial_targets(targets);
                }
            }
            Err(e) => {
                self.tailscale_error = Some(format!("tailscale status failed: {e}"));
            }
        }
        self.touch_ui();
    }

    // ----- platform events -----

    /// Drain the platform channel and merge runs of consecutive Motion events into
    /// one (sum deltas) before handling, so slow frames coalesce on the wire.
    async fn on_platform_batch(&mut self, first: PlatformEvent) -> bool {
        let mut events = std::mem::take(&mut self.platform_batch);
        push_platform_event(&mut events, first);
        for _ in 1..MAX_PLATFORM_BATCH_EVENTS {
            match self.platform_events.try_recv() {
                Ok(ev) => push_platform_event(&mut events, ev),
                Err(_) => break,
            }
        }
        let mut recompute = false;
        for ev in events.drain(..) {
            recompute |= matches!(&ev, PlatformEvent::DisplaysChanged { .. });
            self.on_platform_event(ev).await;
        }
        self.platform_batch = events;
        recompute
    }

    async fn on_platform_event(&mut self, ev: PlatformEvent) {
        match ev {
            PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id, along }) => {
                self.on_edge_hit(edge_id, along).await;
            }
            PlatformEvent::Capture(CaptureEvent::Input(ev)) => {
                self.on_capture_input(ev).await;
            }
            PlatformEvent::Capture(CaptureEvent::Broken { reason }) => {
                tracing::warn!(reason, "capture backend broke");
                if let Focus::Remote(target) = self.focus.clone() {
                    self.end_remote(&target, LeaveReason::CaptureLost, None, true).await;
                }
            }
            PlatformEvent::Capture(CaptureEvent::Panic) => self.panic().await,
            PlatformEvent::PhysicalActivity => {
                self.claim_source();
                if let Focus::Driven(source) = self.focus.clone() {
                    self.end_driven(&source).await;
                }
            }
            PlatformEvent::ClipboardChanged { mimes, inline_text } => {
                self.on_clipboard_changed(mimes, inline_text);
            }
            PlatformEvent::DisplaysChanged { displays } => {
                tracing::info!(
                    machine = %self.self_info.id,
                    hostname = %self.self_info.hostname,
                    displays = ?displays,
                    "local display geometry updated"
                );
                self.self_info.displays = displays;
                if let Some(net) = &self.net {
                    net.update_self(self.self_info.clone());
                }
                self.touch_ui();
            }
            PlatformEvent::Health(report) => {
                self.health = report;
                self.touch_ui();
            }
        }
    }

    // ----- focus FSM: source side -----

    async fn on_edge_hit(&mut self, edge_id: u32, along: f64) {
        let Some(link) = self.armed.get(edge_id as usize).cloned() else {
            self.reject_edge_hit("unknown barrier id", None).await;
            return;
        };
        let target = link.to.clone();
        let link_is_active = self.links.iter().any(|candidate| candidate == &link);
        if self.focus != Focus::Local
            || !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.machine_enabled(&target)
            || !self.peer_usable(&target)
            || !link_is_active
        {
            let local = match link.side {
                EdgeSide::Left | EdgeSide::Right => {
                    Vec2 { x: f64::from(link.at), y: along }
                }
                EdgeSide::Top | EdgeSide::Bottom => {
                    Vec2 { x: along, y: f64::from(link.at) }
                }
            };
            let warp = position_inside_from_edge(&link, local);
            self.reject_edge_hit("edge is not currently crossable", Some(warp)).await;
            return;
        }
        self.claim_source();
        let local = match link.side {
            EdgeSide::Left | EdgeSide::Right => Vec2 { x: f64::from(link.at), y: along },
            EdgeSide::Top | EdgeSide::Bottom => Vec2 { x: along, y: f64::from(link.at) },
        };
        self.last_local_pos = position_inside_from_edge(&link, local);
        let pos = layout::clamp_into_displays(
            &self.displays_of(&target),
            position_inside_to_edge(&link, local),
        );
        self.session += 1;
        self.active_session = self.session;
        let entered = self
            .net
            .as_ref()
            .is_some_and(|net| net.send_to(&target, Frame::Enter {
                session: self.active_session,
                pos,
            }));
        if !entered {
            let warp = position_inside_from_edge(&link, local);
            self.reject_edge_hit("peer session disappeared before Enter", Some(warp)).await;
            return;
        }
        let _ = self.capture.begin_capture().await;
        if let Some(net) = &self.net {
            net.set_active(&target, true);
        }
        self.focus = Focus::Remote(target);
        self.virtual_pos = pos;
        if let Focus::Remote(target) = &self.focus {
            self.active_sensitivity = self.sensitivity(target);
        }
        self.source_ledger = HeldLedger::default();
        self.touch_ui();
    }

    /// InputCapture activates before the engine can validate replicated state.
    /// Every rejected activation must therefore explicitly hand the pointer back.
    async fn reject_edge_hit(&self, reason: &'static str, warp: Option<Vec2>) {
        tracing::debug!(reason, "releasing rejected edge activation");
        let _ = self.capture.end_capture(warp).await;
    }

    async fn on_capture_input(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Motion { dx, dy } => self.on_remote_motion(dx, dy).await,
            other => {
                if !matches!(self.focus, Focus::Remote(_)) {
                    return;
                }
                self.source_ledger.observe(&other);
                if let (Some(net), Focus::Remote(target)) = (&self.net, &self.focus) {
                    net.send_to(target, Frame::Input { session: self.active_session, ev: other });
                }
            }
        }
    }

    async fn on_remote_motion(&mut self, dx: f64, dy: f64) {
        let dx = dx * self.active_sensitivity;
        let dy = dy * self.active_sensitivity;
        let next = Vec2 { x: self.virtual_pos.x + dx, y: self.virtual_pos.y + dy };
        let (inside, crossing) = match &self.focus {
            Focus::Remote(target) => {
                let inside = layout::union_contains(self.display_slice_of(target), next);
                let crossing = (!inside).then(|| self.find_crossing(target, next)).flatten();
                (inside, crossing)
            }
            _ => return,
        };
        if let Some(link) = crossing {
            self.cross_link(link, next).await;
            return;
        }
        if inside {
            self.virtual_pos = next;
        } else {
            self.virtual_pos = match &self.focus {
                Focus::Remote(target) => {
                    layout::clamp_into_displays(self.display_slice_of(target), next)
                }
                _ => return,
            };
        }
        if let (Some(net), Focus::Remote(target)) = (&self.net, &self.focus) {
            net.send_to(
                target,
                Frame::Input { session: self.active_session, ev: InputEvent::Motion { dx, dy } },
            );
        }
    }

    /// A link FROM `target` whose boundary the (un-clamped, target-local) position
    /// crosses inside its span, honoring corner dead zones. Links only exist toward
    /// healthy machines, so any match is crossable.
    fn find_crossing(&self, target: &MachineId, pos: Vec2) -> Option<EdgeLink> {
        let dz = f64::from(self.cfg.corner_dead_zone);
        self.links
            .iter()
            .find(|link| {
                if link.from != *target {
                    return false;
                }
                let (along, crosses) = match link.side {
                    EdgeSide::Left => (pos.y, pos.x <= f64::from(link.at)),
                    EdgeSide::Right => (pos.y, pos.x >= f64::from(link.at)),
                    EdgeSide::Top => (pos.x, pos.y <= f64::from(link.at)),
                    EdgeSide::Bottom => (pos.x, pos.y >= f64::from(link.at)),
                };
                crosses
                    && along > f64::from(link.from_range.0) + dz
                    && along < f64::from(link.from_range.1) - dz
            })
            .cloned()
    }

    /// The cursor left `target` through `link`: back to us, or onward to a third
    /// machine. The triggering motion is consumed by the transition, not forwarded.
    async fn cross_link(&mut self, link: EdgeLink, pos: Vec2) {
        let landing = position_inside_to_edge(&link, pos);
        if link.to == self.self_info.id {
            let warp = layout::clamp_into_displays(&self.self_info.displays, landing);
            self.send_leave(&link.from, LeaveReason::Crossed, false);
            let _ = self.capture.end_capture(Some(warp)).await;
            self.focus = Focus::Local;
            self.active_sensitivity = 1.0;
            self.source_ledger.drain_releases();
            self.touch_ui();
        } else {
            let next = link.to.clone();
            self.send_leave(&link.from, LeaveReason::Crossed, false);
            let landing = layout::clamp_into_displays(self.display_slice_of(&next), landing);
            self.session += 1;
            self.active_session = self.session;
            let entered = self.net.as_ref().is_some_and(|net| {
                net.send_to(
                    &next,
                    Frame::Enter { session: self.active_session, pos: landing },
                )
            });
            if !entered {
                let _ = self.capture.end_capture(Some(self.last_local_pos)).await;
                self.focus = Focus::Local;
                self.active_sensitivity = 1.0;
                self.source_ledger.drain_releases();
                self.touch_ui();
                return;
            }
            if let Some(net) = &self.net {
                net.set_active(&next, true);
            }
            // Capture stays on across the hop.
            self.focus = Focus::Remote(next);
            self.virtual_pos = landing;
            if let Focus::Remote(target) = &self.focus {
                self.active_sensitivity = self.sensitivity(target);
            }
            self.touch_ui();
        }
    }

    /// Drain the source ledger as key-up Input frames, then Leave (+ ReleaseAll on
    /// safety paths). Only sent while the peer is connected.
    fn send_leave(&mut self, target: &MachineId, reason: LeaveReason, release_all: bool) {
        let Some(net) = &self.net else {
            self.source_ledger.drain_releases();
            return;
        };
        if self.peers.get(target).is_some_and(|p| p.connected) {
            for ev in self.source_ledger.drain_releases() {
                net.send_to(target, Frame::Input { session: self.active_session, ev });
            }
            net.send_to(target, Frame::Leave { session: self.active_session, reason });
            if release_all {
                net.send_to(target, Frame::ReleaseAll);
            }
        } else {
            self.source_ledger.drain_releases();
        }
        net.set_active(target, false);
    }

    /// End a SourceRemote session (safety teardown or sourceness loss).
    async fn end_remote(
        &mut self,
        target: &MachineId,
        reason: LeaveReason,
        warp: Option<Vec2>,
        release_all: bool,
    ) {
        self.send_leave(target, reason, release_all);
        let _ = self.capture.end_capture(warp).await;
        self.focus = Focus::Local;
        self.active_sensitivity = 1.0;
        self.touch_ui();
    }

    // ----- focus FSM: target side -----

    async fn release_target_side(&mut self) {
        self.target_ledger.drain_releases();
        let _ = self.emulate.release_all().await;
    }

    async fn end_driven(&mut self, src: &MachineId) {
        self.release_target_side().await;
        let _ = self.emulate.leave().await;
        if let Some(net) = &self.net {
            net.set_active(src, false);
        }
        self.focus = Focus::Local;
        self.touch_ui();
    }

    async fn panic(&mut self) {
        if let Focus::Remote(target) = self.focus.clone() {
            self.end_remote(&target, LeaveReason::Panic, None, true).await;
        }
        if let Focus::Driven(src) = self.focus.clone() {
            self.end_driven(&src).await;
        }
        if let Some(net) = &self.net {
            net.broadcast(Frame::ReleaseAll);
        }
        self.focus = Focus::Local;
        self.touch_ui();
    }

    // ----- source arbitration -----

    fn claim_source(&mut self) {
        // The current claim is explicitly exchanged on every peer connection.
        // Once we hold it, physical-event bursts do not need to create thousands
        // of new Lamport values or redraw the UI continuously.
        if self.claim.as_ref().is_some_and(|c| c.writer == self.self_info.id) {
            return;
        }
        self.claim_lamport += 1;
        let stamp = Stamp { lamport: self.claim_lamport, writer: self.self_info.id.clone() };
        self.claim = Some(stamp.clone());
        tracing::debug!(source = %stamp.writer, lamport = stamp.lamport, "claiming source");
        if let Some(net) = &self.net {
            net.broadcast(Frame::SourceClaim { stamp });
        }
        self.touch_ui();
    }

    async fn on_source_claim(&mut self, stamp: Stamp) {
        self.claim_lamport = self.claim_lamport.max(stamp.lamport);
        if self.claim.as_ref().is_some_and(|c| stamp <= *c) {
            return;
        }
        self.claim = Some(stamp);
        // Lost sourceness while driving: hand the cursor back locally. If we are the
        // Driven side of the previous holder, keep state — its Leave will arrive.
        if let Focus::Remote(target) = self.focus.clone() {
            self.end_remote(
                &target,
                LeaveReason::SourceChanged,
                Some(self.last_local_pos),
                false,
            )
            .await;
        }
        self.touch_ui();
    }

    // ----- peer events & frames -----

    async fn on_peer_event(&mut self, ev: PeerEvent) {
        match ev {
            PeerEvent::Connected { id, hello, caps, .. } => {
                tracing::info!(
                    peer = %id,
                    hostname = %hello.hostname,
                    displays = ?hello.displays,
                    "peer connected"
                );
                let peer = self.peers.entry(id.clone()).or_default();
                peer.info = Some(hello);
                peer.caps = caps;
                peer.connected = true;
                peer.degraded = false;
                if let (Some(net), Some(doc)) = (&self.net, &self.layout) {
                    net.send_to(&id, Frame::LayoutSync(doc.clone()));
                }
                if let (Some(net), Some(claim)) = (&self.net, self.claim.clone()) {
                    net.send_to(&id, Frame::SourceClaim { stamp: claim });
                }
                self.auto_place(&id);
                self.touch_ui();
            }
            PeerEvent::Frame(from, frame) => self.on_frame(from, frame).await,
            PeerEvent::Degraded(id) => {
                self.peers.entry(id.clone()).or_default().degraded = true;
                if self.focus == Focus::Remote(id.clone()) {
                    self.end_remote(&id, LeaveReason::Reconfigured, Some(self.last_local_pos), true)
                        .await;
                } else if self.focus == Focus::Driven(id.clone()) {
                    self.end_driven(&id).await;
                }
                self.touch_ui();
            }
            PeerEvent::Healthy(id, rtt) => {
                let peer = self.peers.entry(id).or_default();
                peer.degraded = false;
                peer.rtt_ms = Some(rtt);
                self.touch_ui();
            }
            PeerEvent::Disconnected(id, reason) => {
                tracing::debug!(peer = %id, reason, "peer disconnected");
                let peer = self.peers.entry(id.clone()).or_default();
                peer.connected = false;
                peer.degraded = false;
                if self.focus == Focus::Remote(id.clone()) {
                    self.end_remote(&id, LeaveReason::Reconfigured, Some(self.last_local_pos), true)
                        .await;
                } else if self.focus == Focus::Driven(id.clone()) {
                    self.end_driven(&id).await;
                }
                self.touch_ui();
            }
            PeerEvent::Rtt(id, rtt) => {
                self.peers.entry(id).or_default().rtt_ms = Some(rtt);
                self.touch_ui();
            }
        }
    }

    async fn on_frame(&mut self, from: Arc<MachineId>, frame: Frame) {
        match frame {
            Frame::SourceClaim { stamp } => self.on_source_claim(stamp).await,
            Frame::LayoutSync(doc) => {
                self.layout_lamport = self.layout_lamport.max(doc.stamp.lamport);
                if self.layout.as_ref().is_none_or(|l| doc.stamp > l.stamp) {
                    tracing::info!(
                        writer = %doc.stamp.writer,
                        lamport = doc.stamp.lamport,
                        machines = ?doc.machines,
                        "adopting peer layout"
                    );
                    self.layout = Some(doc);
                    self.mark_cfg_dirty();
                    self.touch_ui();
                }
            }
            Frame::MachineUpdate(info) => {
                let id = info.id.clone();
                tracing::info!(
                    peer = %id,
                    hostname = %info.hostname,
                    displays = ?info.displays,
                    "peer display geometry updated"
                );
                self.peers.entry(id.clone()).or_default().info = Some(info);
                self.auto_place(&id);
                self.touch_ui();
            }
            Frame::Enter { session, pos } => {
                self.on_enter((*from).clone(), session, pos).await;
            }
            Frame::Input { session, ev } => {
                if let Focus::Driven(src) = &self.focus {
                    // Stale sessions (after Leave/re-Enter) are discarded.
                    if src == from.as_ref() && session == self.active_session {
                        self.target_ledger.observe(&ev);
                        let _ = self.emulate.inject(ev).await;
                    }
                }
            }
            Frame::Leave { session, reason } => {
                if matches!(&self.focus, Focus::Driven(source) if source == from.as_ref()) {
                    self.end_driven(from.as_ref()).await;
                } else if matches!(&self.focus, Focus::Remote(target) if target == from.as_ref())
                    && session == self.active_session
                {
                    // A target may refuse Enter after observing newer replicated
                    // state or an emulation failure. Treat that refusal as an
                    // immediate source-side teardown.
                    self.end_remote(from.as_ref(), reason, Some(self.last_local_pos), true)
                        .await;
                }
            }
            Frame::ReleaseAll => self.release_target_side().await,
            Frame::ClipOffer { id, stamp, mimes, inline_text } => {
                self.on_clip_offer((*from).clone(), id, stamp, mimes, inline_text).await;
            }
            Frame::ClipRequest { id, mime } => {
                self.on_clip_request((*from).clone(), id, mime).await;
            }
            Frame::ClipChunk { id, mime, data, last } => {
                let key = (id, mime);
                let mut pending = self.pending_fetches.lock();
                if let Some(mut fetch) = pending.remove(&key) {
                    fetch.buf.extend_from_slice(&data);
                    if fetch.buf.len() > CLIP_MAX_TOTAL {
                        let _ = fetch.done.send(None);
                    } else if last {
                        let _ = fetch.done.send(Some(fetch.buf));
                    } else {
                        pending.insert(key, fetch);
                    }
                }
            }
            Frame::ClipAbort { id, .. } => {
                let mut pending = self.pending_fetches.lock();
                let keys: Vec<_> =
                    pending.keys().filter(|(fid, _)| *fid == id).cloned().collect();
                for key in keys {
                    if let Some(fetch) = pending.remove(&key) {
                        let _ = fetch.done.send(None);
                    }
                }
            }
            _ => {}
        }
    }

    async fn on_enter(&mut self, from: MachineId, session: u64, pos: Vec2) {
        if !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.machine_enabled(&from)
            || !self.peer_usable(&from)
            || matches!(self.focus, Focus::Remote(_))
        {
            self.refuse_enter(&from, session);
            return;
        }
        // Only the current, explicitly replicated source holder may drive us.
        if !self.claim.as_ref().is_some_and(|c| c.writer == from) {
            self.refuse_enter(&from, session);
            return;
        }
        if let Focus::Driven(old) = self.focus.clone() {
            if old == from {
                self.release_target_side().await;
            } else {
                self.end_driven(&old).await;
            }
        }
        if let Err(err) = self.emulate.enter(pos).await {
            tracing::warn!(source = %from, error = %err, "cannot enter target emulation");
            self.refuse_enter(&from, session);
            return;
        }
        self.focus = Focus::Driven(from.clone());
        self.active_session = session;
        self.target_ledger = HeldLedger::default();
        if let Some(net) = &self.net {
            net.set_active(&from, true);
        }
        self.touch_ui();
    }

    fn refuse_enter(&self, source: &MachineId, session: u64) {
        if let Some(net) = &self.net {
            net.send_to(
                source,
                Frame::Leave { session, reason: LeaveReason::Reconfigured },
            );
        }
    }

    // ----- layout doc -----

    fn ensure_doc(&mut self) {
        if self.layout.is_none() {
            self.layout = Some(LayoutDoc {
                stamp: Stamp { lamport: 0, writer: self.self_info.id.clone() },
                machines: BTreeMap::new(),
                sensitivity: BTreeMap::new(),
            });
        }
        if !self
            .layout
            .as_ref()
            .is_some_and(|d| d.machines.contains_key(&self.self_info.id))
        {
            self.layout.as_mut().expect("doc exists").machines.insert(
                self.self_info.id.clone(),
                MachinePlacement { offset: Vec2I { x: 0, y: 0 }, enabled: true },
            );
            self.bump_layout();
        }
    }

    /// A newly seen machine absent from the doc is placed right of the current
    /// rightmost machine, top-aligned at y=0, enabled.
    fn auto_place(&mut self, id: &MachineId) {
        if *id == self.self_info.id {
            return;
        }
        if self
            .layout
            .as_ref()
            .is_some_and(|d| d.machines.contains_key(id))
        {
            return;
        }
        self.ensure_doc();
        let rightmost = self
            .layout
            .as_ref()
            .expect("doc exists")
            .machines
            .iter()
            .map(|(mid, p)| {
                let right = self
                    .displays_of(mid)
                    .iter()
                    .map(|d| d.x + d.w as i32)
                    .max()
                    .unwrap_or(0);
                p.offset.x + right
            })
            .max()
            .unwrap_or(0);
        self.layout.as_mut().expect("doc exists").machines.insert(
            id.clone(),
            MachinePlacement { offset: Vec2I { x: rightmost, y: 0 }, enabled: true },
        );
        self.bump_layout();
    }

    fn bump_layout(&mut self) {
        self.layout_lamport += 1;
        let stamp = Stamp { lamport: self.layout_lamport, writer: self.self_info.id.clone() };
        let Some(doc) = self.layout.as_mut() else {
            return;
        };
        doc.stamp = stamp;
        if let Some(net) = &self.net {
            net.broadcast(Frame::LayoutSync(doc.clone()));
        }
        self.mark_cfg_dirty();
        self.touch_ui();
    }

    // ----- commands -----

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::SetMasterEnabled(on) => {
                self.cfg.master_enabled = on;
                self.mark_cfg_dirty();
                if !on {
                    if let Focus::Remote(target) = self.focus.clone() {
                        self.end_remote(
                            &target,
                            LeaveReason::Reconfigured,
                            Some(self.last_local_pos),
                            true,
                        )
                        .await;
                    }
                    if let Focus::Driven(src) = self.focus.clone() {
                        self.end_driven(&src).await;
                    }
                }
                self.touch_ui();
            }
            Command::SetMachineEnabled(id, enabled) => {
                self.ensure_doc();
                self.layout
                    .as_mut()
                    .expect("doc exists")
                    .machines
                    .entry(id)
                    .or_insert(MachinePlacement { offset: Vec2I { x: 0, y: 0 }, enabled: true })
                    .enabled = enabled;
                self.bump_layout();
            }
            Command::SetPlacement(id, offset) => {
                self.ensure_doc();
                let moving = self.displays_of(&id);
                let others: Vec<(Vec<DisplayRect>, Vec2I)> = self
                    .layout
                    .as_ref()
                    .expect("doc exists")
                    .machines
                    .iter()
                    .filter(|(mid, _)| **mid != id)
                    .map(|(mid, p)| (self.displays_of(mid), p.offset))
                    .collect();
                let others_ref: Vec<(&[DisplayRect], Vec2I)> =
                    others.iter().map(|(d, o)| (d.as_slice(), *o)).collect();
                let snapped = layout::snap_offset(&moving, offset, &others_ref, SNAP_TOLERANCE);
                self.layout
                    .as_mut()
                    .expect("doc exists")
                    .machines
                    .entry(id)
                    .or_insert(MachinePlacement { offset: Vec2I { x: 0, y: 0 }, enabled: true })
                    .offset = snapped;
                self.bump_layout();
            }
            Command::SetSensitivity { link_key, factor } => {
                self.ensure_doc();
                self.layout
                    .as_mut()
                    .expect("doc exists")
                    .sensitivity
                    .insert(link_key, factor.clamp(0.25, 4.0));
                self.bump_layout();
            }
            Command::SetClipboardSync(on) => {
                self.cfg.clipboard_sync = on;
                self.mark_cfg_dirty();
                self.touch_ui();
            }
            Command::Panic => self.panic().await,
            Command::Refresh => self.discover().await,
        }
    }

    // ----- clipboard broker -----

    fn on_clipboard_changed(&mut self, mimes: Vec<String>, inline_text: Option<String>) {
        if !self.cfg.clipboard_sync {
            return;
        }
        // Loop guard: don't re-offer what we just applied from a remote offer.
        if inline_text.is_some() && inline_text == self.last_applied_inline {
            return;
        }
        self.clip_lamport += 1;
        let stamp = Stamp { lamport: self.clip_lamport, writer: self.self_info.id.clone() };
        self.clip_seen = Some(stamp.clone());
        self.offer_id += 1;
        let id = self.offer_id;
        self.live_offer = Some((id, mimes.clone()));
        if let Some(net) = &self.net {
            net.broadcast(Frame::ClipOffer { id, stamp, mimes, inline_text });
        }
    }

    async fn on_clip_offer(
        &mut self,
        from: MachineId,
        id: u64,
        stamp: Stamp,
        mimes: Vec<String>,
        inline_text: Option<String>,
    ) {
        self.clip_lamport = self.clip_lamport.max(stamp.lamport);
        if !self.cfg.clipboard_sync {
            return;
        }
        if !self
            .peers
            .get(&from)
            .is_some_and(|p| p.caps.iter().any(|c| c == caps::CLIPBOARD_V1))
        {
            return;
        }
        if self.clip_seen.as_ref().is_some_and(|seen| stamp <= *seen) {
            return;
        }
        self.clip_seen = Some(stamp);
        self.last_applied_inline = inline_text.clone();
        if let Some(net) = &self.net {
            let fetch = Arc::new(RemoteFetch {
                net: net.clone(),
                origin: from,
                id,
                pending: self.pending_fetches.clone(),
            });
            let _ = self
                .clipboard
                .set_remote_offer(ClipboardOffer { id, mimes, inline_text }, fetch)
                .await;
        }
    }

    async fn on_clip_request(&mut self, from: MachineId, id: u64, mime: String) {
        let live = self.live_offer.as_ref().is_some_and(|(oid, _)| *oid == id);
        let Some(net) = &self.net else {
            return;
        };
        if !live {
            net.send_to(&from, Frame::ClipAbort { id, reason: "stale offer".into() });
            return;
        }
        match self.clipboard.read_local(&mime).await {
            Ok(bytes) if bytes.is_empty() => {
                net.send_to(&from, Frame::ClipChunk { id, mime, data: Vec::new(), last: true });
            }
            Ok(bytes) => {
                let chunks: Vec<&[u8]> = bytes.chunks(CLIP_CHUNK).collect();
                let count = chunks.len();
                for (index, chunk) in chunks.into_iter().enumerate() {
                    net.send_to(
                        &from,
                        Frame::ClipChunk {
                            id,
                            mime: mime.clone(),
                            data: chunk.to_vec(),
                            last: index + 1 == count,
                        },
                    );
                }
            }
            Err(e) => {
                net.send_to(&from, Frame::ClipAbort { id, reason: e.to_string() });
            }
        }
    }

    // ----- recompute & helpers -----

    /// Recompute the derived state from layout + reachability. Focus validity,
    /// crossable links and OS barriers are all projections of authoritative state.
    async fn recompute(&mut self) {
        self.reconcile_focus().await;
        let mut geo: BTreeMap<MachineId, MachineGeom> = BTreeMap::new();
        let mut active: BTreeMap<MachineId, MachineGeom> = BTreeMap::new();
        if let Some(doc) = &self.layout {
            for (id, placement) in &doc.machines {
                let reachable = *id == self.self_info.id || self.peer_usable(id);
                let displays = self.displays_of(id);
                geo.insert(
                    id.clone(),
                    MachineGeom {
                        id: id.clone(),
                        displays: displays.clone(),
                        placement: MachinePlacement {
                            offset: placement.offset,
                            // Geometry is deliberately independent of the logical
                            // enable bit; compute_links filters disabled placements.
                            enabled: true,
                        },
                        reachable: true,
                    },
                );
                if placement.enabled {
                    active.insert(
                        id.clone(),
                        MachineGeom {
                            id: id.clone(),
                            displays,
                            placement: placement.clone(),
                            reachable,
                        },
                    );
                }
            }
        }
        self.geo_links = layout::compute_links(&geo);
        self.links = layout::compute_links(&active);
        self.armed = self
            .geo_links
            .iter()
            .filter(|link| link.from == self.self_info.id)
            .cloned()
            .collect();
        // Barrier geometry follows physical placement only. Focus, enable toggles and
        // peer liveness are enforced above/on EdgeHit and must not recreate a portal
        // session (v1 portals prompt again for every new session).
        let specs = layout::edge_specs_for(&self.geo_links, &self.self_info.id);
        let _ = self.capture.set_edges(specs).await;
        self.active_sensitivity = match &self.focus {
            Focus::Remote(target) => self.sensitivity(target),
            _ => 1.0,
        };
    }

    /// Enforce the focus FSM invariants after every command, platform batch and
    /// replicated peer event. No individual event handler owns special teardown rules.
    async fn reconcile_focus(&mut self) {
        match self.focus.clone() {
            Focus::Local => {}
            Focus::Remote(target) => {
                let valid = self.cfg.master_enabled
                    && self.machine_enabled(&self.self_info.id)
                    && self.machine_enabled(&target)
                    && self.peer_usable(&target)
                    && self.claim.as_ref().is_some_and(|c| c.writer == self.self_info.id);
                if !valid {
                    self.end_remote(
                        &target,
                        LeaveReason::Reconfigured,
                        Some(self.last_local_pos),
                        true,
                    )
                    .await;
                }
            }
            Focus::Driven(source) => {
                let valid = self.cfg.master_enabled
                    && self.machine_enabled(&self.self_info.id)
                    && self.machine_enabled(&source)
                    && self.peer_usable(&source)
                    && self.claim.as_ref().is_some_and(|c| c.writer == source);
                if !valid {
                    self.end_driven(&source).await;
                }
            }
        }
    }

    fn peer_usable(&self, id: &MachineId) -> bool {
        self.peers.get(id).is_some_and(|p| p.connected && !p.degraded)
    }

    fn machine_enabled(&self, id: &MachineId) -> bool {
        self.layout
            .as_ref()
            .and_then(|d| d.machines.get(id))
            .is_none_or(|p| p.enabled)
    }

    fn displays_of(&self, id: &MachineId) -> Vec<DisplayRect> {
        self.display_slice_of(id).to_vec()
    }

    fn display_slice_of(&self, id: &MachineId) -> &[DisplayRect] {
        if *id == self.self_info.id {
            return &self.self_info.displays;
        }
        self.peers
            .get(id)
            .and_then(|p| p.info.as_ref())
            .map(|info| info.displays.as_slice())
            .unwrap_or(&[])
    }

    fn sensitivity(&self, target: &MachineId) -> f64 {
        self.layout
            .as_ref()
            .and_then(|d| d.sensitivity.get(&LayoutDoc::link_key(&self.self_info.id, target)))
            .copied()
            .unwrap_or(1.0)
    }

    // ----- UiState & config -----

    fn touch_ui(&mut self) {
        if self.ui_deadline.is_none() {
            self.ui_deadline = Some(Instant::now() + UI_DEBOUNCE);
        }
    }

    fn mark_cfg_dirty(&mut self) {
        self.cfg_deadline = Some(Instant::now() + CFG_DEBOUNCE);
    }

    fn save_config(&mut self) {
        self.cfg.layout = self.layout.clone();
        if let Err(e) = config::save(&self.data_dir, &self.cfg) {
            tracing::warn!(error = %e, "config save failed");
        }
    }

    fn publish_ui(&mut self) {
        let _ = self.ui_tx.send(self.build_ui());
    }

    fn build_ui(&self) -> UiState {
        let source = self.claim.as_ref().map(|c| c.writer.clone());
        let doc = &self.layout;
        let placement_of = |id: &MachineId| doc.as_ref().and_then(|d| d.machines.get(id));

        let mut machines = vec![UiMachine {
            id: self.self_info.id.clone(),
            hostname: self.self_info.hostname.clone(),
            os: self.self_info.os,
            displays: self.self_info.displays.clone(),
            offset: placement_of(&self.self_info.id).map(|p| p.offset).unwrap_or_default(),
            enabled: placement_of(&self.self_info.id).is_none_or(|p| p.enabled),
            connection: UiConnection::SelfMachine,
            is_source: source.as_ref() == Some(&self.self_info.id),
        }];
        let mut peers: Vec<UiMachine> = self
            .peers
            .iter()
            .filter_map(|(id, peer)| {
                let info = peer.info.as_ref()?;
                // Degraded has no dedicated badge; Connecting reads as "flaky link".
                let connection = if !peer.connected {
                    if peer.ts_online {
                        UiConnection::Connecting
                    } else {
                        UiConnection::Offline
                    }
                } else if peer.degraded {
                    UiConnection::Connecting
                } else {
                    let rtt_ms = peer.rtt_ms.unwrap_or(0.0);
                    if peer.direct {
                        UiConnection::Direct { rtt_ms }
                    } else {
                        UiConnection::Derp { rtt_ms }
                    }
                };
                Some(UiMachine {
                    id: id.clone(),
                    hostname: info.hostname.clone(),
                    os: info.os,
                    displays: info.displays.clone(),
                    offset: placement_of(id).map(|p| p.offset).unwrap_or_default(),
                    enabled: placement_of(id).is_none_or(|p| p.enabled),
                    connection,
                    is_source: source.as_ref() == Some(id),
                })
            })
            .collect();
        peers.sort_by(|a, b| a.id.cmp(&b.id));
        machines.extend(peers);

        let mut edges = Vec::new();
        for link in &self.geo_links {
            // One strip per unordered pair + segment (links exist in both directions).
            if link.from >= link.to {
                continue;
            }
            let Some(off) = placement_of(&link.from).map(|p| p.offset) else {
                continue;
            };
            let (x1, y1, x2, y2) = match link.side {
                EdgeSide::Left | EdgeSide::Right => {
                    let x = link.at + off.x;
                    (x, link.from_range.0 + off.y, x, link.from_range.1 + off.y)
                }
                EdgeSide::Top | EdgeSide::Bottom => {
                    let y = link.at + off.y;
                    (link.from_range.0 + off.x, y, link.from_range.1 + off.x, y)
                }
            };
            let usable = |id: &MachineId| {
                *id == self.self_info.id || self.peer_usable(id)
            };
            let crossable = self.machine_enabled(&link.from)
                && self.machine_enabled(&link.to)
                && usable(&link.from)
                && usable(&link.to);
            edges.push(UiEdge { a: link.from.clone(), b: link.to.clone(), x1, y1, x2, y2, crossable });
        }

        let focus = match &self.focus {
            Focus::Local => UiFocus::Local,
            Focus::Remote(t) => UiFocus::Remote(t.clone()),
            Focus::Driven(s) => UiFocus::Driven(s.clone()),
        };
        UiState {
            self_id: self.self_info.id.clone(),
            master_enabled: self.cfg.master_enabled,
            clipboard_sync: self.cfg.clipboard_sync,
            machines,
            edges,
            source,
            focus,
            health: self.health.clone(),
            panic_chord: format_chord(&self.cfg.panic_chord),
            sensitivity: doc.as_ref().map(|d| d.sensitivity.clone()).unwrap_or_default(),
            tailscale_error: self.tailscale_error.clone(),
        }
    }
}

fn push_platform_event(events: &mut Vec<PlatformEvent>, ev: PlatformEvent) {
    if let PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Motion {
        dx: next_dx,
        dy: next_dy,
    })) = ev
    {
        if let Some(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Motion { dx, dy }))) =
            events.last_mut()
        {
            *dx += next_dx;
            *dy += next_dy;
        } else {
            events.push(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Motion {
                dx: next_dx,
                dy: next_dy,
            })));
        }
    } else {
        events.push(ev);
    }
}

/// Map a crossing position through a link into the TO machine's local coords (1:1
/// along the shared span; spans are equal-length by construction).
fn landing_pos(link: &EdgeLink, pos: Vec2) -> Vec2 {
    let along = match link.side {
        EdgeSide::Left | EdgeSide::Right => pos.y,
        EdgeSide::Top | EdgeSide::Bottom => pos.x,
    };
    let to_along = f64::from(link.to_range.0) + (along - f64::from(link.from_range.0));
    match link.side {
        EdgeSide::Left | EdgeSide::Right => Vec2 { x: f64::from(link.to_at), y: to_along },
        EdgeSide::Top | EdgeSide::Bottom => Vec2 { x: to_along, y: f64::from(link.to_at) },
    }
}

fn position_inside_from_edge(link: &EdgeLink, pos: Vec2) -> Vec2 {
    match link.side {
        EdgeSide::Left => Vec2 { x: f64::from(link.at) + 1.0, y: pos.y },
        EdgeSide::Right => Vec2 { x: f64::from(link.at) - 1.0, y: pos.y },
        EdgeSide::Top => Vec2 { x: pos.x, y: f64::from(link.at) + 1.0 },
        EdgeSide::Bottom => Vec2 { x: pos.x, y: f64::from(link.at) - 1.0 },
    }
}

fn position_inside_to_edge(link: &EdgeLink, pos: Vec2) -> Vec2 {
    let mut landing = landing_pos(link, pos);
    match link.side {
        EdgeSide::Left => landing.x -= 1.0,
        EdgeSide::Right => landing.x += 1.0,
        EdgeSide::Top => landing.y -= 1.0,
        EdgeSide::Bottom => landing.y += 1.0,
    }
    landing
}

fn parse_os(os: &str) -> Os {
    match os.to_ascii_lowercase().as_str() {
        "macos" | "darwin" => Os::Macos,
        "linux" => Os::Linux,
        _ => Os::Other,
    }
}

fn format_chord(codes: &[u32]) -> String {
    use splice_platform::keymap::ev;
    let name = |code: u32| match code {
        ev::KEY_LEFTCTRL | ev::KEY_RIGHTCTRL => "Ctrl".to_string(),
        ev::KEY_LEFTALT | ev::KEY_RIGHTALT => "Alt".to_string(),
        ev::KEY_LEFTSHIFT => "Left Shift".to_string(),
        ev::KEY_RIGHTSHIFT => "Right Shift".to_string(),
        ev::KEY_LEFTMETA | ev::KEY_RIGHTMETA => "Meta".to_string(),
        ev::KEY_ESC => "Esc".to_string(),
        ev::KEY_DELETE => "Del".to_string(),
        ev::KEY_UP => "Up".to_string(),
        ev::KEY_DOWN => "Down".to_string(),
        ev::KEY_LEFT => "Left".to_string(),
        ev::KEY_RIGHT => "Right".to_string(),
        other => format!("Key{other}"),
    };
    codes.iter().map(|c| name(*c)).collect::<Vec<_>>().join("+")
}
