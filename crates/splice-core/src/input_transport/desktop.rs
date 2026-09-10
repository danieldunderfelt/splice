use super::{state, Connection, Packet, HEARTBEAT, INPUT_TIMEOUT};
use crate::{diagnostics::Traffic, net::PeerEvent, raw_transport::timing::ClockMap};
use anyhow::{anyhow, bail, ensure, Result};
use splice_platform::raw::clock::now_us;
use splice_proto::{raw::InputMode, InputEvent, MachineId};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};

enum Command {
    Start(u64),
    Abandon(u64),
    Input {
        session: u64,
        event: InputEvent,
        captured_us: u64,
    },
    Finish {
        session: u64,
        complete: oneshot::Sender<()>,
    },
}

struct Task(tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone)]
pub(crate) struct Control {
    commands: mpsc::Sender<Command>,
    allowed: watch::Sender<Option<u64>>,
    _task: Arc<Task>,
}

impl Control {
    pub(crate) fn spawn(
        connection: Connection,
        peer: MachineId,
        generation: u64,
        events: mpsc::UnboundedSender<PeerEvent>,
        shutdown: watch::Sender<Option<String>>,
        traffic: Arc<Traffic>,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(4096);
        let (allowed, policy) = watch::channel(None);
        let task = tokio::spawn(async move {
            let result = run(
                connection, &peer, generation, &events, receiver, policy, traffic,
            )
            .await;
            if let Err(error) = result {
                let reason = format!("UDP input failed: {error}");
                let _ = events.send(PeerEvent::InputFailed {
                    id: peer,
                    connection: generation,
                    reason: reason.clone(),
                });
                shutdown.send_replace(Some(reason));
            }
        });
        Self {
            commands,
            allowed,
            _task: Arc::new(Task(task)),
        }
    }

    pub(crate) fn start(&self, session: u64) -> bool {
        self.commands.try_send(Command::Start(session)).is_ok()
    }

    pub(crate) fn push(&self, session: u64, event: InputEvent) -> bool {
        self.commands
            .try_send(Command::Input {
                session,
                event,
                captured_us: now_us(),
            })
            .is_ok()
    }

    pub(crate) fn allow(&self, session: Option<u64>) {
        self.allowed.send_replace(session);
    }

    pub(crate) fn abandon(&self, session: u64) -> bool {
        self.commands.try_send(Command::Abandon(session)).is_ok()
    }

    pub(crate) async fn finish(&self, session: u64) -> bool {
        let (complete, finished) = oneshot::channel();
        tokio::time::timeout(INPUT_TIMEOUT, async {
            self.commands
                .send(Command::Finish { session, complete })
                .await
                .map_err(|_| ())?;
            finished.await.map_err(|_| ())
        })
        .await
        .is_ok_and(|result| result.is_ok())
    }
}

struct Source {
    abandoned: bool,
    state: state::Sender,
    sent_transition: u64,
    acknowledged_serial: u64,
    last_ack: Instant,
    finish: Option<oneshot::Sender<()>>,
}

impl Source {
    fn new() -> Self {
        Self {
            abandoned: false,
            state: state::Sender::new(InputMode::Desktop),
            sent_transition: 0,
            acknowledged_serial: 0,
            last_ack: Instant::now(),
            finish: None,
        }
    }

    fn delivered(&self) -> bool {
        self.state.pending().is_empty()
            && self
                .state
                .position()
                .is_none_or(|position| position.serial <= self.acknowledged_serial)
    }
}

struct Destination {
    session: u64,
    state: state::Receiver,
    last_receive: Instant,
}

