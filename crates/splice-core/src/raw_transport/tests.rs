use super::*;
use splice_proto::raw::RawEvent;
use splice_tailscale::{Node, Status, WhoIs, WhoIsUser};

async fn connected() -> (
    Connection,
    tokio::task::JoinHandle<Result<()>>,
    splice_platform::mock::MockHandle,
) {
    let (platform, handle) = splice_platform::mock::create(splice_platform::mock::one_display());
    let target = platform.raw_emulate.unwrap();
    target.prepare().await.unwrap();
    target.begin(1).unwrap();
    let reservation = Reservation::bind("127.0.0.2".parse().unwrap())
        .await
        .unwrap();
    let address = SocketAddr::new("127.0.0.2".parse().unwrap(), reservation.port);
    let ticket = reservation.ticket;
    let receiver = tokio::spawn(reservation.receive(
        MachineId("a".into()),
        "127.0.0.1".parse().unwrap(),
        1,
        Arc::new(Identity("b")),
        target,
    ));
    let connection = connect(
        "127.0.0.1".parse().unwrap(),
        address,
        1,
        ticket,
        Arc::new(Identity("a")),
        &MachineId("b".into()),
    )
    .await
    .unwrap();
    (connection, receiver, handle)
}

async fn acknowledged(connection: &mut Connection, serial: u64) -> u64 {
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Packet::Ack {
                serial: applied,
                next,
                ..
            } = connection.receive().await.unwrap().packet
            {
                if applied >= serial {
                    return next;
                }
            }
        }
    })
    .await
    .unwrap()
}

fn report(sequence: u64, events: Vec<RawEvent>) -> RawReport {
    RawReport {
        device: 1,
        sequence,
        captured_us: now_us(),
        events,
    }
}

#[tokio::test]
async fn the_ticket_and_ip_do_not_replace_the_control_owners_whois_identity() {
    let (platform, mock) = splice_platform::mock::create(splice_platform::mock::one_display());
    let target = platform.raw_emulate.unwrap();
    target.prepare().await.unwrap();
    target.begin(1).unwrap();
    let reservation = Reservation::bind("127.0.0.2".parse().unwrap())
        .await
        .unwrap();
    let address = SocketAddr::new("127.0.0.2".parse().unwrap(), reservation.port);
    let ticket = reservation.ticket;
    let receiver = tokio::spawn(reservation.receive(
        MachineId("control-owner".into()),
        "127.0.0.1".parse().unwrap(),
        1,
        Arc::new(Identity("b")),
        target,
    ));
    let mut connection = input_transport::client("127.0.0.1".parse().unwrap(), address, ticket)
        .await
        .unwrap();
    connection
        .send(&Packet::Hello { sent_us: now_us() })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), connection.receive())
            .await
            .is_err()
    );
    assert!(mock.state.lock().raw_events.is_empty());
    receiver.abort();
    let _ = receiver.await;
    assert!(mock.state.lock().raw_session.is_none());
}

#[tokio::test]
async fn missing_motion_recovers_without_blocking_or_replaying_old_packets() {
    let (mut connection, receiver, mock) = connected().await;
    let mut source = input_transport::state::Sender::new(InputMode::Raw);
    source
        .push_raw(&report(0, vec![RawEvent::Motion { x: 3, y: -4 }]))
        .unwrap();
    let old = Packet::Data {
        session: 1,
        sent_us: now_us(),
        update: input_transport::state::Update {
            position: source.position().unwrap().clone(),
            transitions: vec![],
        },
    };
    source
        .push_raw(&report(1, vec![RawEvent::Motion { x: 8, y: 7 }]))
        .unwrap();
    input_transport::transmit(&connection, 1, &source, &mut 0, false)
        .await
        .unwrap();
    acknowledged(&mut connection, 2).await;
    connection.send(&old).await.unwrap();
    input_transport::transmit(&connection, 1, &source, &mut 0, true)
        .await
        .unwrap();
    acknowledged(&mut connection, 2).await;
    assert_eq!(
        mock.state.lock().raw_events,
        vec![RawEvent::Motion { x: 11, y: 3 }]
    );
    receiver.abort();
    let _ = receiver.await;
    assert!(mock.state.lock().raw_session.is_none());
}

