//! Tailscale LocalAPI client: peer discovery, WhoIs authentication, status polling.
//!
//! READ `docs/research/tailscale.md` BEFORE IMPLEMENTING — socket discovery differs per
//! platform and per Tailscale variant, and the WhoIs self-resolution footgun is real.
//!
//! Implementation: hand-rolled one-shot HTTP/1.1 GET over UnixStream (Linux) or
//! loopback TCP with Basic auth (macOS App Store/standalone variants), serde_json parsing
//! with `#[serde(default)]` everywhere (the status format is officially unstable).

use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, thiserror::Error)]
pub enum TsError {
    #[error("tailscaled unreachable: {0}")]
    Unreachable(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http status {0}")]
    Http(u16),
    #[error("peer not found for {0}")]
    PeerNotFound(SocketAddr),
}

pub type Result<T> = std::result::Result<T, TsError>;

/// Where/how to reach the LocalAPI (resolved once, re-resolved on failure).
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// Linux: /var/run/tailscale/tailscaled.sock
    Unix(std::path::PathBuf),
    /// macOS GUI variants: 127.0.0.1:port with `Authorization: Basic base64(":" + token)`.
    Loopback { port: u16, token: String },
}

/// A tailnet node (self or peer) as we consume it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Node {
    #[serde(rename = "ID")]
    pub stable_id: String,
    #[serde(rename = "HostName")]
    pub hostname: String,
    #[serde(rename = "DNSName")]
    pub dns_name: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "UserID")]
    pub user_id: u64,
    #[serde(rename = "TailscaleIPs")]
    pub ips: Vec<IpAddr>,
    #[serde(rename = "Online")]
    pub online: bool,
    /// Non-empty "ip:port" = direct connection; empty = DERP or idle.
    #[serde(rename = "CurAddr")]
    pub cur_addr: String,
    #[serde(rename = "Relay")]
    pub relay: String,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub self_node: Node,
    pub peers: Vec<Node>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WhoIsUser {
    #[serde(rename = "ID")]
    pub id: u64,
    #[serde(rename = "LoginName")]
    pub login_name: String,
}

#[derive(Clone, Debug, Default)]
pub struct WhoIs {
    pub node_stable_id: String,
    pub user: WhoIsUser,
}

/// LocalAPI client. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Client {
    endpoint: Endpoint,
}

impl Client {
    /// Discover the LocalAPI endpoint for this machine (see docs/research/tailscale.md:
    /// Linux unix socket → macOS group-container sameuserproof glob → /Library/Tailscale).
    pub async fn discover() -> Result<Client> {
        let endpoint = discovery::discover_endpoint().await?;
        Ok(Client { endpoint })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// GET /localapi/v0/status
    pub async fn status(&self) -> Result<Status> {
        http::get_status(&self.endpoint).await
    }

    /// GET /localapi/v0/whois?addr=ip:port — MUST be passed ip:port, never a bare IP.
    pub async fn whois(&self, addr: SocketAddr) -> Result<WhoIs> {
        http::get_whois(&self.endpoint, addr).await
    }
}

/// Authentication verdict for an inbound connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthDecision {
    /// Same-user tailnet peer; carry on. Value = peer StableID.
    Peer(String),
    /// WhoIs resolved to this machine itself (local process dialing our tailscale IP).
    RejectSelf,
    /// Different user / not resolvable.
    RejectUnknown,
}

/// Decide whether an inbound connection is an authorized peer.
pub fn authorize(status: &Status, who: &WhoIs) -> AuthDecision {
    if who.node_stable_id == status.self_node.stable_id {
        return AuthDecision::RejectSelf;
    }
    if who.user.id != status.self_node.user_id {
        return AuthDecision::RejectUnknown;
    }
    AuthDecision::Peer(who.node_stable_id.clone())
}

mod discovery {
    use super::*;

    pub async fn discover_endpoint() -> Result<Endpoint> {
        // Implemented by the tailscale agent task; see docs/research/tailscale.md.
        Err(TsError::Unreachable("endpoint discovery not yet implemented".into()))
    }
}

mod http {
    use super::*;

    pub async fn get_status(_ep: &Endpoint) -> Result<Status> {
        Err(TsError::Unreachable("not yet implemented".into()))
    }

    pub async fn get_whois(_ep: &Endpoint, addr: SocketAddr) -> Result<WhoIs> {
        let _ = addr;
        Err(TsError::Unreachable("not yet implemented".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_with(self_id: &str, user: u64) -> Status {
        Status {
            self_node: Node {
                stable_id: self_id.into(),
                user_id: user,
                ..Default::default()
            },
            peers: vec![],
        }
    }

    #[test]
    fn authorize_rejects_self_and_foreign_users() {
        let st = status_with("SELF1", 42);
        let self_who = WhoIs {
            node_stable_id: "SELF1".into(),
            user: WhoIsUser { id: 42, login_name: "me".into() },
        };
        assert_eq!(authorize(&st, &self_who), AuthDecision::RejectSelf);

        let foreign = WhoIs {
            node_stable_id: "PEER9".into(),
            user: WhoIsUser { id: 7, login_name: "someone".into() },
        };
        assert_eq!(authorize(&st, &foreign), AuthDecision::RejectUnknown);

        let peer = WhoIs {
            node_stable_id: "PEER9".into(),
            user: WhoIsUser { id: 42, login_name: "me".into() },
        };
        assert_eq!(authorize(&st, &peer), AuthDecision::Peer("PEER9".into()));
    }
}
