//! Net layer integration tests: two in-process NetManagers over loopback TCP with a
//! fake TsApi (no tailscaled involved). Each pair uses MachineIds "aaa" / "bbb", so
//! "aaa" is always the rule-following dialer.

use splice_core::net::{NetControl, NetManager, NetOpts, PeerEvent, TsApi};
use splice_proto::{caps, Frame, MachineId, MachineInfo, Os};
use splice_tailscale::{Node, Status, TsError, WhoIs, WhoIsUser};
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[derive(Clone)]
struct FakeTs {
    self_id: String,
    user_id: u64,
    whois: Arc<HashMap<IpAddr, (String, u64)>>,
}

impl TsApi for FakeTs {
    fn status(&self) -> Pin<Box<dyn Future<Output = Result<Status, TsError>> + Send + '_>> {
        let (self_id, user_id) = (self.self_id.clone(), self.user_id);
        Box::pin(async move {
            Ok(Status {
                self_node: Node { stable_id: self_id, user_id, ..Default::default() },
                peers: vec![],
            })
        })
    }

    fn whois(
        &self,
        addr: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Result<WhoIs, TsError>> + Send + '_>> {
        let found = self.whois.get(&addr.ip()).cloned();
        Box::pin(async move {
            match found {
                Some((id, user)) => Ok(WhoIs {
                    node_stable_id: id,
                    user: WhoIsUser { id: user, login_name: "tester".into() },
                }),
                None => Err(TsError::PeerNotFound(addr)),
            }
        })
    }
}

fn test_opts() -> NetOpts {
    NetOpts {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        dial_timeout: Duration::from_millis(500),
        handshake_timeout: Duration::from_secs(2),
        ..Default::default()
    }
}

async fn spawn_node(id: &str, whois_peer: &str, opts: NetOpts) -> (NetManager, NetControl) {
    let info = MachineInfo {
        build: splice_proto::BuildInfo::current(),
        id: MachineId(id.into()),
        hostname: id.into(),
        os: Os::Linux,
        displays: vec![],
    };
    let ts = FakeTs {
        self_id: id.into(),
        user_id: 7,
        whois: Arc::new(HashMap::from([(LOCAL, (whois_peer.to_string(), 7))])),
    };
    NetManager::spawn_with(info, SocketAddr::new(LOCAL, 0), Arc::new(ts), opts)
        .await
        .expect("spawn")
}

/// Spawn "aaa" and "bbb", wire dial ports, and point both at each other.
async fn pair(a_opts: NetOpts, b_opts: NetOpts) -> ((NetManager, NetControl), (NetManager, NetControl)) {
    let ports_a = a_opts.dial_ports.clone();
    let ports_b = b_opts.dial_ports.clone();
    let (a, ca) = spawn_node("aaa", "bbb", a_opts).await;
    let (b, cb) = spawn_node("bbb", "aaa", b_opts).await;
    ports_a
        .write()
        .unwrap()
        .insert(MachineId("bbb".into()), b.local_addr.port());
    ports_b
        .write()
        .unwrap()
        .insert(MachineId("aaa".into()), a.local_addr.port());
    ca.update_dial_targets(vec![(MachineId("bbb".into()), LOCAL)]);
    cb.update_dial_targets(vec![(MachineId("aaa".into()), LOCAL)]);
    ((a, ca), (b, cb))
}

async fn wait_for(
    m: &mut NetManager,
    within: Duration,
    pred: impl Fn(&PeerEvent) -> bool,
) -> PeerEvent {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for event");
        let ev = tokio::time::timeout(left, m.events.recv())
            .await
            .expect("timed out waiting for event")
            .expect("event channel closed");
        if pred(&ev) {
            return ev;
        }
    }
}

fn connected(peer: &str) -> impl Fn(&PeerEvent) -> bool {
    let p = MachineId(peer.into());
    move |ev| matches!(ev, PeerEvent::Connected { id, .. } if *id == p)
}

fn disconnected(peer: &str) -> impl Fn(&PeerEvent) -> bool {
    let p = MachineId(peer.into());
    move |ev| matches!(ev, PeerEvent::Disconnected(id, _) if *id == p)
}

