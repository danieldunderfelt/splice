use crate::net::TsApi;
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use splice_platform::raw::{clock::now_us, diagnostics::Diagnostics, CapturedReport, RawEmulate};
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
#[path = "raw_transport/timing.rs"]
mod timing;

#[derive(Serialize, Deserialize)]
enum Packet {
    Open {
        session: u64,
        ticket: [u8; 32],
    },
    Accepted,
    Report {
        session: u64,
        sent_us: u64,
        report: RawReport,
    },
    Ping {
        tick: u64,
        sent_us: u64,
    },
    Pong(u64),
}

pub struct Reservation {
    listener: TcpListener,
    pub port: u16,
    pub ticket: [u8; 32],
}

pub enum Event {
    Prepared {
        operation: Arc<splice_platform::raw::RawOperation>,
        peer: MachineId,
        session: u64,
        pos: splice_proto::Vec2,
        result: std::result::Result<Reservation, String>,
    },
    Connected {
        operation: Arc<splice_platform::raw::RawOperation>,
        peer: MachineId,
        session: u64,
        stream: TcpStream,
    },
    Ended {
        operation: Arc<splice_platform::raw::RawOperation>,
        peer: MachineId,
        session: u64,
        error: String,
    },
}

impl Event {
    pub fn belongs_to(&self, current: &Arc<splice_platform::raw::RawOperation>) -> bool {
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
        let mut clock = timing::ClockMap::default();
        let mut diagnostics = Diagnostics::new("receive");
        let mut last_receive = None;
        loop {
            match tokio::time::timeout(IO_TIMEOUT, read(&mut stream))
                .await
                .context("raw input heartbeat timed out")??
            {
                Packet::Report {
                    session: offered,
                    sent_us,
                    report,
                } => {
                    ensure!(offered == session, "raw report belongs to another session");
                    ensure!(
                        report.captured_us > 0 && report.captured_us <= sent_us,
                        "invalid raw capture timestamp"
                    );
                    let received_us = now_us();
                    clock.observe(sent_us, received_us);
                    let captured_local_us = clock.map(report.captured_us);
                    ensure!(
                        received_us.saturating_sub(captured_local_us)
                            < IO_TIMEOUT.as_micros() as u64,
                        "raw input exceeded the 750 ms delivery age limit; input released"
                    );
                    diagnostics.record("transit_above_floor", received_us - clock.map(sent_us));
                    diagnostics.record(
                        "mapped_capture_age",
                        received_us.saturating_sub(captured_local_us),
                    );
                    if let Some(last) = last_receive {
                        diagnostics.record("receive_gap", received_us.saturating_sub(last));
                    }
                    last_receive = Some(received_us);
                    let inject_us = now_us();
                    diagnostics.record("receive_to_inject", inject_us - received_us);
                    target.inject(session, &report, captured_local_us)?;
                    diagnostics.record("inject_duration", now_us() - inject_us);
                    diagnostics.flush_if_due(received_us);
                }
                Packet::Ping { tick, sent_us } => {
                    clock.observe(sent_us, now_us());
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
        for tick in 0..4 {
            write(
                &mut stream,
                &Packet::Ping {
                    tick,
                    sent_us: now_us(),
                },
            )
            .await?;
        }
        for tick in 0..4 {
            ensure!(
                matches!(read(&mut stream).await?, Packet::Pong(received) if received == tick),
                "raw destination did not acknowledge clock samples"
            );
        }
        Ok(stream)
    })
    .await
    .context("raw connection preparation timed out")?
}

pub async fn send(
    stream: TcpStream,
    session: u64,
    mut reports: mpsc::Receiver<CapturedReport>,
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
        let mut diagnostics = Diagnostics::new("send");
        let mut pending_pings = std::collections::BTreeMap::new();
        let mut heartbeat = tokio::time::interval(Duration::from_millis(200));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next_ping = 0;
        let mut last_pong = tokio::time::Instant::now();
        let mut last_pong_tick = None;
        loop {
            tokio::select! {
                report = reports.recv() => {
                    let Some(report) = report else { return Ok(()); };
                    let CapturedReport { report, enqueued_us } = report;
                    report.validate().map_err(|e| anyhow!(e))?;
                    let sent_us = now_us();
                    ensure!(report.captured_us > 0 && report.captured_us <= enqueued_us && enqueued_us <= sent_us, "invalid raw source timestamps");
                    ensure!(sent_us - report.captured_us < IO_TIMEOUT.as_micros() as u64, "raw source exceeded the 750 ms capture age limit; input released");
                    diagnostics.record("capture_to_enqueue", enqueued_us - report.captured_us);
                    diagnostics.record("source_queue", sent_us - enqueued_us);
                    write(&mut writer, &Packet::Report { session, sent_us, report }).await?;
                    diagnostics.record("socket_write", now_us() - sent_us);
                    diagnostics.flush_if_due(sent_us);
                }
                pong = pong_rx.recv() => {
                    let tick = pong.ok_or_else(|| anyhow!("raw acknowledgement channel closed"))?;
                    ensure!(tick < next_ping && last_pong_tick.is_none_or(|last| tick > last), "invalid raw acknowledgement");
                    last_pong_tick = Some(tick);
                    last_pong = tokio::time::Instant::now();
                    let sent_us = pending_pings.remove(&tick).ok_or_else(|| anyhow!("unknown raw heartbeat"))?;
                    diagnostics.record("heartbeat_rtt", now_us() - sent_us);
                }
                _ = heartbeat.tick() => {
                    ensure!(last_pong.elapsed() < IO_TIMEOUT, "raw destination stopped acknowledging input");
                    let sent_us = now_us();
                    pending_pings.insert(next_ping, sent_us);
                    write(&mut writer, &Packet::Ping { tick: next_ping, sent_us }).await?;
                    diagnostics.flush_if_due(sent_us);
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
            captured_us: now_us(),
            events: vec![RawEvent::Key {
                code: 30,
                pressed: true,
            }],
        };
        write(
            &mut stream,
            &Packet::Report {
                session: 1,
                sent_us: now_us(),
                report,
            },
        )
        .await
        .unwrap();
        write(
            &mut stream,
            &Packet::Ping {
                tick: 0,
                sent_us: now_us(),
            },
        )
        .await
        .unwrap();
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

    struct StalledTarget {
        inner: Arc<dyn RawEmulate>,
    }

    #[tokio::test]
    async fn receiver_rejects_expired_reports_and_releases_previously_held_input() {
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
        let mut stream = connect(
            "127.0.0.1".parse().unwrap(),
            addr,
            1,
            ticket,
            Arc::new(Identity("a")),
            &MachineId("b".into()),
        )
        .await
        .unwrap();
        let report = RawReport {
            device: 1,
            sequence: 0,
            captured_us: now_us(),
            events: vec![RawEvent::Key {
                code: 42,
                pressed: true,
            }],
        };
        write(
            &mut stream,
            &Packet::Report {
                session: 1,
                sent_us: now_us(),
                report,
            },
        )
        .await
        .unwrap();
        write(
            &mut stream,
            &Packet::Ping {
                tick: 0,
                sent_us: now_us(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(read(&mut stream).await.unwrap(), Packet::Pong(0)));
        let report = RawReport {
            device: 1,
            sequence: 1,
            captured_us: now_us() - 800_000,
            events: vec![RawEvent::Key {
                code: 30,
                pressed: true,
            }],
        };
        write(
            &mut stream,
            &Packet::Report {
                session: 1,
                sent_us: now_us(),
                report,
            },
        )
        .await
        .unwrap();
        let error = receiver.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("750 ms delivery age"), "{error}");
        let state = handle.state.lock();
        assert!(state.raw_session.is_none());
        assert_eq!(state.raw_reports.len(), 1);
        assert_eq!(
            state.raw_events,
            [
                RawEvent::Key {
                    code: 42,
                    pressed: true
                },
                RawEvent::Key {
                    code: 42,
                    pressed: false
                }
            ]
        );
    }

    #[async_trait::async_trait]
    impl RawEmulate for StalledTarget {
        async fn prepare(&self) -> splice_platform::Result<()> {
            self.inner.prepare().await
        }
        fn begin(&self, session: u64) -> splice_platform::Result<()> {
            self.inner.begin(session)
        }
        fn end(&self, session: u64) -> splice_platform::Result<()> {
            self.inner.end(session)
        }
        fn inject(
            &self,
            session: u64,
            report: &RawReport,
            timestamp: u64,
        ) -> splice_platform::Result<()> {
            if report.sequence == 1 {
                std::thread::sleep(Duration::from_millis(100));
            }
            self.inner.inject(session, report, timestamp)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_report_spacing_survives_a_receiver_stall_without_paced_replay() {
        let (platform, handle) =
            splice_platform::mock::create(splice_platform::mock::one_display());
        let target = Arc::new(StalledTarget {
            inner: platform.raw_emulate.unwrap(),
        });
        target.prepare().await.unwrap();
        target.begin(1).unwrap();
        let reservation = Reservation::bind("127.0.0.2".parse().unwrap())
            .await
            .unwrap();
        let address = SocketAddr::new("127.0.0.2".parse().unwrap(), reservation.port);
        let ticket = reservation.ticket;
        let receive = tokio::spawn(reservation.receive(
            MachineId("a".into()),
            "127.0.0.1".parse().unwrap(),
            1,
            Arc::new(Identity("b")),
            target,
        ));
        let stream = connect(
            "127.0.0.1".parse().unwrap(),
            address,
            1,
            ticket,
            Arc::new(Identity("a")),
            &MachineId("b".into()),
        )
        .await
        .unwrap();
        let (tx, rx) = mpsc::channel(128);
        let send = tokio::spawn(send(stream, 1, rx));
        let source = std::thread::spawn(move || {
            for sequence in 0..50 {
                let report = RawReport {
                    device: 1,
                    sequence,
                    captured_us: now_us(),
                    events: vec![RawEvent::Motion { x: 1, y: -1 }],
                };
                tx.blocking_send(report.into()).unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        tokio::task::spawn_blocking(move || source.join().unwrap())
            .await
            .unwrap();
        send.await.unwrap().unwrap();
        assert!(receive.await.unwrap().is_err());
        let state = handle.state.lock();
        assert_eq!(state.raw_reports.len(), 50);
        assert_eq!(state.raw_events.len(), 50);
        let native_span = state.raw_reports[49].captured_us - state.raw_reports[2].captured_us;
        let mapped_span = state.raw_timestamps[49] - state.raw_timestamps[2];
        assert!(native_span >= 40_000);
        assert!(
            native_span.abs_diff(mapped_span) < 5000,
            "native {native_span} mapped {mapped_span}"
        );
        assert!(state.raw_timestamps.iter().all(|stamp| *stamp <= now_us()));
        assert!(state.raw_session.is_none());
    }
}
