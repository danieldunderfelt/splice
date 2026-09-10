use super::state::Update;
use anyhow::{anyhow, ensure, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use splice_platform::raw::clock::now_us;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle};

pub(crate) const MAX_DATAGRAM: usize = 1200;
pub(crate) type Token = [u8; 16];
const MAGIC: [u8; 4] = *b"SPI6";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Packet {
    Hello {
        sent_us: u64,
    },
    HelloAck {
        sent_us: u64,
    },
    Data {
        session: u64,
        sent_us: u64,
        update: Update,
    },
    Ack {
        session: u64,
        next: u64,
        serial: u64,
        sent_us: u64,
    },
}

struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Route {
    remote: SocketAddr,
    packets: mpsc::Sender<Datagram>,
}

struct Datagram {
    address: SocketAddr,
    received_us: u64,
    bytes: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct Endpoint {
    socket: Arc<UdpSocket>,
    routes: Arc<Mutex<HashMap<Token, Route>>>,
    _task: Arc<Task>,
}

pub(crate) struct Connection {
    send_drops: AtomicU64,
    endpoint: Endpoint,
    remote: SocketAddr,
    local_token: Token,
    remote_token: Token,
    incoming: mpsc::Receiver<Datagram>,
}

pub(crate) struct Incoming {
    pub address: SocketAddr,
    pub received_us: u64,
    pub packet: Packet,
}

impl Endpoint {
    pub(crate) async fn bind(address: SocketAddr) -> Result<Self> {
        let socket = Arc::new(
            UdpSocket::bind(address)
                .await
                .context("cannot bind UDP input socket")?,
        );
        let routes = Arc::new(Mutex::new(HashMap::<Token, Route>::new()));
        let receive_socket = socket.clone();
        let receive_routes = routes.clone();
        let task = tokio::spawn(async move {
            let mut bytes = [0; MAX_DATAGRAM + 1];
            loop {
                let (length, address) = match receive_socket.recv_from(&mut bytes).await {
                    Ok(received) => received,
                    Err(error) => {
                        tracing::warn!(%error, "UDP input receive failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                if !(21..=MAX_DATAGRAM).contains(&length) || bytes[..4] != MAGIC {
                    continue;
                }
                let token: Token = bytes[4..20].try_into().expect("fixed header length");
                let routes = receive_routes.lock();
                let Some(route) = routes.get(&token) else {
                    continue;
                };
                if address.ip() != route.remote.ip()
                    || (route.remote.port() != 0 && address != route.remote)
                {
                    continue;
                }
                let _ = route.packets.try_send(Datagram {
                    address,
                    received_us: now_us(),
                    bytes: bytes[20..length].to_vec(),
                });
            }
        });
        Ok(Self {
            socket,
            routes,
            _task: Arc::new(Task(task)),
        })
    }

    pub(crate) fn address(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    pub(crate) fn subscribe(
        &self,
        remote: SocketAddr,
        local_token: Token,
        remote_token: Token,
    ) -> Result<Connection> {
        let (packets, incoming) = mpsc::channel(256);
        let mut routes = self.routes.lock();
        ensure!(
            !routes.contains_key(&local_token),
            "duplicate UDP input authorization"
        );
        routes.insert(local_token, Route { remote, packets });
        Ok(Connection {
            send_drops: AtomicU64::new(0),
            endpoint: self.clone(),
            remote,
            local_token,
            remote_token,
            incoming,
        })
    }
}

impl Connection {
    pub(crate) fn pin(&mut self, address: SocketAddr) -> Result<()> {
        ensure!(
            self.remote.ip() == address.ip() && (self.remote.port() == 0 || self.remote == address),
            "UDP input peer address changed"
        );
        self.remote = address;
        self.endpoint
            .routes
            .lock()
            .get_mut(&self.local_token)
            .ok_or_else(|| anyhow!("UDP input route closed"))?
            .remote = address;
        Ok(())
    }

    pub(crate) async fn send(&self, packet: &Packet) -> Result<usize> {
        ensure!(
            self.remote.port() != 0,
            "UDP input peer has not been authenticated"
        );
        let mut bytes = Vec::with_capacity(256);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&self.remote_token);
        bytes.extend_from_slice(&postcard::to_allocvec(packet)?);
        ensure!(
            bytes.len() <= MAX_DATAGRAM,
            "input datagram exceeds the path MTU limit"
        );
        let result = tokio::time::timeout(
            super::INPUT_TIMEOUT,
            self.endpoint.socket.send_to(&bytes, self.remote),
        )
        .await
        .context("UDP input write timed out")?;
        let sent = match result {
            Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                let dropped = self.send_drops.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped.is_power_of_two() {
                    tracing::warn!(
                        dropped,
                        "UDP interface queue dropped input datagrams; awaiting delivery recovery"
                    );
                }
                return Ok(0);
            }
            result => result?,
        };
        ensure!(sent == bytes.len(), "incomplete input datagram write");
        Ok(sent)
    }

    fn decode(datagram: Datagram) -> Result<Incoming> {
        let (packet, extra) = postcard::take_from_bytes(&datagram.bytes)?;
        ensure!(extra.is_empty(), "trailing UDP input bytes");
        Ok(Incoming {
            address: datagram.address,
            received_us: datagram.received_us,
            packet,
        })
    }

    pub(crate) async fn receive(&mut self) -> Result<Incoming> {
        Self::decode(
            self.incoming
                .recv()
                .await
                .ok_or_else(|| anyhow!("UDP input receiver stopped"))?,
        )
    }

    pub(crate) async fn probe(&mut self) -> Result<()> {
        let mut timer = tokio::time::interval(std::time::Duration::from_millis(50));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut sent = std::collections::VecDeque::new();
        let exchange = async {
            for _ in 0..4 {
                let stamp = now_us();
                self.send(&Packet::Hello { sent_us: stamp }).await?;
                sent.push_back(stamp);
            }
            let mut acknowledged = 0;
            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        let stamp = now_us();
                        self.send(&Packet::Hello { sent_us: stamp }).await?;
                        sent.push_back(stamp);
                        if sent.len() > 100 { sent.pop_front(); }
                    }
                    packet = self.receive() => {
                        match packet?.packet {
                            Packet::Hello { sent_us } => { self.send(&Packet::HelloAck { sent_us }).await?; }
                            Packet::HelloAck { sent_us } if sent.contains(&sent_us) => {
                                sent.retain(|stamp| *stamp != sent_us);
                                acknowledged += 1;
                                if acknowledged >= 4 { return Ok(()); }
                            }
                            _ => {}
                        }
                    }
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), exchange)
            .await
            .context("UDP input handshake timed out; allow UDP input on the Tailscale interface")?
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.endpoint.routes.lock().remove(&self.local_token);
    }
}