/// During `dur`, no new connection churn may happen: no Disconnected at all, and no
/// Connected for a peer already marked seen. (A duplicate dial resolving may produce
/// one repeat Connected per side before the dup is closed.)
async fn assert_stable(m: &mut NetManager, dur: Duration, seen: &[&str]) {
    let deadline = Instant::now() + dur;
    let mut repeats: HashMap<String, u32> = HashMap::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        match tokio::time::timeout(left, m.events.recv()).await {
            Ok(Some(PeerEvent::Connected { id, .. })) => {
                assert!(seen.iter().any(|s| *s == id.0), "unexpected Connected({id})");
                let n = repeats.entry(id.0.clone()).or_insert(0);
                *n += 1;
                assert!(*n <= 1, "repeated Connected({id}) — dedupe not converging");
            }
            Ok(Some(PeerEvent::Disconnected(id, r))) => {
                panic!("unexpected Disconnected({id}, {r})")
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("event channel closed"),
            Err(_) => return,
        }
    }
}

#[tokio::test]
async fn connect_with_dedupe() {
    // Rule case: both sides list each other, only the smaller id dials.
    let ((mut a, _ca), (mut b, _cb)) = pair(test_opts(), test_opts()).await;
    let ev = wait_for(&mut a, Duration::from_secs(3), connected("bbb")).await;
    match ev {
        PeerEvent::Connected { caps, hello, .. } => {
            assert!(caps.iter().any(|c| c == caps::INPUT_V1));
            assert_eq!(hello.hostname, "bbb");
        }
        _ => unreachable!(),
    }
    wait_for(&mut b, Duration::from_secs(3), connected("aaa")).await;
    assert_stable(&mut a, Duration::from_millis(300), &[]).await;
    assert_stable(&mut b, Duration::from_millis(300), &[]).await;

    // Forced duplicate: the larger id dials anyway. The smaller-dialer connection
    // must win; the dup closes quietly; the surviving link works both ways.
    let mut b_opts = test_opts();
    b_opts.force_dial = true;
    let ((mut a2, ca2), (mut b2, cb2)) = pair(test_opts(), b_opts).await;
    wait_for(&mut a2, Duration::from_secs(3), connected("bbb")).await;
    wait_for(&mut b2, Duration::from_secs(3), connected("aaa")).await;
    assert_stable(&mut a2, Duration::from_millis(600), &["bbb"]).await;
    assert_stable(&mut b2, Duration::from_millis(600), &["aaa"]).await;
    assert!(ca2.send_to(&MachineId("bbb".into()), Frame::ReleaseAll));
    wait_for(&mut b2, Duration::from_secs(2), |ev| {
        matches!(ev, PeerEvent::Frame(id, Frame::ReleaseAll) if id.0 == "aaa")
    })
    .await;
    assert!(cb2.send_to(&MachineId("aaa".into()), Frame::ReleaseAll));
    wait_for(&mut a2, Duration::from_secs(2), |ev| {
        matches!(ev, PeerEvent::Frame(id, Frame::ReleaseAll) if id.0 == "bbb")
    })
    .await;
}

#[tokio::test]
async fn dropping_the_manager_closes_sessions_and_releases_the_listener() {
    let ((a, _ca), (mut b, _cb)) = pair(test_opts(), test_opts()).await;
    wait_for(&mut b, Duration::from_secs(2), connected("aaa")).await;
    let address = a.local_addr;
    drop(a);
    wait_for(&mut b, Duration::from_secs(2), disconnected("aaa")).await;
    assert!(tokio::net::TcpListener::bind(address).await.is_ok());
}

#[tokio::test]
async fn entering_a_session_starts_active_heartbeats_immediately() {
    let a_opts = NetOpts {
        idle_hb: Duration::from_secs(10),
        active_hb: Duration::from_millis(20),
        max_misses: 2,
        ..test_opts()
    };
    let b_opts = test_opts();
    b_opts.answer_pings.store(false, Ordering::Relaxed);
    let ((mut a, ca), (_b, _cb)) = pair(a_opts, b_opts).await;
    wait_for(&mut a, Duration::from_secs(2), connected("bbb")).await;
    ca.set_active(&MachineId("bbb".into()), true);
    wait_for(&mut a, Duration::from_millis(300), |event| matches!(event, PeerEvent::Degraded(_))).await;
}

