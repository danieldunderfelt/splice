//! Engine task internals: single task owning all mutable state. Driven by platform
//! events, peer session events (net layer), discovery ticks and UI commands; drives
//! capture/emulation and publishes UiState snapshots (debounced to <=10 Hz).

mod crossing;
mod raw;

use crate::engine::Command;
use crate::arrange::{self, Body, Rules};
use crate::layout::{self, EdgeLink, MachineGeom};
use crate::ledger::HeldLedger;
use crate::net::{self, NetControl, NetOpts, PeerEvent, TsApi};
use crate::ui_state::{UiConnection, UiEdge, UiFocus, UiMachine, UiState};
use crate::config;
use splice_platform::{
    BackendPrefs, BackendStatus, CaptureEvent, ClipboardOffer, EdgeSide, HealthReport,
    PlatformEvent,
};
use splice_proto::{
    caps, DisplayRect, Frame, InputEvent, LayoutDoc, LeaveReason, MachineId, MachineInfo,
    MachinePlacement, Os, Stamp, Vec2, Vec2I, CLIP_CHUNK, CLIP_MAX_TOTAL,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// UiState bursts are coalesced to one publish per this window (DESIGN: <=10 Hz).
const UI_DEBOUNCE: Duration = Duration::from_millis(100);
/// Config writes are debounced this long after the last change.
const CFG_DEBOUNCE: Duration = Duration::from_secs(1);
const MAX_PLATFORM_BATCH_EVENTS: usize = 64;
const DRIVEN_GRACE: Duration = Duration::from_secs(1);
const TS_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq)]
enum Focus {
    Local,
    Remote(MachineId),
    Driven(MachineId),
}

#[derive(Default)]
struct Peer {
    raw_generation: u64,
    hostname: Option<String>,
    error: Option<String>,
    info: Option<MachineInfo>,
    caps: Vec<String>,
    connected: bool,
    degraded: bool,
    master_off: bool,
    ts_online: bool,
    /// Tailscale reports a direct path (CurAddr non-empty) vs DERP relay.
    direct: bool,
    rtt_ms: Option<f64>,
}

pub struct Inner {
    raw: raw::RawState,
    crossing: Option<crossing::Crossing>,
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
    armed_specs: Vec<splice_platform::EdgeSpec>,
    health: HealthReport,
    backends: Option<watch::Sender<BackendPrefs>>,
    backend_status: Option<BackendStatus>,
    tailscale_error: Option<String>,
    config_error: Option<String>,
    diagnostics: crate::diagnostics::Diagnostics,
    update_host: Option<splice_update::Host>,
    updates: Option<crate::updates::Updates>,
    restart_requested: bool,
    ui_deadline: Option<Instant>,
    cfg_deadline: Option<Instant>,
    platform_batch: Vec<PlatformEvent>,
    driven_grace_until: Option<Instant>,

