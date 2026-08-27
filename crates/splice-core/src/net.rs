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

use splice_proto::{Frame, MachineId};
use std::net::SocketAddr;
use tokio::sync::mpsc;

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

/// Spawn the listener + dialer manager. Emits PeerEvents; the engine hands back dial
/// targets whenever discovery updates.
pub struct NetManager {
    pub events: mpsc::UnboundedReceiver<PeerEvent>,
    // implemented by the net agent: internal task handles, dial-target updates, etc.
}

impl NetManager {
    /// `self_info` describes this machine for Hellos. `bind_ip` is our tailscale IP.
    pub async fn spawn(
        _self_info: splice_proto::MachineInfo,
        _bind_ip: std::net::IpAddr,
        _ts: splice_tailscale::Client,
    ) -> anyhow::Result<(NetManager, NetControl)> {
        todo!("implemented by net agent")
    }
}

/// Engine-side control for the net layer.
pub struct NetControl {
    // implemented by net agent:
    // - update_dial_targets(Vec<(MachineId, IpAddr)>)
    // - peer_handle(&MachineId) -> Option<PeerHandle-ish sender>
    // - broadcast(Frame)
    // - set_active(MachineId, bool)  // heartbeat cadence hint
}