#[tokio::test]
async fn sending_welcome_does_not_mark_an_unconfirmed_peer_connected() {
    use splice_proto::framing::{read_frame, write_frame};
    let opts = NetOpts { handshake_timeout: Duration::from_millis(150), ..test_opts() };
    let (mut b, _control) = spawn_node("bbb", "aaa", opts).await;
    let mut socket = tokio::net::TcpStream::connect(b.local_addr).await.unwrap();
    write_frame(
        &mut socket,
        &Frame::Hello(splice_proto::Hello {
            proto_min: splice_proto::PROTO_VERSION,
            proto_max: splice_proto::PROTO_VERSION,
            machine: MachineInfo {
                build: splice_proto::BuildInfo::current(),
                id: MachineId("aaa".into()),
                hostname: "aaa".into(),
                os: Os::Linux,
                displays: vec![],
            },
            caps: [caps::INPUT_V1, caps::CLIPBOARD_V2, caps::LAYOUT_V1, caps::MASTER_V1].map(str::to_string).to_vec(),
        }),
    )
    .await
    .unwrap();
    assert!(matches!(read_frame(&mut socket).await.unwrap(), Frame::Welcome(_)));
    let event = tokio::time::timeout(Duration::from_millis(400), b.events.recv()).await.unwrap().unwrap();
    assert!(matches!(event, PeerEvent::Rejected { .. }), "unconfirmed connection was published: {event:?}");
}

#[tokio::test]
async fn disjoint_proto_ranges_refused() {
    let a_opts = NetOpts { proto_min: 1, proto_max: 1, ..test_opts() };
    let b_opts = NetOpts { proto_min: 2, proto_max: 2, ..test_opts() };
    let ((mut a, _ca), (mut b, _cb)) = pair(a_opts, b_opts).await;
    // The listener answers Bye and closes; nobody ever becomes Connected, and the
    // dialer keeps retrying (backoff) without surfacing phantom events.
    let quiet = Duration::from_millis(600);
    let deadline = Instant::now() + quiet;
    for m in [&mut a, &mut b] {
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if let Ok(Some(ev)) = tokio::time::timeout(left, m.events.recv()).await {
                assert!(
                    !matches!(ev, PeerEvent::Connected { .. }),
                    "unexpected Connected on disjoint versions"
                );
            }
        }
    }
}

#[tokio::test]
async fn degraded_then_healthy_when_pongs_resume() {
    let fast = NetOpts {
        idle_hb: Duration::from_millis(20),
        active_hb: Duration::from_millis(20),
        ..test_opts()
    };
    let b_opts = fast.clone();
    let b_gate = b_opts.answer_pings.clone();
    let ((mut a, _ca), (mut b, _cb)) = pair(fast, b_opts).await;
    wait_for(&mut a, Duration::from_secs(3), connected("bbb")).await;
    wait_for(&mut b, Duration::from_secs(3), connected("aaa")).await;

    // Silence b's Pong replies: a must degrade but keep the socket.
    b_gate.store(false, Ordering::SeqCst);
    wait_for(&mut a, Duration::from_secs(3), |ev| {
        matches!(ev, PeerEvent::Degraded(id) if id.0 == "bbb")
    })
    .await;

    // Resume replies: the next answered Ping recovers the link with an RTT.
    b_gate.store(true, Ordering::SeqCst);
    let ev = wait_for(&mut a, Duration::from_secs(3), |ev| {
        matches!(ev, PeerEvent::Healthy(id, _) if id.0 == "bbb")
    })
    .await;
    match ev {
        PeerEvent::Healthy(_, rtt_ms) => assert!(rtt_ms >= 0.0),
        _ => unreachable!(),
    }
    // The socket was never closed: the same link still carries frames.
    let ca = _ca;
    assert!(ca.send_to(&MachineId("bbb".into()), Frame::ReleaseAll));
    wait_for(&mut b, Duration::from_secs(2), |ev| {
        matches!(ev, PeerEvent::Frame(id, Frame::ReleaseAll) if id.0 == "aaa")
    })
    .await;
}