    clip_lamport: u64,
    clip_seen: Option<Stamp>,
    last_applied_inline: Option<String>,
    offer_id: u64,
    live_offer: Option<(u64, Vec<String>)>,
    pending_fetches: crate::clipboard::Transfers,
    clipboard_jobs: tokio::task::JoinSet<()>,
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
        update_host: Option<splice_update::Host>,
    ) -> anyhow::Result<Self> {
        let cfg = config::load(&data_dir)?;
        let layout_lamport = cfg.layout.as_ref().map(|d| d.stamp.lamport).unwrap_or(0);
        let splice_platform::Platform {
            raw_capture,
            raw_emulate,
            capture,
            emulate,
            clipboard,
            displays,
            events,
            backends,
        } = platform;
        if let Some(backends) = &backends {
            let _ = backends.send(cfg.backends);
        }
        Ok(Inner {
            crossing: None,
            raw: raw::RawState::new(
                raw_capture,
                raw_emulate,
                crate::input_settings::InputSettings::load(&data_dir, cfg.edge_dwell_ms)?,
            ),
            self_info: MachineInfo {
                build: splice_proto::BuildInfo::current(),
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
            armed_specs: Vec::new(),
            health: HealthReport::default(),
            backends,
            backend_status: None,
            tailscale_error: None,
            config_error: None,
            diagnostics: Default::default(),
            update_host,
            updates: None,
            restart_requested: false,
            ui_deadline: None,
            cfg_deadline: None,
            platform_batch: Vec::with_capacity(16),
            driven_grace_until: None,
            clip_lamport: 0,
            clip_seen: None,
            last_applied_inline: None,
            offer_id: 0,
            live_offer: None,
            pending_fetches: crate::clipboard::Transfers::default(),
            clipboard_jobs: tokio::task::JoinSet::new(),
        })
    }

    pub async fn run(mut self) {
        if !self.bootstrap().await {
            return;
        }
        self.ensure_doc();
        if self.settle_layout() {
            self.bump_layout();
        }
        self.discover().await;
        self.recompute().await;
        self.publish_ui();

        let mut net_events = self.net_events.take();
        let mut discovery = tokio::time::interval_at(
            tokio::time::Instant::now() + self.poll_interval,
            self.poll_interval,
        );
        let mut platform_open = true;
        let mut crossing_tick = tokio::time::interval(Duration::from_millis(16));
        crossing_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut diagnostic_tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            let ui_at = self.ui_deadline;
            let cfg_at = self.cfg_deadline;
            tokio::select! {
                _ = crossing_tick.tick(), if self.crossing.is_some() => self.crossing_tick().await,
                Some(event) = self.raw.events.recv() => self.on_raw_event(event).await,
                _ = diagnostic_tick.tick() => {
                    if let Some(updates) = &mut self.updates {
                        updates.poll();
                        if updates.restart_requested() {
                            self.restart_requested = true;
                            break;
                        }
                    }
                    self.touch_ui();
                }
                result = self.clipboard_jobs.join_next(), if !self.clipboard_jobs.is_empty() => {
                    if let Some(Err(error)) = result {
                        if !error.is_cancelled() {
                            tracing::error!(%error, "clipboard request task failed");
                        }
                    }
                }
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
                                    Frame::LayoutSync(_)
                                        | Frame::MachineUpdate(_)
                                        | Frame::MasterState { .. }
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

        match self.focus.clone() {
            Focus::Remote(target) => {
                self.end_remote(&target, LeaveReason::Reconfigured, Some(self.last_local_pos), true).await
            }
            Focus::Driven(source) => self.end_driven(&source, Some(LeaveReason::Reconfigured)).await,
            Focus::Local => {}
        }
        self.stop_raw().await;
        self.pending_fetches.clear();
        self.clipboard_jobs.abort_all();
        if self.cfg_deadline.is_some() {
            self.save_config();
        }
        if let Some(net) = &self.net {
            net.update_dial_targets(Vec::new());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if self.restart_requested { self.publish_ui(); }
    }

    /// Learn self from tailscale (retrying while LocalAPI is unreachable) and bring
    /// up the net layer. Runs inside the engine task so spawn returns immediately.
    async fn bootstrap(&mut self) -> bool {
        let retry = self.poll_interval.min(Duration::from_secs(5));
        loop {
            if self.cmd.is_closed() {
                return false;
            }
            let status = match self.ts_status().await {
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
                build: splice_proto::BuildInfo::current(),
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
                    if let Some(host) = self.update_host.take() {
                        self.updates = Some(crate::updates::Updates::new(host, self.self_info.id.clone(), bind_ip, self.ts.clone()).await);
                    }
                    return true;
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

    async fn ts_status(&self) -> Result<splice_tailscale::Status, String> {
        match tokio::time::timeout(TS_STATUS_TIMEOUT, self.ts.status()).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("LocalAPI status timed out".into()),
        }
    }

    async fn discover(&mut self) {
        match self.ts_status().await {
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
                    peer.hostname = Some(node.hostname.clone());
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
                if let Some(updates) = &mut self.updates { updates.discover(&targets); }
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
            PlatformEvent::Capture(CaptureEvent::EdgeMotion {
                edge_id,
                along,
                dx,
                dy,
            }) => self.edge_motion(edge_id, along, dx, dy).await,
            PlatformEvent::Capture(CaptureEvent::EdgeLeft) => {
                if self
                    .crossing
                    .as_ref()
                    .is_some_and(|c| c.local_edge.is_some())
                {
                    self.crossing = None;
                    self.touch_ui();
                }
                if self.raw.preparing.is_some() && self.raw.edge.is_some() {
                    if let Focus::Remote(target) = self.focus.clone() {
                        self.end_remote(&target, LeaveReason::Crossed, None, true)
                            .await;
                    }
                }
            }
            PlatformEvent::SwitchTarget => self.switch_target().await,
            PlatformEvent::RawCaptureFailed(operation) => {
                if Arc::ptr_eq(&self.raw.operation, &operation) {
                    self.raw_capture_failed(operation.error().expect("capture failure includes its reason")).await;
                }
            }
            PlatformEvent::RawError(reason) => self.raw_capture_failed(reason).await,
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
                if !self.cfg.master_enabled || !self.machine_enabled(&self.self_info.id) {
                    return;
                }
                self.claim_source();
                if let Focus::Driven(source) = self.focus.clone() {
                    self.end_driven(&source, Some(LeaveReason::SourceChanged)).await;
                }
                self.driven_grace_until = None;
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
                if self.settle_layout() {
                    self.bump_layout();
                }
                self.touch_ui();
            }
            PlatformEvent::Health(report) => {
                tracing::info!(?report, "platform health");
                self.health = report;
                self.touch_ui();
            }
            PlatformEvent::Backends(status) => {
                tracing::info!(?status, "platform backends");
                self.backend_status = Some(status);
                self.touch_ui();
            }
        }
    }

    // ----- focus FSM: source side -----

    async fn on_edge_hit(&mut self, edge_id: u32, along: f64) {
        self.begin_edge(edge_id, along, false).await;
    }

    async fn begin_edge(&mut self, edge_id: u32, along: f64, committed: bool) {
        let Some(link) = self.armed.get(edge_id as usize).cloned() else {
            self.reject_edge_hit("unknown barrier id", None).await;
            return;
        };
        let target = link.to.clone();
        let link_is_active = self.links.iter().any(|candidate| candidate == &link);
        let recently_driven = self
            .driven_grace_until
            .is_some_and(|until| Instant::now() < until);
        let block = if self.focus != Focus::Local {
            Some("already focused elsewhere")
        } else if recently_driven {
            Some("in post-driven grace window")
        } else if !self.cfg.master_enabled {
            Some("master disabled")
        } else if !self.machine_enabled(&self.self_info.id) {
            Some("self disabled in layout")
        } else if !self.machine_enabled(&target) {
            Some("target disabled in layout")
        } else if !self.peer_usable(&target) {
            Some("target not connected")
        } else if !link_is_active {
            Some("edge link not currently active")
        } else {
            None
        };
        if let Some(reason) = block {
            tracing::debug!(target = %target, reason, "edge hit not crossable");
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
        if !committed && self.contact_gesture(&link, edge_id, along).await {
            return;
        }
        let pos = layout::clamp_into_displays(
            &self.displays_of(&target),
            position_inside_to_edge(&link, local),
        );
        if self.raw.settings.mode(&target) == splice_proto::raw::InputMode::Raw {
            self.start_raw(target, pos, Some(edge_id)).await;
            return;
        }
        self.start_desktop(target, pos).await;
    }

    async fn start_desktop(&mut self, target: MachineId, pos: Vec2) {
        self.claim_source();
        self.session += 1;
        self.active_session = self.session;
        let entered = self.net.as_ref().is_some_and(|net| {
            net.send_to(
                &target,
                Frame::Enter {
                    session: self.active_session,
                    pos,
                },
            )
        });
        if !entered {
            self.raw.error = Some("The destination disconnected before capture".into());
            self.reject_edge_hit(
                "peer session disappeared before Enter",
                Some(self.last_local_pos),
            )
            .await;
            self.touch_ui();
            return;
        }
        tracing::debug!(target = %target, session = self.active_session, ?pos, "entering remote machine");
        if let Err(error) = self.capture.begin_capture().await {
            if let Some(net) = &self.net {
                net.send_to(
                    &target,
                    Frame::Leave {
                        session: self.active_session,
                        reason: LeaveReason::CaptureLost,
                    },
                );
            }
            self.raw.error = Some(format!("Cannot capture desktop input: {error}"));
            self.capture
                .end_capture(Some(self.last_local_pos))
                .await
                .ok();
            self.touch_ui();
            return;
        }
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

    async fn raw_capture_failed(&mut self, reason: String) {
        self.raw.error = Some(reason);
        if self.raw.active || self.raw.preparing.is_some() {
            if let Focus::Remote(target) = self.focus.clone() {
                self.end_remote(
                    &target,
                    LeaveReason::CaptureLost,
                    Some(self.last_local_pos),
                    true,
                )
                .await;
            }
        }
        self.touch_ui();
    }

    async fn on_capture_input(&mut self, ev: InputEvent) {
        if self.raw.active || self.raw.preparing.is_some() {
            return;
        }
        match ev {
            InputEvent::Motion { dx, dy } => self.on_remote_motion(dx, dy).await,
            other => {
                if !matches!(self.focus, Focus::Remote(_)) {
                    return;
                }
                if !self.source_ledger.observe(&other) { return; }
                if let (Some(net), Focus::Remote(target)) = (&self.net, &self.focus) {
                    net.send_to(target, Frame::Input { session: self.active_session, ev: other });
                }
            }
        }
    }

    async fn on_remote_motion(&mut self, dx: f64, dy: f64) {
        if self.remote_gesture_motion(dx, dy).await {
            return;
        }
        let dx = dx * self.active_sensitivity;
        let dy = dy * self.active_sensitivity;
        let next = Vec2 { x: self.virtual_pos.x + dx, y: self.virtual_pos.y + dy };
        let (inside, crossing) = match &self.focus {
            Focus::Remote(target) => {
                let inside = layout::union_contains(self.display_slice_of(target), next);
                let crossing = (!inside && !self.raw.settings.focus_lock)
                    .then(|| self.find_crossing(target, next))
                    .flatten();
                (inside, crossing)
            }
            _ => return,
        };
        if let Some(link) = crossing {
            if self.start_remote_gesture(link.clone(), next) {
                return;
            }
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
                let span = f64::from(link.from_range.1 - link.from_range.0);
                let dz = dz.min(span / 4.0);
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
        if link.to != self.self_info.id
            && self.raw.settings.mode(&link.to) == splice_proto::raw::InputMode::Raw
        {
            self.handoff_remote(link.to, landing).await;
            return;
        }
        if link.to == self.self_info.id {
            let warp = layout::clamp_into_displays(&self.self_info.displays, landing);
            tracing::debug!(from = %link.from, ?warp, "cursor crossed back home");
            self.send_leave(&link.from, LeaveReason::Crossed, false);
            let _ = self.capture.end_capture(Some(warp)).await;
            self.focus = Focus::Local;
            self.active_sensitivity = 1.0;
            self.source_ledger.drain_releases();
            self.touch_ui();
        } else {
            let next = link.to.clone();
            let held = self.source_ledger.clone();
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
                for event in held.presses() {
                    net.send_to(&next, Frame::Input { session: self.active_session, ev: event });
                }
            }
            self.source_ledger = held;
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
        tracing::debug!(target = %target, ?reason, ?warp, "remote session ended");
        self.leave_remote(target, reason, release_all).await;
        let _ = self.capture.end_capture(warp).await;
    }

    async fn leave_remote(&mut self, target: &MachineId, reason: LeaveReason, release_all: bool) {
        self.crossing = None;
        self.stop_raw().await;
        self.send_leave(target, reason, release_all);
        self.focus = Focus::Local;
        self.active_sensitivity = 1.0;
        self.touch_ui();
    }

    // ----- focus FSM: target side -----

    async fn release_target_side(&mut self) {
        self.target_ledger.drain_releases();
        let _ = self.emulate.release_all().await;
    }

    async fn end_driven(&mut self, src: &MachineId, notify: Option<LeaveReason>) {
        self.stop_raw().await;
        tracing::debug!(source = %src, ?notify, "driven session ended");
        self.release_target_side().await;
        let _ = self.emulate.leave().await;
        if let Some(net) = &self.net {
            if let Some(reason) = notify {
                if self.peers.get(src).is_some_and(|p| p.connected) {
                    net.send_to(src, Frame::Leave { session: self.active_session, reason });
                }
            }
            net.set_active(src, false);
        }
        self.driven_grace_until = Some(Instant::now() + DRIVEN_GRACE);
        self.focus = Focus::Local;
        self.touch_ui();
    }

    async fn panic(&mut self) {
        self.crossing = None;
        self.stop_raw().await;
        if let Focus::Remote(target) = self.focus.clone() {
            self.end_remote(&target, LeaveReason::Panic, None, true)
                .await;
        }
        if let Focus::Driven(src) = self.focus.clone() {
            self.end_driven(&src, Some(LeaveReason::Panic)).await;
        }
        if let Some(net) = &self.net {
            net.broadcast(Frame::Panic);
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
        if self.raw.pending_target.as_ref().is_some_and(|(peer, _)| *peer != stamp.writer) {
            self.stop_raw().await;
        }
        self.claim = Some(stamp);
        if let Focus::Remote(target) = self.focus.clone() {
            self.end_remote(&target, LeaveReason::SourceChanged, Some(self.last_local_pos), false).await;
        }
        if let Focus::Driven(source) = self.focus.clone() {
            if self.claim.as_ref().is_some_and(|claim| claim.writer != source) {
                self.end_driven(&source, Some(LeaveReason::SourceChanged)).await;
            }
        }
        self.touch_ui();
    }

    // ----- peer events & frames -----

    async fn on_peer_event(&mut self, ev: PeerEvent) {
        match ev {
            PeerEvent::Connected {
                id, hello, caps, ..
            } => {
                if self.raw.active || self.raw.preparing.is_some() {
                    if self.focus == Focus::Remote(id.clone()) {
                        self.end_remote(
                            &id,
                            LeaveReason::Reconfigured,
                            Some(self.last_local_pos),
                            false,
                        )
                        .await;
                    } else if self.focus == Focus::Driven(id.clone()) {
                        self.end_driven(&id, None).await;
                    }
                }
                if self
                    .raw
                    .pending_target
                    .as_ref()
                    .is_some_and(|(peer, _)| *peer == id)
                {
                    self.stop_raw().await;
                }
                tracing::info!(
                    peer = %id,
                    hostname = %hello.hostname,
                    displays = ?hello.displays,
                    "peer connected"
                );
                let peer = self.peers.entry(id.clone()).or_default();
                peer.hostname = Some(hello.hostname.clone());
                peer.error = None;
                peer.info = Some(hello);
                peer.caps = caps;
                peer.connected = true;
                peer.raw_generation = 0;
                peer.degraded = false;
                if let (Some(net), Some(doc)) = (&self.net, &self.layout) {
                    net.send_to(&id, Frame::LayoutSync(doc.clone()));
                }
                if let (Some(net), Some(claim)) = (&self.net, self.claim.clone()) {
                    net.send_to(&id, Frame::SourceClaim { stamp: claim });
                }
                self.send_master_state(&id);
                self.auto_place(&id);
                if self.settle_layout() {
                    self.bump_layout();
                }
                self.touch_ui();
            }
            PeerEvent::Frame(from, frame) => self.on_frame(from, frame).await,
            PeerEvent::Degraded(id) => {
                if self
                    .raw
                    .pending_target
                    .as_ref()
                    .is_some_and(|(peer, _)| *peer == id)
                {
                    self.stop_raw().await;
                }
                self.peers.entry(id.clone()).or_default().degraded = true;
                if self.focus == Focus::Remote(id.clone()) {
                    self.end_remote(&id, LeaveReason::Reconfigured, Some(self.last_local_pos), true)
                        .await;
                } else if self.focus == Focus::Driven(id.clone()) {
                    self.end_driven(&id, Some(LeaveReason::Reconfigured)).await;
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
                if self
                    .raw
                    .pending_target
                    .as_ref()
                    .is_some_and(|(peer, _)| *peer == id)
                {
                    self.stop_raw().await;
                }
                self.pending_fetches.disconnect(&id);
                tracing::debug!(peer = %id, reason, "peer disconnected");
                let peer = self.peers.entry(id.clone()).or_default();
                peer.error = Some(reason);
                peer.connected = false;
                peer.degraded = false;
                if self.focus == Focus::Remote(id.clone()) {
                    self.end_remote(&id, LeaveReason::Reconfigured, Some(self.last_local_pos), true)
                        .await;
                } else if self.focus == Focus::Driven(id.clone()) {
                    self.end_driven(&id, None).await;
                }
                self.touch_ui();
            }
            PeerEvent::Rtt(id, rtt) => {
                self.peers.entry(id).or_default().rtt_ms = Some(rtt);
                self.touch_ui();
            }
            PeerEvent::Rejected { id, reason } => {
                let peer = self.peers.entry(id).or_default();
                if !peer.connected {
                    peer.error = Some(reason);
                }
                self.touch_ui();
            }
        }
    }

    async fn on_frame(&mut self, from: Arc<MachineId>, frame: Frame) {
        match frame {
            Frame::RawPrepare { session, pos } => {
                self.prepare_raw_target((*from).clone(), session, pos).await
            }
            Frame::RawReady {
                session,
                port,
                ticket,
            } => self.raw_ready((*from).clone(), session, port, ticket).await,
            Frame::RawReject { session, reason } => {
                self.on_raw_event(crate::raw_transport::Event::Ended {
                    operation: self.raw.operation.clone(),
                    peer: (*from).clone(),
                    session,
                    error: reason,
                })
                .await
            }
            Frame::SourceClaim { stamp } => self.on_source_claim(stamp).await,
            Frame::LayoutSync(doc) => {
                self.layout_lamport = self.layout_lamport.max(doc.stamp.lamport);
                let newer = self.layout.as_ref().is_none_or(|l| doc.stamp > l.stamp);
                let previous = if newer { self.layout.replace(doc) } else { Some(doc) };
                let mut combined = false;
                if let Some(previous) = previous {
                    let current = self.layout.as_mut().expect("layout exists");
                    for (id, placement) in previous.machines {
                        if let std::collections::btree_map::Entry::Vacant(entry) = current.machines.entry(id) {
                            entry.insert(placement);
                            combined = true;
                        }
                    }
                }
                if newer || combined {
                    self.mark_cfg_dirty();
                    if self.settle_layout() || combined {
                        self.bump_layout();
                    } else if let (Some(net), Some(doc)) = (&self.net, &self.layout) {
                        net.broadcast(Frame::LayoutSync(doc.clone()));
                    }
                    self.touch_ui();
                }
            }
            Frame::MasterState { enabled } => {
                tracing::info!(peer = %from, enabled, "peer master switch");
                self.peers.entry((*from).clone()).or_default().master_off = !enabled;
                self.touch_ui();
            }
            Frame::MachineUpdate(info) => {
                let id = info.id.clone();
                if id != *from {
                    tracing::warn!(peer = %from, claimed = %id, "rejected machine update for another peer");
                    return;
                }
                tracing::info!(
                    peer = %id,
                    hostname = %info.hostname,
                    displays = ?info.displays,
                    "peer display geometry updated"
                );
                self.peers.entry(id.clone()).or_default().info = Some(info);
                self.auto_place(&id);
                if self.settle_layout() {
                    self.bump_layout();
                }
                self.touch_ui();
            }
            Frame::Enter { session, pos } => {
                self.on_enter((*from).clone(), session, pos).await;
            }
            Frame::Input { session, ev } => {
                let current = matches!(
                    &self.focus,
                    Focus::Driven(src) if src == from.as_ref() && session == self.active_session
                );
                if current && !self.raw.active {
                    self.target_ledger.observe(&ev);
                    if let Err(err) = self.emulate.inject(ev).await {
                        tracing::warn!(source = %from, error = %err, "target input emulation failed");
                        self.end_driven(from.as_ref(), Some(LeaveReason::CaptureLost)).await;
                    }
                }
            }
            Frame::Leave { session, reason } => {
                if self.raw.pending_target.as_ref() == Some(&(from.as_ref().clone(), session)) {
                    self.stop_raw().await;
                }
                if matches!(
                    &self.focus,
                    Focus::Driven(source)
                        if source == from.as_ref() && session == self.active_session
                ) {
                    self.end_driven(from.as_ref(), None).await;
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
            Frame::ReleaseAll => {
                if matches!(&self.focus, Focus::Driven(source) if source == from.as_ref()) {
                    if self.raw.active {
                        self.end_driven(from.as_ref(), Some(LeaveReason::Reconfigured)).await;
                    } else {
                        self.release_target_side().await;
                    }
                }
            }
            Frame::Panic => {
                self.stop_raw().await;
                match self.focus.clone() {
                Focus::Remote(target) => self.end_remote(&target, LeaveReason::Panic, None, true).await,
                Focus::Driven(source) => self.end_driven(&source, Some(LeaveReason::Panic)).await,
                Focus::Local => self.release_target_side().await,
                }
            },
            Frame::ClipOffer { id, stamp, mimes, inline_text } => {
                self.on_clip_offer((*from).clone(), id, stamp, mimes, inline_text).await;
            }
            Frame::ClipRequest { id, request, mime } => {
                self.on_clip_request((*from).clone(), id, request, mime);
            }
            Frame::ClipChunk { request, data, last } => {
                self.pending_fetches.chunk(&from, request, data, last);
            }
            Frame::ClipAbort { request, reason } => {
                tracing::debug!(peer = %from, request, reason, "clipboard request refused");
                self.pending_fetches.abort(&from, request);
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
        if self.raw.pending_target.is_some() { self.stop_raw().await; }
        if let Focus::Driven(old) = self.focus.clone() {
            self.stop_raw().await;
            if old == from {
                self.release_target_side().await;
            } else {
                self.end_driven(&old, Some(LeaveReason::SourceChanged)).await;
            }
        }
        if let Err(err) = self.emulate.enter(pos).await {
            tracing::warn!(source = %from, error = %err, "cannot enter target emulation");
            self.refuse_enter(&from, session);
            return;
        }
        tracing::debug!(source = %from, session, ?pos, "driven session started");
        self.focus = Focus::Driven(from.clone());
        self.active_session = session;
        self.target_ledger = HeldLedger::default();
        if let Some(net) = &self.net {
            net.set_active(&from, true);
        }
        self.touch_ui();
    }

    fn refuse_enter(&self, source: &MachineId, session: u64) {
        tracing::debug!(
            source = %source,
            session,
            master = self.cfg.master_enabled,
            self_enabled = self.machine_enabled(&self.self_info.id),
            source_enabled = self.machine_enabled(source),
            source_usable = self.peer_usable(source),
            claim = ?self.claim.as_ref().map(|c| c.writer.clone()),
            "refusing Enter"
        );
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
    /// rightmost machine at y=0, then rested against the cluster under the
    /// arrangement rules so it starts out touching, enabled.
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
        let placed: Vec<Body> = self
            .layout
            .as_ref()
            .expect("doc exists")
            .machines
            .iter()
            .map(|(mid, p)| Body::new(&self.displays_of(mid), p.offset))
            .collect();
        let rightmost = placed
            .iter()
            .filter_map(Body::bounds)
            .map(|bounds| bounds.right)
            .max()
            .unwrap_or(0) as i32;
        let start = Vec2I { x: rightmost, y: 0 };
        let newcomer = Body::new(&self.displays_of(id), start);
        let offset = match arrange::resolve(&newcomer, Vec2I::default(), &placed, &Rules::default(), None) {
            Some(placement) => Vec2I { x: start.x + placement.delta.x, y: start.y + placement.delta.y },
            None => start,
        };
        self.layout.as_mut().expect("doc exists").machines.insert(
            id.clone(),
            MachinePlacement { offset, enabled: true },
        );
        self.settle_layout();
        self.bump_layout();
    }

    /// Repair the doc into one connected, overlap-free cluster with the smallest moves:
    /// arrangements saved before the rules existed, display geometry that changed, or a
    /// commit computed against a stale topology. Only a machine that knows every placed
    /// machine's displays may do this, and bodies are ordered by id, so every peer that
    /// repairs the doc computes the same repair. True when anything moved.
    fn settle_layout(&mut self) -> bool {
        let Some(doc) = self.layout.as_ref() else {
            return false;
        };
        let ids: Vec<MachineId> = doc.machines.keys().cloned().collect();
        if ids.iter().any(|id| self.display_slice_of(id).is_empty()) {
            return false;
        }
        let bodies: Vec<Body> = ids
            .iter()
            .map(|id| Body::new(&self.displays_of(id), doc.machines[id].offset))
            .collect();
        let deltas = arrange::normalize(&bodies, &Rules::default());
        if deltas.iter().all(|delta| *delta == Vec2I::default()) {
            return false;
        }
        let doc = self.layout.as_mut().expect("doc exists");
        for (id, delta) in ids.iter().zip(deltas) {
            let placement = doc.machines.get_mut(id).expect("listed above");
            placement.offset.x += delta.x;
            placement.offset.y += delta.y;
        }
        true
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
            Command::SetInputSettings(settings) => {
                self.crossing = None;
                match settings.save(&self.data_dir) {
                    Ok(()) => {
                        if let Focus::Remote(target) = self.focus.clone() {
                            self.end_remote(
                                &target,
                                LeaveReason::Reconfigured,
                                Some(self.last_local_pos),
                                true,
                            )
                            .await;
                        }
                        self.cfg.edge_dwell_ms = match settings.crossing {
                            crate::input_settings::CrossingPolicy::Dwell { milliseconds } => {
                                milliseconds
                            }
                            _ => 0,
                        };
                        self.mark_cfg_dirty();
                        self.raw.settings = settings;
                        self.raw.error = None;
                    }
                    Err(error) => {
                        self.raw.error = Some(format!("Cannot save input settings: {error:#}"))
                    }
                }
                self.touch_ui();
            }
            Command::SelectTarget(target) => self.select_target(target).await,
            Command::Update { machine, action } => {
                if let Some(updates) = &mut self.updates {
                    updates.request(machine, action);
                }
                self.touch_ui();
            }
            Command::ExportDiagnostics => {
                match crate::diagnostics::export(&self.data_dir, &self.build_ui()) {
                    Ok(path) => {
                        self.diagnostics.export_path = Some(path.display().to_string());
                        self.diagnostics.export_error = None;
                    }
                    Err(error) => self.diagnostics.export_error = Some(format!("Cannot save diagnostics: {error:#}")),
                }
                self.touch_ui();
            }
            Command::SetMasterEnabled(on) => {
                self.cfg.master_enabled = on;
                self.mark_cfg_dirty();
                let ids: Vec<MachineId> = self.peers.keys().cloned().collect();
                for id in &ids {
                    self.send_master_state(id);
                }
                if !on {
                    self.crossing = None;
                    if self.raw.pending_target.is_some() { self.stop_raw().await; }
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
                        self.end_driven(&src, Some(LeaveReason::Reconfigured)).await;
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
            Command::SetArrangement(placements) => {
                let limit = splice_proto::validation::MAX_COORDINATE;
                if placements.iter().any(|(id, offset)| {
                    id.0.is_empty() || !(-limit..=limit).contains(&offset.x) || !(-limit..=limit).contains(&offset.y)
                }) {
                    tracing::warn!("rejected invalid workspace arrangement");
                    return;
                }
                self.ensure_doc();
                let doc = self.layout.as_mut().expect("doc exists");
                for (id, offset) in placements {
                    doc.machines
                        .entry(id)
                        .or_insert(MachinePlacement { offset: Vec2I { x: 0, y: 0 }, enabled: true })
                        .offset = offset;
                }
                self.settle_layout();
                self.bump_layout();
            }
            Command::SetSensitivity { link_key, factor } => {
                if !factor.is_finite() || !(0.25..=4.0).contains(&factor) {
                    tracing::warn!(factor, "rejected invalid pointer sensitivity");
                    return;
                }
                self.ensure_doc();
                self.layout.as_mut().expect("doc exists").sensitivity.insert(link_key, factor);
                self.bump_layout();
            }
            Command::SetClipboardSync(on) => {
                self.cfg.clipboard_sync = on;
                if !on {
                    self.live_offer = None;
                    self.pending_fetches.clear();
                    self.clipboard_jobs.abort_all();
                }
                self.mark_cfg_dirty();
                self.touch_ui();
            }
            Command::SetBackends(prefs) => {
                self.cfg.backends = prefs;
                self.save_config();
                if let Some(backends) = &self.backends {
                    let _ = backends.send(prefs);
                }
                if let Some(status) = &mut self.backend_status {
                    status.prefs = prefs;
                }
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
        if !self.peers.get(&from).is_some_and(|p| p.caps.iter().any(|c| c == caps::CLIPBOARD_V2)) {
            return;
        }
        if self.clip_seen.as_ref().is_some_and(|seen| stamp <= *seen) {
            return;
        }
        self.clip_seen = Some(stamp);
        self.last_applied_inline = inline_text.clone();
        if let Some(net) = &self.net {
            let fetch = self.pending_fetches.offer(net.clone(), from, id, mimes.clone());
            let _ = self.clipboard.set_remote_offer(ClipboardOffer { id, mimes, inline_text }, fetch).await;
        }
    }

    fn on_clip_request(&mut self, from: MachineId, id: u64, request: u64, mime: String) {
        let Some(net) = self.net.clone() else { return };
        let live = self.cfg.clipboard_sync
            && self.live_offer.as_ref().is_some_and(|(offer, mimes)| *offer == id && mimes.contains(&mime));
        if !live {
            net.send_to(&from, Frame::ClipAbort { request, reason: "clipboard offer is unavailable".into() });
            return;
        }
        if self.clipboard_jobs.len() >= 8 {
            net.send_to(&from, Frame::ClipAbort { request, reason: "too many clipboard reads in progress".into() });
            return;
        }
        let clipboard = self.clipboard.clone();
        self.clipboard_jobs.spawn(async move {
            let bytes = match tokio::time::timeout(crate::clipboard::FETCH_TIMEOUT, clipboard.read_local(&mime)).await {
                Ok(Ok(bytes)) if bytes.len() <= CLIP_MAX_TOTAL => bytes,
                result => {
                    let reason = match result {
                        Ok(Ok(_)) => "clipboard representation exceeds size limit".to_string(),
                        Ok(Err(error)) => error.to_string(),
                        Err(_) => "clipboard read timed out".to_string(),
                    };
                    net.send_to(&from, Frame::ClipAbort { request, reason });
                    return;
                }
            };
            if bytes.is_empty() {
                net.send_to(&from, Frame::ClipChunk { request, data: Vec::new(), last: true });
            } else {
                let count = bytes.len().div_ceil(CLIP_CHUNK);
                for (index, chunk) in bytes.chunks(CLIP_CHUNK).enumerate() {
                    if !net
                        .send_to_wait(
                            &from,
                            Frame::ClipChunk { request, data: chunk.to_vec(), last: index + 1 == count },
                        )
                        .await
                    {
                        break;
                    }
                }
            }
        });
    }

    // ----- recompute & helpers -----

    /// Recompute the derived state from layout + reachability. Focus validity,
    /// crossable links and OS barriers are all projections of authoritative state.
    async fn recompute(&mut self) {
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
        if self
            .crossing
            .as_ref()
            .is_some_and(|crossing| !self.links.contains(&crossing.link))
        {
            self.crossing = None;
            self.touch_ui();
        }
        self.reconcile_focus().await;
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
        if specs != self.armed_specs {
            tracing::info!(
                edges = ?specs,
                crossable = ?self.links.iter().filter(|l| l.from == self.self_info.id).map(|l| l.to.clone()).collect::<Vec<_>>(),
                "armed edges updated"
            );
            self.armed_specs = specs.clone();
        }
        if let Err(error) = self.capture.set_edges(specs).await {
            tracing::warn!(%error, "cannot arm capture edges");
            self.health.capture = Some(error.to_string());
            if let Focus::Remote(target) = self.focus.clone() {
                self.end_remote(&target, LeaveReason::CaptureLost, Some(self.last_local_pos), true).await;
            }
            self.touch_ui();
        }
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
                    && self.claim.as_ref().is_some_and(|c| c.writer == self.self_info.id)
                    && self.return_path_exists(&target);
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
                    self.end_driven(&source, Some(LeaveReason::Reconfigured)).await;
                }
            }
        }
    }

    fn return_path_exists(&self, from: &MachineId) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from.clone()];
        while let Some(node) = stack.pop() {
            if node == self.self_info.id {
                return true;
            }
            if !seen.insert(node.clone()) {
                continue;
            }
            stack.extend(
                self.links
                    .iter()
                    .filter(|link| link.from == node)
                    .map(|link| link.to.clone()),
            );
        }
        false
    }

    fn peer_usable(&self, id: &MachineId) -> bool {
        self.peers
            .get(id)
            .is_some_and(|p| p.connected && !p.degraded && !p.master_off)
    }

    fn send_master_state(&self, id: &MachineId) {
        let supports = self
            .peers
            .get(id)
            .is_some_and(|p| p.connected && p.caps.iter().any(|c| c == caps::MASTER_V1));
        if let (true, Some(net)) = (supports, &self.net) {
            net.send_to(id, Frame::MasterState { enabled: self.cfg.master_enabled });
        }
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
        let delay = if self.crossing.is_some() { Duration::from_millis(16) } else { UI_DEBOUNCE };
        let deadline = Instant::now() + delay;
        if self.ui_deadline.is_none_or(|current| deadline < current) {
            self.ui_deadline = Some(deadline);
        }
    }

    fn mark_cfg_dirty(&mut self) {
        self.cfg_deadline = Some(Instant::now() + CFG_DEBOUNCE);
    }

    fn save_config(&mut self) {
        self.cfg.layout = self.layout.clone();
        match config::save(&self.data_dir, &self.cfg) {
            Ok(()) => self.config_error = None,
            Err(error) => {
                tracing::warn!(%error, "config save failed");
                self.config_error = Some(format!("Settings could not be saved: {error}"));
                self.cfg_deadline = Some(Instant::now() + Duration::from_secs(5));
            }
        }
        self.touch_ui();
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
        let mut diagnostics = self.diagnostics.clone();
        if let Some(net) = &self.net {
            diagnostics.peers = net.diagnostics();
        }
        UiState {
            crossing_progress: self.crossing.as_ref().map(|c| crate::ui_state::UiCrossing {
                from: c.link.from.clone(),
                to: c.link.to.clone(),
                progress: c.progress,
                side: c.link.side,
                position: c.pos,
            }),
            input_settings: self.raw.settings.clone(),
            input_error: self.raw.error.clone(),
            raw_active: self.raw.active,
            preparing_input: self.raw.preparing.clone(),
            updates: self
                .updates
                .as_ref()
                .map(|u| u.snapshot())
                .unwrap_or_default(),
            restart_requested: self.restart_requested,
            build: splice_proto::BuildInfo::current(),
            diagnostics,
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
            config_error: self.config_error.clone(),
            connection_errors: {
                let mut errors: Vec<_> = self
                    .peers
                    .iter()
                    .filter_map(|(id, peer)| {
                        peer.error
                            .as_ref()
                            .map(|error| format!("{}: {error}", peer.hostname.as_deref().unwrap_or(&id.0)))
                    })
                    .collect();
                errors.sort();
                errors
            },
            backends: self.backend_status.clone(),
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
