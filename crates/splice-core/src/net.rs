//! Peer networking: listener, dialer, per-peer session tasks, handshake, heartbeats.
//!
//! Rules (DESIGN.md):
//! - Listener binds to the Tailscale IP ONLY, port SPLICE_PORT, TCP_NODELAY everywhere.
//! - Inbound: WhoIs(remote ip:port) → authorize() → else close silently.
//! - Dial dedupe: the machine with the LEXICOGRAPHICALLY SMALLER MachineId dials; the
//!   larger side only accepts (but accepts either if its own preferred connection is
//!   not yet up; on conflict keep the smaller-dialer's connection, drop the other).
//! - Handshake: dialer sends Hello first; listener replies Welcome. Version = clamp,
//!   caps = intersection. Disjoint version ranges → Bye + close.
//! - Heartbeat: Ping every 1 s while a session with that peer is active, 5 s idle;
//!   3 consecutive missed → mark peer Degraded (engine releases input) but KEEP the
//!   socket and keep pinging; recover on next Pong. Reconnect (if socket dies) with
//!   1 s→30 s backoff while tailscale says the peer is online.
//! - RTT from Ping/Pong t_us echo; report to engine for the UI.
//!
//! Each peer session task owns its socket; frames to the engine via mpsc, frames from
//! the engine via a per-peer mpsc. The engine task never touches sockets.

mod session;

use futures::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use splice_proto::{Frame, MachineId};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};

/// Engine → session commands.
#[derive(Debug)]
pub enum PeerCmd {
    Send(Frame),
    /// Close gracefully (Bye) and stop the task.
    Shutdown(String),
}

/// Session → engine notifications.
#[derive(Debug)]
pub enum PeerEvent {
    /// Handshake complete; peer identity + negotiated caps.
    Connected {
        id: MachineId,
        hello: splice_proto::MachineInfo,
        caps: Vec<String>,
        addr: SocketAddr,
    },
    Frame(MachineId, Frame),
    /// 3 missed heartbeats — treat as input-unsafe but keep the link.
    Degraded(MachineId),
    /// Recovered from Degraded.
    Healthy(MachineId, f64),
    /// Socket gone; the manager will redial per backoff if still online.
    Disconnected(MachineId, String),
    /// Measured RTT update (ms).
    Rtt(MachineId, f64),
}

/// Handle owned by the engine for one connected peer.
#[derive(Debug)]
pub struct PeerHandle {
    pub id: MachineId,
    pub cmd: mpsc::UnboundedSender<PeerCmd>,
}

/// Tailscale LocalAPI surface the net layer needs. `splice_tailscale::Client` implements
/// it; integration tests substitute a fake (the real client needs a live tailscaled).
/// Uses boxed futures so implementors don't need an async-trait dependency.
#[doc(hidden)]
pub trait TsApi: Send + Sync + 'static {
    fn status(&self) -> BoxFuture<'_, splice_tailscale::Result<splice_tailscale::Status>>;
    fn whois(
        &self,
        addr: SocketAddr,
    ) -> BoxFuture<'_, splice_tailscale::Result<splice_tailscale::WhoIs>>;
}

impl TsApi for splice_tailscale::Client {
    fn status(&self) -> BoxFuture<'_, splice_tailscale::Result<splice_tailscale::Status>> {
        Box::pin(splice_tailscale::Client::status(self))
    }
    fn whois(
        &self,
        addr: SocketAddr,
    ) -> BoxFuture<'_, splice_tailscale::Result<splice_tailscale::WhoIs>> {
        Box::pin(splice_tailscale::Client::whois(self, addr))
    }
}

/// Tunables for the net layer. Defaults are the production values from DESIGN.md;
/// tests shrink timings and gate Pong replies. Not part of the stable API.
#[doc(hidden)]
#[derive(Clone)]
pub struct NetOpts {
    /// Advertised protocol range (Hello.proto_min/proto_max).
    pub proto_min: u16,
    pub proto_max: u16,
    /// Per-peer dial port overrides; peers missing from the map use SPLICE_PORT.
    pub dial_ports: Arc<std::sync::RwLock<HashMap<MachineId, u16>>>,
    /// Ping cadence while a session with the peer is active.
    pub active_hb: Duration,
    /// Ping cadence while idle.
    pub idle_hb: Duration,
    /// Consecutive missed Pongs before PeerEvent::Degraded.
    pub max_misses: u32,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub dial_timeout: Duration,
    pub handshake_timeout: Duration,
    /// How long a fetched tailscale status may be reused for inbound auth.
    pub status_ttl: Duration,
    /// Test hook: when false, sessions ignore Ping (simulates a silent peer).
    pub answer_pings: Arc<AtomicBool>,
    /// Test hook: dial targets even when the dedupe rule says we only listen.
    pub force_dial: bool,
}