async fn run(
    mut connection: Connection,
    peer: &MachineId,
    generation: u64,
    events: &mpsc::UnboundedSender<PeerEvent>,
    mut commands: mpsc::Receiver<Command>,
    mut allowed: watch::Receiver<Option<u64>>,
    traffic: Arc<Traffic>,
) -> Result<()> {
    let peer = Arc::new(peer.clone());
    let mut sources = BTreeMap::<u64, Source>::new();
    let mut destination: Option<Destination> = None;
    let mut clock = ClockMap::default();
    let mut last_heartbeat = Instant::now();
    let mut timer = tokio::time::interval(HEARTBEAT);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let accepted = *allowed.borrow_and_update();
        if destination.as_ref().map(|target| target.session) != accepted {
            destination = accepted.map(|session| Destination {
                session,
                state: state::Receiver::new(InputMode::Desktop),
                last_receive: Instant::now(),
            });
        }
        tokio::select! {
            changed = allowed.changed() => { if changed.is_err() { return Ok(()); } }
            command = commands.recv() => {
                match command {
                    None => return Ok(()),
                    Some(Command::Start(session)) => {
                        ensure!(!sources.contains_key(&session) && sources.len() < 4, "overlapping desktop input sessions exceeded their limit");
                        let mut source = Source::new();
                        source.state.push_desktop(InputEvent::Motion { dx: 0.0, dy: 0.0 }, now_us())?;
                        sources.insert(session, source);
                    }
                    Some(Command::Input { session, event, captured_us }) => {
                        let Some(source) = sources.get_mut(&session) else { continue };
                        if source.abandoned || source.finish.is_some() { continue; }
                        let started = Instant::now();
                        let queued = now_us().saturating_sub(captured_us);
                        ensure!(queued < INPUT_TIMEOUT.as_micros() as u64, "desktop capture queue exceeded 750 ms");
                        source.state.push_desktop(event, captured_us)?;
                        let bytes = super::transmit(&connection, session, &source.state, &mut source.sent_transition, false).await?;
                        traffic.sent(true, bytes, Duration::from_micros(queued), started.elapsed());
                    }
                    Some(Command::Finish { session, complete }) => {
                        if let Some(source) = sources.get_mut(&session) {
                            ensure!(source.finish.is_none(), "desktop session ended twice");
                            source.finish = Some(complete);
                        } else {
                            let _ = complete.send(());
                        }
                    }
                    Some(Command::Abandon(session)) => {
                        if let Some(source) = sources.get_mut(&session) {
                            source.abandoned = true;
                            source.state = state::Sender::new(InputMode::Desktop);
                        }
                    }
                }
            }
            incoming = connection.receive() => {
                let incoming = incoming?;
                match incoming.packet {
                    Packet::Hello { sent_us } => {
                        clock.observe(sent_us, incoming.received_us);
                        if let Some(target) = &mut destination { target.last_receive = Instant::now(); }
                        connection.send(&Packet::HelloAck { sent_us }).await?;
                    }
                    Packet::HelloAck { sent_us } => {
                        if sent_us <= now_us() && now_us().saturating_sub(sent_us) < INPUT_TIMEOUT.as_micros() as u64 {
                            for source in sources.values_mut().filter(|source| !source.abandoned && source.delivered()) { source.last_ack = Instant::now(); }
                        }
                    }
                    Packet::Data { session, sent_us, update } => {
                        if *allowed.borrow() != Some(session) { continue; }
                        if destination.as_ref().is_none_or(|target| target.session != session) {
                            destination = Some(Destination { session, state: state::Receiver::new(InputMode::Desktop), last_receive: Instant::now() });
                        }
                        let target = destination.as_mut().expect("allowed destination exists");
                        ensure!(update.position.captured_us > 0 && update.position.captured_us <= sent_us
                            && update.transitions.iter().all(|edge| edge.position.captured_us > 0 && edge.position.captured_us <= sent_us), "invalid desktop capture timestamp");
                        clock.observe(sent_us, incoming.received_us);
                        let serial = update.position.serial;
                        let barrier = update.position.barrier;
                        for delivery in target.state.receive(update)? {
                            let captured_us = clock.map(delivery.captured_us);
                            ensure!(now_us().saturating_sub(captured_us) < INPUT_TIMEOUT.as_micros() as u64, "desktop input delivery exceeded 750 ms");
                            let state::Events::Desktop(input) = delivery.events else { bail!("raw data in desktop input session") };
                            events.send(PeerEvent::Input { from: peer.clone(), connection: generation, session, captured_us, events: input }).map_err(|_| anyhow!("input engine stopped"))?;
                        }
                        target.last_receive = Instant::now();
                        let next = target.state.next_transition();
                        connection.send(&Packet::Ack { session, next, serial: if next >= barrier { serial } else { 0 }, sent_us }).await?;
                    }
                    Packet::Ack { session, next, serial, sent_us } => {
                        let Some(source) = sources.get_mut(&session) else { continue };
                        if source.abandoned { continue; }
                        if sent_us > now_us() || now_us().saturating_sub(sent_us) >= INPUT_TIMEOUT.as_micros() as u64 { continue; }
                        source.state.acknowledge(next)?;
                        ensure!(source.state.position().is_some_and(|position| serial <= position.serial), "invalid desktop acknowledgement");
                        source.acknowledged_serial = source.acknowledged_serial.max(serial);
                        source.last_ack = Instant::now();
                    }
                }
            }
            _ = timer.tick() => {
                for (&session, source) in &mut sources {
                    if source.state.position().is_some() {
                        let deadline = if source.acknowledged_serial == 0 { Duration::from_secs(5) } else { INPUT_TIMEOUT };
                        ensure!(source.last_ack.elapsed() < deadline, "desktop destination stopped acknowledging input");
                        if !source.delivered() {
                            super::transmit(&connection, session, &source.state, &mut source.sent_transition, true).await?;
                        }
                    }
                }
                if let Some(target) = &destination {
                    ensure!(target.last_receive.elapsed() < INPUT_TIMEOUT, "desktop source stopped sending input");
                }
                if last_heartbeat.elapsed() >= Duration::from_millis(100) {
                    connection.send(&Packet::Hello { sent_us: now_us() }).await?;
                    last_heartbeat = Instant::now();
                }
            }
        }
        sources.retain(|_, source| {
            if source.finish.is_some() && source.delivered() {
                let _ = source
                    .finish
                    .take()
                    .expect("completed source has a waiter")
                    .send(());
                false
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_transport::Endpoint;

    #[tokio::test]
    async fn delayed_ack_after_target_departure_cannot_revive_or_disconnect_the_session() {
        let a = Endpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let b = Endpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let source = a.subscribe(b.address().unwrap(), [1; 16], [2; 16]).unwrap();
        let remote = b.subscribe(a.address().unwrap(), [2; 16], [1; 16]).unwrap();
        let (events, mut received) = mpsc::unbounded_channel();
        let (shutdown, reason) = watch::channel(None);
        let source = Control::spawn(
            source,
            MachineId("b".into()),
            1,
            events,
            shutdown,
            Arc::new(Traffic::default()),
        );
        assert!(source.start(1));
        assert!(source.push(
            1,
            InputEvent::Key {
                code: 42,
                pressed: true
            }
        ));
        assert!(source.abandon(1));
        tokio::time::sleep(Duration::from_millis(20)).await;
        remote
            .send(&Packet::Ack {
                session: 1,
                next: 1,
                serial: 2,
                sent_us: now_us(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(source.finish(1).await);
        assert!(reason.borrow().is_none());
        assert!(received.try_recv().is_err());
    }
}
