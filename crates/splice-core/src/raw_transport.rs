use crate::{
    input_transport::{self, Connection, Endpoint, Incoming, Packet, HEARTBEAT, INPUT_TIMEOUT},
    net::TsApi,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use splice_platform::raw::{clock::now_us, diagnostics::Diagnostics, CapturedReport, RawEmulate};
use splice_proto::{
    raw::{InputMode, RawReport},
    MachineId,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

pub const RAW_PORT: u16 = 41719;
const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);
#[path = "raw_transport/timing.rs"]
pub(crate) mod timing;

pub struct Reservation {
    endpoint: Endpoint,
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
        stream: Connection,
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
    pub(crate) fn from_endpoint(endpoint: Endpoint) -> Result<Self> {
        let port = endpoint.address()?.port();
        let mut ticket = [0; 32];
        getrandom::fill(&mut ticket)
            .map_err(|error| anyhow!("cannot create raw input authorization: {error}"))?;
        Ok(Self {
            endpoint,
            port,
            ticket,
        })
    }

    #[cfg(test)]
    pub async fn bind(ip: IpAddr) -> Result<Self> {
        let endpoint = Endpoint::bind(SocketAddr::new(
            ip,
            if ip.is_loopback() { 0 } else { RAW_PORT },
        ))
        .await?;
        Self::from_endpoint(endpoint)
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
        let mut connection = self.endpoint.subscribe(
            SocketAddr::new(expected_ip, 0),
            self.ticket[..16].try_into().expect("ticket half"),
            self.ticket[16..].try_into().expect("ticket half"),
        )?;
        let mut clock = timing::ClockMap::default();
        tokio::time::timeout(PREPARE_TIMEOUT, async {
            loop {
                let incoming = connection.receive().await?;
                let Packet::Hello { sent_us } = incoming.packet else { continue };
                let identity = tokio::time::timeout(INPUT_TIMEOUT, async {
                    let (status, who) = tokio::try_join!(ts.status(), ts.whois(incoming.address))?;
                    Ok::<_, splice_tailscale::TsError>(splice_tailscale::authorize(&status, &who))
                }).await;
                if !matches!(identity, Ok(Ok(splice_tailscale::AuthDecision::Peer(ref id))) if id == &peer.0) { continue; }
                connection.pin(incoming.address)?;
                clock.observe(sent_us, incoming.received_us);
                connection.send(&Packet::HelloAck { sent_us }).await?;
                return Ok::<_, anyhow::Error>(());
            }
        }).await.context("raw UDP source did not authenticate before preparation expired")??;
        let mut receiver = input_transport::state::Receiver::new(InputMode::Raw);
        let mut sequence = 0u64;
        let mut diagnostics = Diagnostics::new("receive");
        let mut last_receive = None;
        loop {
            let Incoming {
                packet,
                received_us,
                ..
            } = tokio::time::timeout(INPUT_TIMEOUT, connection.receive())
                .await
                .context("raw UDP input heartbeat timed out")??;
            match packet {
                Packet::Hello { sent_us } => {
                    clock.observe(sent_us, received_us);
                    connection.send(&Packet::HelloAck { sent_us }).await?;
                }
                Packet::Data {
                    session: offered,
                    sent_us,
                    update,
                } => {
                    ensure!(
                        offered == session,
                        "raw datagram belongs to another session"
                    );
                    ensure!(
                        update.position.captured_us > 0
                            && update.position.captured_us <= sent_us
                            && update
                                .transitions
                                .iter()
                                .all(|edge| edge.position.captured_us > 0
                                    && edge.position.captured_us <= sent_us),
                        "invalid raw capture timestamp"
                    );
                    clock.observe(sent_us, received_us);
                    diagnostics.record(
                        "transit_above_floor",
                        received_us.saturating_sub(clock.map(sent_us)),
                    );
                    if let Some(last) = last_receive {
                        diagnostics.record("receive_gap", received_us.saturating_sub(last));
                    }
                    last_receive = Some(received_us);
                    let serial = update.position.serial;
                    let barrier = update.position.barrier;
                    for delivery in receiver.receive(update)? {
                        let captured_local_us = clock.map(delivery.captured_us);
                        let inject_us = now_us();
                        let age = inject_us.saturating_sub(captured_local_us);
                        ensure!(
                            age < INPUT_TIMEOUT.as_micros() as u64,
                            "raw input exceeded the 750 ms delivery age limit; input released"
                        );
                        diagnostics.record("mapped_capture_age", age);
                        let input_transport::state::Events::Raw(events) = delivery.events else {
                            bail!("desktop data on raw input session")
                        };
                        for events in events.chunks(splice_proto::raw::MAX_EVENTS) {
                            let report = RawReport {
                                device: 1,
                                sequence,
                                captured_us: delivery.captured_us,
                                events: events.to_vec(),
                            };
                            target.inject(session, &report, captured_local_us)?;
                            sequence = sequence
                                .checked_add(1)
                                .ok_or_else(|| anyhow!("raw injection sequence exhausted"))?;
                        }
                        diagnostics.record("inject_duration", now_us() - inject_us);
                    }
                    let next = receiver.next_transition();
                    connection
                        .send(&Packet::Ack {
                            session,
                            next,
                            serial: if next >= barrier { serial } else { 0 },
                            sent_us,
                        })
                        .await?;
                }
                _ => {}
            }
            diagnostics.flush_if_due(now_us());
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
    _session: u64,
    ticket: [u8; 32],
    ts: Arc<dyn TsApi>,
    peer: &MachineId,
) -> Result<Connection> {
    tokio::time::timeout(PREPARE_TIMEOUT, async {
        let (status, who) = tokio::try_join!(ts.status(), ts.whois(remote))?;
        ensure!(
            splice_tailscale::authorize(&status, &who)
                == splice_tailscale::AuthDecision::Peer(peer.0.clone()),
            "raw destination authentication failed"
        );
        let mut connection = input_transport::client(bind, remote, ticket).await?;
        connection.probe().await?;
        Ok(connection)
    })
    .await
    .context("raw UDP connection preparation timed out")?
}

pub async fn send(
    mut connection: Connection,
    session: u64,
    mut reports: mpsc::Receiver<CapturedReport>,
    mut finish: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let mut source = input_transport::state::Sender::new(InputMode::Raw);
    let mut sent_transition = 0;
    let mut acknowledged_serial = 0;
    let mut last_ack = tokio::time::Instant::now();
    let mut diagnostics = Diagnostics::new("send");
    let mut timer = tokio::time::interval(HEARTBEAT);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut closed = false;
    let mut finish_received = false;
    loop {
        if closed
            && source
                .position()
                .is_none_or(|position| acknowledged_serial >= position.serial)
            && source.pending().is_empty()
        {
            return Ok(());
        }
        tokio::select! {
            request = &mut finish, if !finish_received => {
                finish_received = true;
                if request.is_ok() { reports.close(); }
            }
            report = reports.recv(), if !closed => {
                let Some(CapturedReport { report, enqueued_us }) = report else { closed = true; continue };
                let sent_us = now_us();
                ensure!(report.captured_us > 0 && report.captured_us <= enqueued_us && enqueued_us <= sent_us, "invalid raw source timestamps");
                ensure!(sent_us - report.captured_us < INPUT_TIMEOUT.as_micros() as u64, "raw source exceeded the 750 ms capture age limit; input released");
                diagnostics.record("capture_to_enqueue", enqueued_us - report.captured_us);
                diagnostics.record("source_queue", sent_us - enqueued_us);
                source.push_raw(&report)?;
                input_transport::transmit(&connection, session, &source, &mut sent_transition, false).await?;
                diagnostics.record("socket_write", now_us() - sent_us);
            }
            incoming = connection.receive() => {
                match incoming?.packet {
                    Packet::Ack { session: offered, next, serial, sent_us } if offered == session => {
                        if sent_us > now_us() || now_us().saturating_sub(sent_us) >= INPUT_TIMEOUT.as_micros() as u64 { continue; }
                        source.acknowledge(next)?;
                        ensure!(source.position().is_some_and(|position| serial <= position.serial), "invalid raw UDP acknowledgement");
                        acknowledged_serial = acknowledged_serial.max(serial);
                        last_ack = tokio::time::Instant::now();
                        diagnostics.record("ack_rtt", now_us().saturating_sub(sent_us));
                    }
                    Packet::HelloAck { sent_us } => {
                        if sent_us <= now_us() && now_us() - sent_us < INPUT_TIMEOUT.as_micros() as u64 {
                            last_ack = tokio::time::Instant::now();
                            diagnostics.record("heartbeat_rtt", now_us() - sent_us);
                        }
                    }
                    Packet::Hello { sent_us } => { connection.send(&Packet::HelloAck { sent_us }).await?; }
                    _ => {}
                }
            }
            _ = timer.tick() => {
                ensure!(last_ack.elapsed() < INPUT_TIMEOUT, "raw destination stopped acknowledging UDP input");
                input_transport::transmit(&connection, session, &source, &mut sent_transition, true).await?;
                connection.send(&Packet::Hello { sent_us: now_us() }).await?;
            }
        }
        diagnostics.flush_if_due(now_us());
    }
}

#[cfg(test)]
#[path = "raw_transport/tests.rs"]
pub(crate) mod tests;
