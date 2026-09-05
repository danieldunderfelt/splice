use crate::net::TsApi;
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use splice_platform::raw::RawEmulate;
use splice_proto::{raw::RawReport, MachineId};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::mpsc,
};

const IO_TIMEOUT: Duration = Duration::from_millis(750);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PACKET: usize = 32768;
pub const RAW_PORT: u16 = 41719;

#[derive(Serialize, Deserialize)]
enum Packet {
    Open { session: u64, ticket: [u8; 32] },
    Accepted,
    Report { session: u64, report: RawReport },
    Ping(u64),
    Pong(u64),
}

pub struct Reservation {
    listener: TcpListener,
    pub port: u16,
    pub ticket: [u8; 32],
}

pub enum Event {
    Prepared {
        operation: Arc<()>,
        peer: MachineId,
        session: u64,
        pos: splice_proto::Vec2,
        result: std::result::Result<Reservation, String>,
    },
    Connected {
        operation: Arc<()>,
        peer: MachineId,
        session: u64,
        stream: TcpStream,
    },
    Ended {
        operation: Arc<()>,
        peer: MachineId,
        session: u64,
        error: String,
    },
}

impl Event {
    pub fn belongs_to(&self, current: &Arc<()>) -> bool {
        let operation = match self {
            Self::Prepared { operation, .. }
            | Self::Connected { operation, .. }
            | Self::Ended { operation, .. } => operation,
        };
        Arc::ptr_eq(operation, current)
    }
}

impl Reservation {
    pub async fn bind(ip: IpAddr) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::new(
            ip,
            if ip.is_loopback() { 0 } else { RAW_PORT },
        ))
        .await
        .context("cannot bind raw input listener")?;
        let port = listener.local_addr()?.port();
        let mut ticket = [0; 32];
        getrandom::fill(&mut ticket)
            .map_err(|e| anyhow!("cannot create raw session ticket: {e}"))?;
        Ok(Self {
            listener,
            port,
            ticket,
        })
    }

    pub async fn receive(
        self,
        peer: MachineId,
        expected_ip: IpAddr,
        session: u64,
        ts: Arc<dyn TsApi>,
        target: Arc<dyn RawEmulate>,
    ) -> Result<()> {
        let _release = Release {
            target: target.clone(),
            session,
        };
        let accept = async {
            use futures::{stream::FuturesUnordered, StreamExt};
            let mut pending = FuturesUnordered::new();
            loop {
                tokio::select! {
                    accepted = self.listener.accept(), if pending.len() < 8 => {
                        let (mut stream, addr) = accepted?;
                        if addr.ip() != expected_ip { continue; }
                        stream.set_nodelay(true)?;
                        let ts = ts.clone();
                        let peer = peer.clone();
                        let ticket = self.ticket;
                        pending.push(async move {
                            tokio::time::timeout(IO_TIMEOUT, async {
                                let (status, who) = tokio::try_join!(ts.status(), ts.whois(addr))?;
                                ensure!(splice_tailscale::authorize(&status, &who) == splice_tailscale::AuthDecision::Peer(peer.0), "raw peer authentication failed");
                                match read(&mut stream).await? {
                                    Packet::Open { session: offered, ticket: offered_ticket } if offered == session && offered_ticket == ticket => {}
                                    _ => bail!("raw connection does not match its control session"),
                                }
                                write(&mut stream, &Packet::Accepted).await?;
                                Ok::<_, anyhow::Error>(stream)
                            }).await
                        });
                    }
                    authenticated = pending.next(), if !pending.is_empty() => {
                        if let Some(Ok(Ok(stream))) = authenticated { return Ok::<_, anyhow::Error>(stream); }
                    }
                }
            }
        };
        let mut stream = tokio::time::timeout(PREPARE_TIMEOUT, accept)
            .await
            .context("raw source did not connect before preparation expired")??;
        drop(self.listener);
        loop {
            match tokio::time::timeout(IO_TIMEOUT, read(&mut stream))
                .await
                .context("raw input heartbeat timed out")??
            {
                Packet::Report {
                    session: offered,
                    report,
                } => {
                    ensure!(offered == session, "raw report belongs to another session");
                    target.inject(session, &report)?;
                }
                Packet::Ping(tick) => {
                    write(&mut stream, &Packet::Pong(tick)).await?;
                }
                _ => bail!("unexpected raw input packet"),
            }
        }
    }
}

struct Release {
    target: Arc<dyn RawEmulate>,
    session: u64,
}

impl Drop for Release {
    fn drop(&mut self) {
        if let Err(error) = self.target.end(self.session) {
            tracing::error!(%error, "raw target release failed");
        }
    }
}