impl Default for NetOpts {
    fn default() -> Self {
        Self {
            proto_min: 1,
            proto_max: splice_proto::PROTO_VERSION,
            dial_ports: Arc::new(std::sync::RwLock::new(HashMap::new())),
            active_hb: Duration::from_secs(1),
            idle_hb: Duration::from_secs(5),
            max_misses: 3,
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            dial_timeout: Duration::from_secs(2),
            handshake_timeout: Duration::from_secs(5),
            status_ttl: Duration::from_secs(5),
            answer_pings: Arc::new(AtomicBool::new(true)),
            force_dial: false,
        }
    }
}

/// Spawn the listener + dialer manager. Emits PeerEvents; the engine hands back dial
/// targets whenever discovery updates.
pub struct NetManager {
    pub events: mpsc::UnboundedReceiver<PeerEvent>,
    /// Actual listener address (port known after binding port 0 in tests).
    #[doc(hidden)]
    pub local_addr: SocketAddr,
}

impl NetManager {
    /// `self_info` describes this machine for Hellos (the engine keeps it updated via
    /// [`NetControl::update_self`]). `bind_ip` is our tailscale IP.
    pub async fn spawn(
        self_info: splice_proto::MachineInfo,
        bind_ip: std::net::IpAddr,
        ts: splice_tailscale::Client,
    ) -> anyhow::Result<(NetManager, NetControl)> {
        Self::spawn_with(
            self_info,
            SocketAddr::new(bind_ip, splice_proto::SPLICE_PORT),
            Arc::new(ts),
            NetOpts::default(),
        )
        .await
    }

    /// Bind-address / port / timing override entry point (loopback tests, harnesses).
    #[doc(hidden)]
    pub async fn spawn_with(
        self_info: splice_proto::MachineInfo,
        bind: SocketAddr,
        ts: Arc<dyn TsApi>,
        opts: NetOpts,
    ) -> anyhow::Result<(NetManager, NetControl)> {
        let listener = TcpListener::bind(bind).await?;
        let local_addr = listener.local_addr()?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(NetControlInner {
            self_info: RwLock::new(self_info),
            targets: RwLock::new(HashMap::new()),
            peers: RwLock::new(HashMap::new()),
            dialing: Mutex::new(HashSet::new()),
            events: events_tx,
            targets_changed: Notify::new(),
            ts,
            opts,
            status_cache: tokio::sync::Mutex::new(None),
            next_seq: AtomicU64::new(1),
        });
        let manager = NetManager { events: events_rx, local_addr };
        let control = NetControl { inner: inner.clone() };
        tokio::spawn(accept_loop(inner, listener));
        Ok((manager, control))
    }
}

/// Engine-side control for the net layer. Cheap to clone; methods enqueue, never block.
#[derive(Clone)]
pub struct NetControl {
    pub(crate) inner: std::sync::Arc<NetControlInner>,
}

pub(crate) struct NetControlInner {
    self_info: RwLock<splice_proto::MachineInfo>,
    targets: RwLock<HashMap<MachineId, IpAddr>>,
    peers: RwLock<HashMap<MachineId, session::PeerSlot>>,
    /// Peers with a live dialer task (guards against duplicate dial loops).
    dialing: Mutex<HashSet<MachineId>>,
    events: mpsc::UnboundedSender<PeerEvent>,
    /// Wakes dialer tasks early when the target set changes.
    targets_changed: Notify,
    ts: Arc<dyn TsApi>,
    opts: NetOpts,
    /// Last tailscale status + fetch time, for inbound WhoIs authorization.
    status_cache: tokio::sync::Mutex<Option<(Instant, splice_tailscale::Status)>>,
    next_seq: AtomicU64,
}

impl NetControl {
    /// Replace the set of peers we should be connected to (from discovery): id + IP.
    /// The manager dials (respecting the smaller-id-dials rule), redials with backoff
    /// while a target remains listed, and drops connections to unlisted peers.
    pub fn update_dial_targets(&self, targets: Vec<(MachineId, std::net::IpAddr)>) {
        let inner = &*self.inner;
        let self_id = inner.self_info.read().id.clone();
        let mut removed = Vec::new();
        {
            let mut guard = inner.targets.write();
            for old in guard.keys() {
                if !targets.iter().any(|(id, _)| id == old) {
                    removed.push(old.clone());
                }
            }
            guard.clear();
            guard.extend(targets.iter().cloned());
        }
        for id in &removed {
            if let Some(slot) = inner.peers.read().get(id) {
                let _ = slot.cmd.send(PeerCmd::Shutdown("unlisted".into()));
            }
        }
        for (id, _) in &targets {
            let we_dial = inner.opts.force_dial || self_id < *id;
            if we_dial && !removed.contains(id) {
                let mut dialing = inner.dialing.lock();
                if dialing.insert(id.clone()) {
                    tokio::spawn(dial_loop(self.inner.clone(), id.clone()));
                }
            }
        }
        inner.targets_changed.notify_waiters();
    }