#[tokio::test]
async fn missing_button_down_holds_drag_motion_until_its_exact_position_is_known() {
    let (mut connection, receiver, mock) = connected().await;
    let mut source = input_transport::state::Sender::new(InputMode::Raw);
    source
        .push_raw(&report(
            0,
            vec![
                RawEvent::Motion { x: 10, y: 0 },
                RawEvent::Button {
                    number: 1,
                    pressed: true,
                },
            ],
        ))
        .unwrap();
    source
        .push_raw(&report(
            1,
            vec![
                RawEvent::Motion { x: 20, y: 0 },
                RawEvent::Button {
                    number: 1,
                    pressed: false,
                },
            ],
        ))
        .unwrap();
    let incomplete = input_transport::state::Update {
        position: source.position().unwrap().clone(),
        transitions: vec![source.pending()[1].clone()],
    };
    connection
        .send(&Packet::Data {
            session: 1,
            sent_us: now_us(),
            update: incomplete,
        })
        .await
        .unwrap();
    loop {
        if let Packet::Ack { next, serial, .. } = connection.receive().await.unwrap().packet {
            assert_eq!((next, serial), (0, 0));
            break;
        }
    }
    assert!(mock.state.lock().raw_events.is_empty());
    input_transport::transmit(&connection, 1, &source, &mut 0, true)
        .await
        .unwrap();
    assert_eq!(acknowledged(&mut connection, 2).await, 2);
    assert_eq!(
        mock.state.lock().raw_events,
        vec![
            RawEvent::Motion { x: 10, y: 0 },
            RawEvent::Button {
                number: 1,
                pressed: true
            },
            RawEvent::Motion { x: 20, y: 0 },
            RawEvent::Button {
                number: 1,
                pressed: false
            },
        ]
    );
    receiver.abort();
    let _ = receiver.await;
}

#[tokio::test]
async fn many_fast_taps_survive_chunking_lost_acknowledgements_and_duplicates() {
    let (mut connection, receiver, mock) = connected().await;
    let mut source = input_transport::state::Sender::new(InputMode::Raw);
    let mut expected = Vec::new();
    for index in 0..200 {
        let event = RawEvent::Key {
            code: 30,
            pressed: index % 2 == 0,
        };
        source.push_raw(&report(index, vec![event])).unwrap();
        expected.push(event);
    }
    input_transport::transmit(&connection, 1, &source, &mut 0, true)
        .await
        .unwrap();
    while acknowledged(&mut connection, 200).await < 200 {}
    input_transport::transmit(&connection, 1, &source, &mut 0, true)
        .await
        .unwrap();
    while acknowledged(&mut connection, 200).await < 200 {}
    assert_eq!(mock.state.lock().raw_events, expected);
    receiver.abort();
    let _ = receiver.await;
}

