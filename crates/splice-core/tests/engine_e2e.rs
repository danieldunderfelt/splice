//! Engine end-to-end tests: two in-process engines over loopback TCP with mock
//! platforms and a fake TsApi (same pattern as tests/net_session.rs). MachineIds
//! "aaa"/"bbb" make "aaa" the rule-following dialer; both dial ports are wired
//! explicitly after each engine reports its bound address.

use splice_core::engine::{Command, Engine, EngineHandle};
use splice_core::net::{NetOpts, TsApi};
use splice_core::ui_state::{UiConnection, UiFocus};
use splice_platform::mock::{self, MockHandle};
use splice_platform::{CaptureEvent, EdgeSide, EdgeSpec, PlatformEvent};
use splice_proto::{InputEvent, MachineId, Vec2I};
use splice_tailscale::{Node, Status, TsError, WhoIs, WhoIsUser};
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USER: u64 = 7;
const TEXT_MIME: &str = "text/plain;charset=utf-8";

#[derive(Clone)]
struct FakeTs {
    self_node: Node,
    peers: Vec<Node>,
    whois: Arc<HashMap<IpAddr, (String, u64)>>,
}

impl TsApi for FakeTs {
    fn status(&self) -> Pin<Box<dyn Future<Output = Result<Status, TsError>> + Send + '_>> {
        let (self_node, peers) = (self.self_node.clone(), self.peers.clone());
        Box::pin(async move { Ok(Status { self_node, peers }) })
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

fn node(id: &str) -> Node {
    node_at(id, Ipv4Addr::LOCALHOST)
}

fn node_at(id: &str, ip: Ipv4Addr) -> Node {
    Node {
        stable_id: id.into(),
        hostname: format!("host-{id}"),
        os: "linux".into(),
        user_id: USER,
        ips: vec![IpAddr::V4(ip)],
        online: true,
        cur_addr: "127.0.0.1:41641".into(),
        ..Default::default()
    }
}

fn mid(id: &str) -> MachineId {
    MachineId(id.into())
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

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "splice-e2e-{}-{}-{}",
        std::process::id(),
        tag,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    handle: EngineHandle,
    mock: MockHandle,
    dial_ports: Arc<std::sync::RwLock<HashMap<MachineId, u16>>>,
    addr: SocketAddr,
}

async fn spawn_rig(id: &str, peer: &Node) -> Rig {
    spawn_rig_with(node(id), vec![peer.clone()]).await
}

async fn spawn_rig_with(self_node: Node, peers: Vec<Node>) -> Rig {
    let (platform, mock) = mock::create(mock::one_display());
    let whois = peers
        .iter()
        .filter_map(|peer| peer.ips.first().map(|ip| (*ip, (peer.stable_id.clone(), USER))))
        .collect();
    let ts = FakeTs {
        self_node: self_node.clone(),
        peers,
        whois: Arc::new(whois),
    };
    let opts = test_opts();
    let dial_ports = opts.dial_ports.clone();
    let dir = temp_dir(&self_node.stable_id);
    let handle = Engine::spawn_with(
        platform,
        Arc::new(ts),
        dir.clone(),
        opts,
        Duration::from_millis(50),
    )
    .await
    .expect("spawn engine");
    let addr = handle.bound_addr().await.expect("bootstrap binds a listener");
    Rig { handle, mock, dial_ports, addr }
}

async fn spawn_trio() -> (Rig, Rig, Rig) {
    let na = node_at("aaa", Ipv4Addr::new(127, 0, 0, 1));
    let nb = node_at("bbb", Ipv4Addr::new(127, 0, 0, 2));
    let nc = node_at("ccc", Ipv4Addr::new(127, 0, 0, 3));
    let a = spawn_rig_with(na.clone(), vec![nb.clone(), nc.clone()]).await;
    let b = spawn_rig_with(nb.clone(), vec![na.clone(), nc.clone()]).await;
    let c = spawn_rig_with(nc, vec![na, nb]).await;
    for (rig, peers) in [
        (&a, [(&mid("bbb"), &b), (&mid("ccc"), &c)]),
        (&b, [(&mid("aaa"), &a), (&mid("ccc"), &c)]),
        (&c, [(&mid("aaa"), &a), (&mid("bbb"), &b)]),
    ] {
        let mut ports = rig.dial_ports.write().unwrap();
        for (id, peer) in peers {
            ports.insert(id.clone(), peer.addr.port());
        }
    }
    // aaa is lexicographically first and establishes the two hub links needed
    // for replication. (Loopback cannot model three distinct Tailscale WhoIs
    // source identities for the optional bbb<->ccc connection.)
    wait_until("trio hub sessions connect", || {
        connected_to(&a, "bbb")
            && connected_to(&a, "ccc")
            && connected_to(&b, "aaa")
            && connected_to(&c, "aaa")
    })
    .await;
    a.handle.send(Command::SetPlacement(mid("aaa"), Vec2I { x: 0, y: 0 }));
    a.handle.send(Command::SetPlacement(mid("bbb"), Vec2I { x: 1920, y: 0 }));
    a.handle.send(Command::SetPlacement(mid("ccc"), Vec2I { x: 3840, y: 0 }));
    wait_until("trio layout converges", || {
        let state = a.handle.state();
        let state = state.borrow();
        state.machines.iter().find(|m| m.id == mid("bbb")).is_some_and(|m| {
            m.offset == Vec2I { x: 1920, y: 0 }
        }) && state.machines.iter().find(|m| m.id == mid("ccc")).is_some_and(|m| {
            m.offset == Vec2I { x: 3840, y: 0 }
        })
    })
    .await;
    (a, b, c)
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if f() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn connected_to(rig: &Rig, peer: &str) -> bool {
    rig.handle.state().borrow().machines.iter().any(|m| {
        m.id.0 == peer
            && matches!(m.connection, UiConnection::Direct { .. } | UiConnection::Derp { .. })
    })
}

fn focus_of(rig: &Rig) -> UiFocus {
    rig.handle.state().borrow().focus.clone()
}

/// Two engines, dial ports wired, mutually connected.
async fn spawn_pair() -> (Rig, Rig) {
    let a = spawn_rig("aaa", &node("bbb")).await;
    let b = spawn_rig("bbb", &node("aaa")).await;
    a.dial_ports.write().unwrap().insert(mid("bbb"), b.addr.port());
    b.dial_ports.write().unwrap().insert(mid("aaa"), a.addr.port());
    wait_until("aaa sees bbb connected", || connected_to(&a, "bbb")).await;
    wait_until("bbb sees aaa connected", || connected_to(&b, "aaa")).await;
    (a, b)
}

/// A claims sourceness and drives the cursor onto B. Returns the armed edge used
/// and the along-position of the crossing.
async fn drive_a_to_b(a: &Rig, b: &Rig) -> (EdgeSpec, f64) {
    wait_until("A arms an edge toward B", || !a.mock.state.lock().edges.is_empty()).await;
    a.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    let edge = a.mock.state.lock().edges[0].clone();
    let along = f64::from(edge.from + edge.to) / 2.0;
    a.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: edge.id, along }))
        .unwrap();
    wait_until("A is capturing", || a.mock.state.lock().capturing).await;
    wait_until("B entered", || !b.mock.state.lock().entered.is_empty()).await;
    (edge, along)
}