pub(crate) fn token() -> Result<Token> {
    let mut token = [0; 16];
    getrandom::fill(&mut token)
        .map_err(|error| anyhow!("cannot generate input authorization: {error}"))?;
    Ok(token)
}

pub(crate) async fn client(
    bind: IpAddr,
    remote: SocketAddr,
    ticket: [u8; 32],
) -> Result<Connection> {
    let endpoint = Endpoint::bind(SocketAddr::new(bind, 0)).await?;
    endpoint.subscribe(
        remote,
        ticket[16..].try_into().expect("ticket half"),
        ticket[..16].try_into().expect("ticket half"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokens_addresses_and_datagram_size_are_checked_before_delivery() {
        let endpoint = Endpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let authorized = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let foreign = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut connection = endpoint
            .subscribe(authorized.local_addr().unwrap(), [4; 16], [5; 16])
            .unwrap();
        let address = endpoint.address().unwrap();
        let mut valid = b"SPI6".to_vec();
        valid.extend_from_slice(&[4; 16]);
        valid.extend_from_slice(&postcard::to_allocvec(&Packet::Hello { sent_us: 123 }).unwrap());
        let mut wrong_token = valid.clone();
        wrong_token[4] = 9;
        let mut oversized = valid.clone();
        oversized.resize(MAX_DATAGRAM + 1, 0);
        foreign.send_to(&valid, address).await.unwrap();
        for invalid in [&wrong_token[..], &oversized[..], &valid[..20]] {
            authorized.send_to(invalid, address).await.unwrap();
        }
        authorized.send_to(&valid, address).await.unwrap();
        let packet =
            tokio::time::timeout(std::time::Duration::from_millis(200), connection.receive())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(packet.packet, Packet::Hello { sent_us: 123 }));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), connection.receive())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_new_authorization_rejects_packets_from_the_previous_connection() {
        let endpoint = Endpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let remote = Endpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let old = endpoint
            .subscribe(remote.address().unwrap(), [1; 16], [2; 16])
            .unwrap();
        let stale = remote
            .subscribe(endpoint.address().unwrap(), [2; 16], [1; 16])
            .unwrap();
        drop(old);
        let mut fresh = endpoint
            .subscribe(remote.address().unwrap(), [3; 16], [4; 16])
            .unwrap();
        let current = remote
            .subscribe(endpoint.address().unwrap(), [4; 16], [3; 16])
            .unwrap();
        stale.send(&Packet::Hello { sent_us: 1 }).await.unwrap();
        current.send(&Packet::Hello { sent_us: 77 }).await.unwrap();
        assert!(matches!(
            fresh.receive().await.unwrap().packet,
            Packet::Hello { sent_us: 77 }
        ));
    }
}