pub async fn connect(
    bind: IpAddr,
    remote: SocketAddr,
    session: u64,
    ticket: [u8; 32],
    ts: Arc<dyn TsApi>,
    peer: &MachineId,
) -> Result<TcpStream> {
    tokio::time::timeout(PREPARE_TIMEOUT, async {
        let socket = if bind.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        socket.bind(SocketAddr::new(bind, 0))?;
        let mut stream = socket.connect(remote).await?;
        stream.set_nodelay(true)?;
        let (status, who) = tokio::try_join!(ts.status(), ts.whois(remote))?;
        ensure!(
            splice_tailscale::authorize(&status, &who)
                == splice_tailscale::AuthDecision::Peer(peer.0.clone()),
            "raw destination authentication failed"
        );
        write(&mut stream, &Packet::Open { session, ticket }).await?;
        ensure!(
            matches!(read(&mut stream).await?, Packet::Accepted),
            "raw destination did not accept the session"
        );
        Ok(stream)
    })
    .await
    .context("raw connection preparation timed out")?
}

pub async fn send(
    stream: TcpStream,
    session: u64,
    mut reports: mpsc::Receiver<RawReport>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let (pong_tx, mut pong_rx) = mpsc::channel(4);
    let read_pongs = async {
        loop {
            match read(&mut reader).await? {
                Packet::Pong(tick) => pong_tx
                    .send(tick)
                    .await
                    .map_err(|_| anyhow!("raw writer stopped"))?,
                _ => bail!("unexpected raw destination packet"),
            }
        }
    };
    let write_reports = async {
        let mut heartbeat = tokio::time::interval(Duration::from_millis(200));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next_ping = 0;
        let mut last_pong = tokio::time::Instant::now();
        let mut last_pong_tick = None;
        loop {
            tokio::select! {
                report = reports.recv() => {
                    let Some(report) = report else { return Ok(()); };
                    report.validate().map_err(|e| anyhow!(e))?;
                    write(&mut writer, &Packet::Report { session, report }).await?;
                }
                pong = pong_rx.recv() => {
                    let tick = pong.ok_or_else(|| anyhow!("raw acknowledgement channel closed"))?;
                    ensure!(tick < next_ping && last_pong_tick.is_none_or(|last| tick > last), "invalid raw acknowledgement");
                    last_pong_tick = Some(tick);
                    last_pong = tokio::time::Instant::now();
                }
                _ = heartbeat.tick() => {
                    ensure!(last_pong.elapsed() < IO_TIMEOUT, "raw destination stopped acknowledging input");
                    write(&mut writer, &Packet::Ping(next_ping)).await?;
                    next_ping = next_ping.checked_add(1).ok_or_else(|| anyhow!("raw heartbeat sequence exhausted"))?;
                }
            }
        }
    };
    tokio::select! { result = read_pongs => result, result = write_reports => result }
}

async fn write<W: AsyncWrite + Unpin>(writer: &mut W, packet: &Packet) -> Result<()> {
    let payload = postcard::to_allocvec(packet)?;
    ensure!(
        payload.len() <= MAX_PACKET,
        "raw input packet exceeds size limit"
    );
    let mut bytes = Vec::with_capacity(4 + payload.len());
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&payload);
    tokio::time::timeout(IO_TIMEOUT, writer.write_all(&bytes))
        .await
        .context("raw input write stalled")??;
    Ok(())
}