/// Signed dx that moves the virtual cursor deeper INTO B (away from the shared edge).
fn into_sign(edge: &EdgeSpec) -> f64 {
    match edge.side {
        EdgeSide::Left => -1.0,
        EdgeSide::Right => 1.0,
        other => panic!("expected a vertical shared edge, got {other:?}"),
    }
}

fn push_motion(rig: &Rig, dx: f64, dy: f64) {
    rig.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Motion { dx, dy })))
        .unwrap();
}

fn push_key(rig: &Rig, code: u32, pressed: bool) {
    rig.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::Input(InputEvent::Key { code, pressed })))
        .unwrap();
}

fn injected(rig: &Rig) -> Vec<InputEvent> {
    rig.mock.state.lock().injected.clone()
}

#[tokio::test]
async fn enter_maps_position_and_captures() {
    let (a, b) = spawn_pair().await;
    let (_edge, along) = drive_a_to_b(&a, &b).await;

    assert!(a.mock.state.lock().capturing);
    let entered = b.mock.state.lock().entered.clone();
    assert_eq!(entered.len(), 1);
    // 1:1 along the shared span; landing on B's boundary facing A.
    assert_eq!(entered[0].y, along);
    assert!(entered[0].x == 0.0 || entered[0].x == 1920.0, "entered at {entered:?}");

    wait_until("A reports focus Remote(bbb)", || {
        matches!(focus_of(&a), UiFocus::Remote(t) if t.0 == "bbb")
    })
    .await;
    wait_until("B reports focus Driven(aaa)", || {
        matches!(focus_of(&b), UiFocus::Driven(s) if s.0 == "aaa")
    })
    .await;
}