    /// Refresh the MachineInfo used in future Hellos AND broadcast MachineUpdate to
    /// connected peers (display hotplug).
    pub fn update_self(&self, info: splice_proto::MachineInfo) {
        *self.inner.self_info.write() = info.clone();
        self.broadcast(Frame::MachineUpdate(info));
    }

    /// Send to one connected peer. Returns false if not connected (frame dropped).
    pub fn send_to(&self, id: &MachineId, frame: Frame) -> bool {
        let peers = self.inner.peers.read();
        match peers.get(id) {
            Some(slot) => slot.cmd.send(PeerCmd::Send(frame)).is_ok(),
            None => false,
        }
    }

    /// Send to every connected peer.
    pub fn broadcast(&self, frame: Frame) {
        let peers = self.inner.peers.read();
        for slot in peers.values() {
            let _ = slot.cmd.send(PeerCmd::Send(frame.clone()));
        }
    }

    /// Heartbeat cadence hint: active session with this peer → 1 s pings; idle → 5 s.
    pub fn set_active(&self, id: &MachineId, active: bool) {
        if let Some(slot) = self.inner.peers.read().get(id) {
            slot.active.store(active, Ordering::Relaxed);
        }
    }
}

/// Accept inbound connections; each authorized socket becomes a listener-role session.
async fn accept_loop(inner: Arc<NetControlInner>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((sock, remote)) => {
                let inner = inner.clone();
                tokio::spawn(async move {
                    if let Some(peer_id) = authorize_inbound(&inner, remote).await {
                        // Bind the transport identity (WhoIs) to the claimed identity
                        // (Hello.machine.id): a same-user machine cannot impersonate
                        // another node.
                        session::run(inner, sock, session::Role::Listener, Some(peer_id)).await;
                    }
                    // Unauthorized: close silently (drop).
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "net: accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// WhoIs the remote and authorize against a fresh-enough tailscale status.
/// Returns the peer's WhoIs identity (StableID) on success.
async fn authorize_inbound(inner: &Arc<NetControlInner>, remote: SocketAddr) -> Option<MachineId> {
    let status = cached_status(inner).await?;
    let who = inner.ts.whois(remote).await.ok()?;
    match splice_tailscale::authorize(&status, &who) {
        splice_tailscale::AuthDecision::Peer(id) => Some(MachineId(id)),
        _ => None,
    }
}

/// Tailscale status reused for at most opts.status_ttl (5 s production).
async fn cached_status(inner: &Arc<NetControlInner>) -> Option<splice_tailscale::Status> {
    let mut cache = inner.status_cache.lock().await;
    if let Some((fetched, status)) = &*cache {
        if fetched.elapsed() <= inner.opts.status_ttl {
            return Some(status.clone());
        }
    }
    let status = inner.ts.status().await.ok()?;
    *cache = Some((Instant::now(), status.clone()));
    Some(status)
}

/// Dial a listed target; redial with exponential backoff + jitter until unlisted.
async fn dial_loop(inner: Arc<NetControlInner>, id: MachineId) {
    let mut backoff = inner.opts.backoff_min;
    loop {
        let ip = match inner.targets.read().get(&id) {
            Some(ip) => *ip,
            None => break,
        };
        let port = inner
            .opts
            .dial_ports
            .read()
            .expect("dial_ports poisoned")
            .get(&id)
            .copied()
            .unwrap_or(splice_proto::SPLICE_PORT);
        let attempt =
            tokio::time::timeout(inner.opts.dial_timeout, TcpStream::connect((ip, port))).await;
        match attempt {
            Ok(Ok(sock)) => {
                let connected =
                    session::run(inner.clone(), sock, session::Role::Dialer, Some(id.clone()))
                        .await;
                if connected {
                    backoff = inner.opts.backoff_min;
                }
            }
            Ok(Err(e)) => tracing::debug!(peer = %id, error = %e, "net: dial failed"),
            Err(_) => tracing::debug!(peer = %id, "net: dial timed out"),
        }
        if !inner.targets.read().contains_key(&id) {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(jittered(backoff)) => {}
            _ = inner.targets_changed.notified() => {}
        }
        backoff = (backoff * 2).min(inner.opts.backoff_max);
    }
    inner.dialing.lock().remove(&id);
}

/// No `rand` dependency in the workspace: xorshift seeded once from the clock is
/// plenty for backoff jitter. Spread is [d, d + d/4).
fn jittered(d: Duration) -> Duration {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    let spread = (d.as_nanos() / 4) as u64;
    d + Duration::from_nanos(if spread > 0 { x % spread } else { 0 })
}