async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Packet> {
    let len = reader.read_u32().await? as usize;
    ensure!(
        (1..=MAX_PACKET).contains(&len),
        "invalid raw input packet length"
    );
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    let (packet, extra) = postcard::take_from_bytes(&bytes)?;
    ensure!(extra.is_empty(), "trailing raw input packet bytes");
    Ok(packet)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use splice_proto::raw::RawEvent;
    use splice_tailscale::{Node, Status, WhoIs, WhoIsUser};

    pub(crate) struct Identity(pub(crate) &'static str);

    impl TsApi for Identity {
        fn status(&self) -> futures::future::BoxFuture<'_, splice_tailscale::Result<Status>> {
            Box::pin(async move {
                Ok(Status {
                    self_node: Node {
                        stable_id: self.0.into(),
                        user_id: 7,
                        ..Default::default()
                    },
                    peers: Vec::new(),
                })
            })
        }
        fn whois(
            &self,
            addr: SocketAddr,
        ) -> futures::future::BoxFuture<'_, splice_tailscale::Result<WhoIs>> {
            Box::pin(async move {
                let (id, user) = match addr.ip().to_string().as_str() {
                    "127.0.0.1" => ("a", 7),
                    "127.0.0.2" => ("b", 7),
                    _ => ("intruder", 99),
                };
                Ok(WhoIs {
                    node_stable_id: id.into(),
                    user: WhoIsUser {
                        id: user,
                        login_name: "test".into(),
                    },
                })
            })
        }
    }

    #[tokio::test]
    async fn listener_rejects_wrong_identity_and_ticket_then_releases_on_disconnect() {
        let (platform, handle) =
            splice_platform::mock::create(splice_platform::mock::one_display());
        let target = platform.raw_emulate.unwrap();
        target.prepare().await.unwrap();
        target.begin(1).unwrap();
        let reservation = Reservation::bind("127.0.0.2".parse().unwrap())
            .await
            .unwrap();
        let addr = SocketAddr::new("127.0.0.2".parse().unwrap(), reservation.port);
        let ticket = reservation.ticket;
        let receiver = tokio::spawn(reservation.receive(
            MachineId("a".into()),
            "127.0.0.1".parse().unwrap(),
            1,
            Arc::new(Identity("b")),
            target,
        ));
        for (ip, offered) in [("127.0.0.3", ticket), ("127.0.0.1", [0; 32])] {
            let socket = TcpSocket::new_v4().unwrap();
            socket
                .bind(SocketAddr::new(ip.parse().unwrap(), 0))
                .unwrap();
            let mut stream = socket.connect(addr).await.unwrap();
            let _ = write(
                &mut stream,
                &Packet::Open {
                    session: 1,
                    ticket: offered,
                },
            )
            .await;
            assert!(
                tokio::time::timeout(Duration::from_secs(2), read(&mut stream))
                    .await
                    .unwrap()
                    .is_err()
            );
            assert!(handle.state.lock().raw_reports.is_empty());
        }
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let _stalled = socket.connect(addr).await.unwrap();
        let mut stream = tokio::time::timeout(
            Duration::from_millis(500),
            connect(
                "127.0.0.1".parse().unwrap(),
                addr,
                1,
                ticket,
                Arc::new(Identity("a")),
                &MachineId("b".into()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let report = RawReport {
            device: 1,
            sequence: 0,
            captured_us: 0,
            events: vec![RawEvent::Key {
                code: 30,
                pressed: true,
            }],
        };
        write(&mut stream, &Packet::Report { session: 1, report })
            .await
            .unwrap();
        write(&mut stream, &Packet::Ping(0)).await.unwrap();
        assert!(matches!(read(&mut stream).await.unwrap(), Packet::Pong(0)));
        assert_eq!(handle.state.lock().raw_reports.len(), 1);
        drop(stream);
        assert!(receiver.await.unwrap().is_err());
        assert_eq!(
            handle.state.lock().raw_events,
            vec![
                RawEvent::Key {
                    code: 30,
                    pressed: true
                },
                RawEvent::Key {
                    code: 30,
                    pressed: false
                }
            ]
        );
    }

    #[tokio::test]
    async fn the_control_address_and_ticket_do_not_replace_whois_identity() {
        let (platform, mock) = splice_platform::mock::create(splice_platform::mock::one_display());
        let target = platform.raw_emulate.unwrap();
        target.prepare().await.unwrap();
        target.begin(1).unwrap();
        let reservation = Reservation::bind("127.0.0.2".parse().unwrap())
            .await
            .unwrap();
        let addr = SocketAddr::new("127.0.0.2".parse().unwrap(), reservation.port);
        let ticket = reservation.ticket;
        let receiver = tokio::spawn(reservation.receive(
            MachineId("control-owner".into()),
            "127.0.0.1".parse().unwrap(),
            1,
            Arc::new(Identity("b")),
            target,
        ));
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut stream = socket.connect(addr).await.unwrap();
        let _ = write(&mut stream, &Packet::Open { session: 1, ticket }).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), read(&mut stream))
                .await
                .unwrap()
                .is_err()
        );
        assert!(mock.state.lock().raw_reports.is_empty());
        receiver.abort();
        let _ = receiver.await;
        assert!(mock.state.lock().raw_session.is_none());
    }

    #[tokio::test]
    async fn packet_lengths_and_trailing_bytes_are_strict() {
        for bytes in [
            0u32.to_be_bytes().to_vec(),
            (MAX_PACKET as u32 + 1).to_be_bytes().to_vec(),
            vec![0, 0, 0, 2, 1, 0],
        ] {
            let mut input = bytes.as_slice();
            assert!(read(&mut input).await.is_err());
        }
    }
}