#[tokio::test]
async fn motion_forwarding_and_edge_back() {
    let (a, b) = spawn_pair().await;
    let (edge, _along) = drive_a_to_b(&a, &b).await;

    // Deeper into B: scaled (1.0) deltas are forwarded verbatim to the target.
    let into = into_sign(&edge);
    push_motion(&a, 25.0 * into, 5.0);
    let want = InputEvent::Motion { dx: 25.0 * into, dy: 5.0 };
    wait_until("B injected the forwarded motion", || injected(&b).contains(&want)).await;
    assert!(a.mock.state.lock().capturing);

    // Enough delta back through the shared edge: A warps its own cursor back,
    // B gets Leave and force-releases.
    push_motion(&a, -4000.0 * into, 0.0);
    wait_until("A stopped capturing", || !a.mock.state.lock().capturing).await;
    let ends = a.mock.state.lock().capture_ends.clone();
    let Some(Some(warp)) = ends.last().copied() else {
        panic!("expected an orderly end_capture with a warp target, got {ends:?}");
    };
    assert!((0.0..=1920.0).contains(&warp.x) && (0.0..=1080.0).contains(&warp.y));
    wait_until("B released and left", || {
        let st = b.mock.state.lock();
        st.left >= 1 && st.release_all_calls >= 1
    })
    .await;
    wait_until("A back to Local focus", || matches!(focus_of(&a), UiFocus::Local)).await;
}

#[tokio::test]
async fn held_key_released_on_leave_and_on_peer_drop() {
    let (a, b) = spawn_pair().await;
    let (edge, _along) = drive_a_to_b(&a, &b).await;

    // Key down on A (no release), then an orderly cross-back: B sees the key-up
    // forwarded before the Leave, and its backend force-releases regardless.
    push_key(&a, 30, true);
    let down = InputEvent::Key { code: 30, pressed: true };
    wait_until("B injected key down", || injected(&b).contains(&down)).await;
    push_motion(&a, -4000.0 * into_sign(&edge), 0.0);
    let up = InputEvent::Key { code: 30, pressed: false };
    wait_until("B injected drained key-up", || injected(&b).contains(&up)).await;
    wait_until("B force-released and left", || {
        let st = b.mock.state.lock();
        st.release_all_calls >= 1 && st.left >= 1
    })
    .await;

    // Re-enter, hold another key, then B vanishes entirely: A must return to
    // Local on the disconnect without hanging.
    wait_until("A re-arms edges", || !a.mock.state.lock().edges.is_empty()).await;
    let edge = a.mock.state.lock().edges[0].clone();
    let along = f64::from(edge.from + edge.to) / 2.0;
    a.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: edge.id, along }))
        .unwrap();
    wait_until("A capturing again", || a.mock.state.lock().capturing).await;
    wait_until("B entered again", || b.mock.state.lock().entered.len() >= 2).await;
    push_key(&a, 42, true);
    let down2 = InputEvent::Key { code: 42, pressed: true };
    wait_until("B injected second key down", || injected(&b).contains(&down2)).await;

    drop(b.handle);
    wait_until("A released capture after B dropped", || !a.mock.state.lock().capturing)
        .await;
    wait_until("A back to Local after drop", || matches!(focus_of(&a), UiFocus::Local))
        .await;
}