#[tokio::test]
async fn an_expired_transition_releases_held_input_instead_of_replaying_it() {
    let (mut connection, receiver, mock) = connected().await;
    let mut source = input_transport::state::Sender::new(InputMode::Raw);
    source
        .push_raw(&report(
            0,
            vec![RawEvent::Key {
                code: 42,
                pressed: true,
            }],
        ))
        .unwrap();
    input_transport::transmit(&connection, 1, &source, &mut 0, false)
        .await
        .unwrap();
    source
        .acknowledge(acknowledged(&mut connection, 1).await)
        .unwrap();
    let mut expired = report(
        1,
        vec![RawEvent::Key {
            code: 30,
            pressed: true,
        }],
    );
    expired.captured_us -= 800_000;
    source.push_raw(&expired).unwrap();
    connection
        .send(&Packet::Data {
            session: 1,
            sent_us: now_us(),
            update: input_transport::state::Update {
                position: source.position().unwrap().clone(),
                transitions: source.pending().iter().cloned().collect(),
            },
        })
        .await
        .unwrap();
    assert!(receiver
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("750 ms delivery age"));
    assert_eq!(
        mock.state.lock().raw_events,
        vec![
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
    assert!(mock.state.lock().raw_session.is_none());
}

#[tokio::test]
async fn idle_raw_heartbeats_keep_a_session_alive_and_disconnect_releases_it() {
    let (connection, receiver, mock) = connected().await;
    let (reports, input) = mpsc::channel(4);
    let sender = tokio::spawn(send(connection, 1, input));
    reports
        .send(
            report(
                0,
                vec![RawEvent::Key {
                    code: 42,
                    pressed: true,
                }],
            )
            .into(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!sender.is_finished());
    assert_eq!(mock.state.lock().raw_session, Some(1));
    sender.abort();
    let _ = sender.await;
    tokio::time::timeout(Duration::from_millis(900), receiver)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        mock.state.lock().raw_events.last(),
        Some(&RawEvent::Key {
            code: 42,
            pressed: false
        })
    );
}
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

struct StalledTarget {
    inner: Arc<dyn RawEmulate>,
}

#[tokio::test]
async fn udp_loss_reordering_and_duplication_preserve_every_transition_at_its_position() {
    use tokio::net::UdpSocket;
    let (platform, mock) = splice_platform::mock::create(splice_platform::mock::one_display());
    let target = platform.raw_emulate.unwrap();
    target.prepare().await.unwrap();
    target.begin(1).unwrap();
    let reservation = Reservation::bind("127.0.0.2".parse().unwrap())
        .await
        .unwrap();
    let destination = SocketAddr::new("127.0.0.2".parse().unwrap(), reservation.port);
    let ticket = reservation.ticket;
    let left = UdpSocket::bind("127.0.0.2:0").await.unwrap();
    let right = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let source_endpoint = left.local_addr().unwrap();
    let relay = tokio::spawn(async move {
        let mut source = None;
        let mut outward = [0; 1201];
        let mut inward = [0; 1201];
        let mut delayed: Option<Vec<u8>> = None;
        let mut data_packets = 0u64;
        let mut acknowledgements = 0u64;
        loop {
            tokio::select! {
                packet = left.recv_from(&mut outward) => {
                    let (count, address) = packet.unwrap();
                    source = Some(address);
                    assert!(count <= 1200);
                    let data = matches!(postcard::from_bytes::<Packet>(&outward[20..count]).unwrap(), Packet::Data { .. });
                    if data {
                        data_packets += 1;
                        if data_packets.is_multiple_of(5) { continue; }
                        if data_packets.is_multiple_of(11) && delayed.is_none() {
                            delayed = Some(outward[..count].to_vec());
                            continue;
                        }
                    }
                    right.send_to(&outward[..count], destination).await.unwrap();
                    if data && data_packets.is_multiple_of(13) { right.send_to(&outward[..count], destination).await.unwrap(); }
                    if let Some(old) = delayed.take() { right.send_to(&old, destination).await.unwrap(); }
                }
                packet = right.recv_from(&mut inward) => {
                    let (count, address) = packet.unwrap();
                    assert_eq!(address, destination);
                    if matches!(postcard::from_bytes::<Packet>(&inward[20..count]).unwrap(), Packet::Ack { .. }) {
                        acknowledgements += 1;
                        if acknowledgements.is_multiple_of(7) { continue; }
                    }
                    left.send_to(&inward[..count], source.unwrap()).await.unwrap();
                }
            }
        }
    });
    let receiver = tokio::spawn(reservation.receive(
        MachineId("a".into()),
        "127.0.0.1".parse().unwrap(),
        1,
        Arc::new(Identity("b")),
        target,
    ));
    let connection = connect(
        "127.0.0.1".parse().unwrap(),
        source_endpoint,
        1,
        ticket,
        Arc::new(Identity("a")),
        &MachineId("b".into()),
    )
    .await
    .unwrap();
    let (reports, input) = mpsc::channel(1024);
    let sender = tokio::spawn(send(connection, 1, input));
    let mut expected = Vec::new();
    let mut totals = (0i64, 0i64);
    let mut interval = tokio::time::interval(Duration::from_micros(1000));
    for sequence in 0..400 {
        interval.tick().await;
        let x = if sequence % 9 < 5 { 3 } else { -4 };
        let y = -(sequence as i32 % 7);
        totals.0 += i64::from(x);
        totals.1 += i64::from(y);
        let mut events = vec![RawEvent::Motion { x, y }];
        if sequence % 10 == 0 {
            let event = RawEvent::Button {
                number: 1,
                pressed: sequence % 20 == 0,
            };
            expected.push((event, totals));
            events.push(event);
        }
        reports.send(report(sequence, events).into()).await.unwrap();
    }
    drop(reports);
    tokio::time::timeout(Duration::from_secs(2), sender)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), receiver)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    relay.abort();
    let mut delivered = Vec::new();
    let mut accumulated = (0i64, 0i64);
    for event in &mock.state.lock().raw_events {
        match *event {
            RawEvent::Motion { x, y } => {
                accumulated.0 += i64::from(x);
                accumulated.1 += i64::from(y);
            }
            _ => delivered.push((*event, accumulated)),
        }
    }
    assert_eq!(delivered, expected);
    assert_eq!(accumulated, totals);
    assert!(mock.state.lock().raw_session.is_none());
}
#[async_trait::async_trait]
impl RawEmulate for StalledTarget {
    async fn prepare(&self) -> splice_platform::Result<()> {
        self.inner.prepare().await
    }
    fn begin(&self, session: u64) -> splice_platform::Result<()> {
        self.inner.begin(session)
    }
    fn boundary_policy(&self, session: u64, enabled: bool) -> splice_platform::Result<()> {
        self.inner.boundary_policy(session, enabled)
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
    let (platform, handle) = splice_platform::mock::create(splice_platform::mock::one_display());
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

async fn send(
    connection: Connection,
    session: u64,
    reports: mpsc::Receiver<CapturedReport>,
) -> Result<()> {
    super::send(
        connection,
        session,
        reports,
        tokio::sync::oneshot::channel().1,
    )
    .await
}