#[tokio::test]
async fn send_to_broadcast_and_machine_update() {
    let ((mut a, ca), (mut b, cb)) = pair(test_opts(), test_opts()).await;
    wait_for(&mut a, Duration::from_secs(3), connected("bbb")).await;
    wait_for(&mut b, Duration::from_secs(3), connected("aaa")).await;

    assert!(!ca.send_to(&MachineId("zzz".into()), Frame::ReleaseAll));
    assert!(ca.send_to(&MachineId("bbb".into()), Frame::ReleaseAll));
    wait_for(&mut b, Duration::from_secs(2), |ev| {
        matches!(ev, PeerEvent::Frame(id, Frame::ReleaseAll) if id.0 == "aaa")
    })
    .await;

    cb.broadcast(Frame::ReleaseAll);
    wait_for(&mut a, Duration::from_secs(2), |ev| {
        matches!(ev, PeerEvent::Frame(id, Frame::ReleaseAll) if id.0 == "bbb")
    })
    .await;

    let info = MachineInfo {
        build: splice_proto::BuildInfo::current(),
        id: MachineId("aaa".into()),
        hostname: "renamed".into(),
        os: Os::Macos,
        displays: vec![],
    };
    ca.update_self(info);
    wait_for(&mut b, Duration::from_secs(2), |ev| {
        matches!(
            ev,
            PeerEvent::Frame(id, Frame::MachineUpdate(mi))
                if id.0 == "aaa" && mi.hostname == "renamed"
        )
    })
    .await;
}

#[tokio::test]
async fn removing_dial_target_disconnects() {
    let ((mut a, ca), (mut b, _cb)) = pair(test_opts(), test_opts()).await;
    wait_for(&mut a, Duration::from_secs(3), connected("bbb")).await;
    wait_for(&mut b, Duration::from_secs(3), connected("aaa")).await;

    ca.update_dial_targets(vec![]);
    let ea = wait_for(&mut a, Duration::from_secs(3), disconnected("bbb")).await;
    let eb = wait_for(&mut b, Duration::from_secs(3), disconnected("aaa")).await;
    for ev in [&ea, &eb] {
        match ev {
            PeerEvent::Disconnected(_, reason) => assert_eq!(reason, "peer no longer listed"),
            _ => unreachable!(),
        }
    }
    assert!(!ca.send_to(&MachineId("bbb".into()), Frame::ReleaseAll));
}

#[tokio::test]
async fn prolonged_silence_drops_the_socket_and_redials() {
    let fast = NetOpts {
        idle_hb: Duration::from_millis(20),
        active_hb: Duration::from_millis(20),
        degraded_timeout: Duration::from_millis(150),
        ..test_opts()
    };
    let b_opts = fast.clone();
    let b_gate = b_opts.answer_pings.clone();
    let ((mut a, _ca), (mut b, _cb)) = pair(fast, b_opts).await;
    wait_for(&mut a, Duration::from_secs(3), connected("bbb")).await;
    wait_for(&mut b, Duration::from_secs(3), connected("aaa")).await;

    b_gate.store(false, Ordering::SeqCst);
    wait_for(&mut a, Duration::from_secs(3), |ev| {
        matches!(ev, PeerEvent::Degraded(id) if id.0 == "bbb")
    })
    .await;
    wait_for(&mut a, Duration::from_secs(3), disconnected("bbb")).await;

    b_gate.store(true, Ordering::SeqCst);
    wait_for(&mut a, Duration::from_secs(3), connected("bbb")).await;
    wait_for(&mut b, Duration::from_secs(3), connected("aaa")).await;
}

#[tokio::test]
async fn an_unmatched_pong_cannot_recover_a_degraded_peer() {
    let a_opts = NetOpts {
        idle_hb: Duration::from_millis(20),
        max_misses: 2,
        degraded_timeout: Duration::from_millis(150),
        ..test_opts()
    };
    let b_opts = test_opts();
    b_opts.answer_pings.store(false, Ordering::Relaxed);
    let ((mut a, _ca), (mut b, cb)) = pair(a_opts, b_opts).await;
    wait_for(&mut a, Duration::from_secs(2), connected("bbb")).await;
    wait_for(&mut b, Duration::from_secs(2), connected("aaa")).await;
    wait_for(&mut a, Duration::from_secs(2), |event| matches!(event, PeerEvent::Degraded(_))).await;
    cb.send_to(&MachineId("aaa".into()), Frame::Pong { nonce: u64::MAX, t_us: 0 });
    let event = wait_for(&mut a, Duration::from_secs(1), |event| {
        matches!(event, PeerEvent::Healthy(..) | PeerEvent::Disconnected(..))
    })
    .await;
    assert!(matches!(event, PeerEvent::Disconnected(..)), "unmatched pong falsely restored health: {event:?}");
}