#[tokio::test]
async fn source_claim_ends_remote_capture() {
    let (a, b) = spawn_pair().await;
    drive_a_to_b(&a, &b).await;

    // B produces physical input while driven: it claims sourceness; A must end
    // capture (Leave SourceChanged reaches B, which releases and leaves).
    b.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    wait_until("A ended capture", || !a.mock.state.lock().capturing).await;
    let ends = a.mock.state.lock().capture_ends.clone();
    assert_eq!(ends.last(), Some(&None), "sourceness loss ends capture without warp");
    wait_until("B left the driven session", || b.mock.state.lock().left >= 1).await;
    wait_until("both sides agree bbb is source", || {
        a.handle.state().borrow().source == Some(mid("bbb"))
            && b.handle.state().borrow().source == Some(mid("bbb"))
    })
    .await;
    wait_until("A back to Local", || matches!(focus_of(&a), UiFocus::Local)).await;
}

#[tokio::test]
async fn source_claims_created_before_connection_converge() {
    let a = spawn_rig("aaa", &node("bbb")).await;
    let b = spawn_rig("bbb", &node("aaa")).await;

    // Reproduce a partition: both machines observe local input before they can
    // reach one another, so each starts out believing it is the source.
    a.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    b.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    wait_until("both machines hold independent self-claims", || {
        a.handle.state().borrow().source == Some(mid("aaa"))
            && b.handle.state().borrow().source == Some(mid("bbb"))
    })
    .await;

    // Connecting must exchange the current claims even before either machine
    // produces another input event. Equal Lamport values converge by writer ID.
    a.dial_ports.write().unwrap().insert(mid("bbb"), b.addr.port());
    b.dial_ports.write().unwrap().insert(mid("aaa"), a.addr.port());
    wait_until("both sides connect", || connected_to(&a, "bbb") && connected_to(&b, "aaa"))
        .await;
    wait_until("both sides converge on one source", || {
        let a_source = a.handle.state().borrow().source.clone();
        let b_source = b.handle.state().borrow().source.clone();
        a_source.is_some() && a_source == b_source
    })
    .await;

    // New physical activity supersedes the converged claim and is propagated
    // before a subsequent Enter frame would be sent.
    a.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    wait_until("fresh activity makes aaa source everywhere", || {
        a.handle.state().borrow().source == Some(mid("aaa"))
            && b.handle.state().borrow().source == Some(mid("aaa"))
    })
    .await;
}

#[tokio::test]
async fn disabling_focused_target_returns_both_sides_to_source_and_keeps_barrier_stable() {
    let (a, b) = spawn_pair().await;
    drive_a_to_b(&a, &b).await;
    let barriers = a.mock.state.lock().edges.clone();

    a.handle.send(Command::SetMachineEnabled(mid("bbb"), false));
    wait_until("disabled target is local on both sides", || {
        matches!(focus_of(&a), UiFocus::Local)
            && matches!(focus_of(&b), UiFocus::Local)
            && !a.mock.state.lock().capturing
    })
    .await;
    wait_until("disable replicated to target", || {
        b.handle
            .state()
            .borrow()
            .machines
            .iter()
            .find(|m| m.id == mid("bbb"))
            .is_some_and(|m| !m.enabled)
    })
    .await;
    assert_eq!(
        a.mock.state.lock().edges,
        barriers,
        "enable/focus state must not recreate physical portal barriers"
    );
}

#[tokio::test]
async fn disabling_source_ends_the_whole_two_machine_session() {
    let (a, b) = spawn_pair().await;
    drive_a_to_b(&a, &b).await;

    a.handle.send(Command::SetMachineEnabled(mid("aaa"), false));
    wait_until("disabled source tears down both roles", || {
        matches!(focus_of(&a), UiFocus::Local)
            && matches!(focus_of(&b), UiFocus::Local)
            && !a.mock.state.lock().capturing
            && b.mock.state.lock().left >= 1
    })
    .await;
}

#[tokio::test]
async fn disabled_barrier_activation_is_immediately_released() {
    let (a, _b) = spawn_pair().await;
    wait_until("A has physical barrier", || !a.mock.state.lock().edges.is_empty()).await;
    let edge = a.mock.state.lock().edges[0].clone();
    a.handle.send(Command::SetMachineEnabled(mid("bbb"), false));
    wait_until("B disabled on A", || {
        a.handle
            .state()
            .borrow()
            .machines
            .iter()
            .find(|m| m.id == mid("bbb"))
            .is_some_and(|m| !m.enabled)
    })
    .await;

    // Model the portal's ordering: it captures first, then reports EdgeHit.
    a.mock.state.lock().capturing = true;
    let ends_before = a.mock.state.lock().capture_ends.len();
    a.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::EdgeHit {
            edge_id: edge.id,
            along: f64::from(edge.from + edge.to) / 2.0,
        }))
        .unwrap();
    wait_until("rejected activation released", || {
        let state = a.mock.state.lock();
        !state.capturing && state.capture_ends.len() > ends_before
    })
    .await;
    assert!(matches!(focus_of(&a), UiFocus::Local));
}

#[tokio::test]
async fn disabling_unfocused_third_machine_preserves_active_pair() {
    let (a, b, c) = spawn_trio().await;
    drive_a_to_b(&a, &b).await;

    a.handle.send(Command::SetMachineEnabled(mid("ccc"), false));
    wait_until("third-machine disable converges", || {
        [&a, &c].iter().all(|rig| {
            rig.handle
                .state()
                .borrow()
                .machines
                .iter()
                .find(|m| m.id == mid("ccc"))
                .is_some_and(|m| !m.enabled)
        })
    })
    .await;
    assert!(matches!(focus_of(&a), UiFocus::Remote(id) if id == mid("bbb")));
    assert!(matches!(focus_of(&b), UiFocus::Driven(id) if id == mid("aaa")));
    assert!(a.mock.state.lock().capturing);

    // Disabling the focused target still returns focus to the source.
    a.handle.send(Command::SetMachineEnabled(mid("bbb"), false));
    wait_until("focused target disable tears down active pair", || {
        matches!(focus_of(&a), UiFocus::Local)
            && matches!(focus_of(&b), UiFocus::Local)
            && !a.mock.state.lock().capturing
    })
    .await;
}

#[tokio::test]
async fn clipboard_offer_and_lazy_fetch() {
    let (a, b) = spawn_pair().await;
    let payload = b"hello clipboard".to_vec();
    a.mock.state.lock().local_clip.insert(TEXT_MIME.into(), payload.clone());

    a.mock
        .events
        .send(PlatformEvent::ClipboardChanged {
            mimes: vec![TEXT_MIME.into()],
            inline_text: Some("hello clipboard".into()),
        })
        .unwrap();
    wait_until("B applied the remote offer", || {
        !b.mock.state.lock().remote_offers.is_empty()
    })
    .await;
    let offer = b.mock.state.lock().remote_offers[0].clone();
    assert_eq!(offer.mimes, vec![TEXT_MIME.to_string()]);
    assert_eq!(offer.inline_text.as_deref(), Some("hello clipboard"));

    // Simulate a paste on B: the stored fetch pulls the bytes from A lazily.
    let fetch = b.mock.last_fetch.lock().clone().expect("fetch callback installed");
    let data = fetch.fetch(TEXT_MIME).await.expect("origin serves the representation");
    assert_eq!(data, payload);
}
