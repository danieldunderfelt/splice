//! Engine end-to-end tests: two in-process engines over loopback TCP with mock
//! platforms and a fake TsApi (same pattern as tests/net_session.rs). MachineIds
//! "aaa"/"bbb" make "aaa" the rule-following dialer; both dial ports are wired
//! explicitly after each engine reports its bound address.

use splice_core::engine::{Command, Engine, EngineHandle};
use splice_core::net::{NetOpts, TsApi};
use splice_core::ui_state::{UiConnection, UiFocus};
use splice_platform::mock::{self, MockHandle};
use splice_platform::{CaptureEvent, EdgeSide, EdgeSpec, PlatformEvent};
use splice_proto::{DisplayRect, InputEvent, LayoutDoc, MachineId, Vec2I};
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
    data_dir: PathBuf,
}

async fn spawn_rig(id: &str, peer: &Node) -> Rig {
    spawn_rig_with(node(id), vec![peer.clone()]).await
}

async fn spawn_rig_with(self_node: Node, peers: Vec<Node>) -> Rig {
    spawn_rig_configured(self_node, peers, splice_core::config::Config::default()).await
}

async fn spawn_rig_configured(self_node: Node, peers: Vec<Node>, config: splice_core::config::Config) -> Rig {
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
    splice_core::config::save(&dir, &config).unwrap();
    let handle = Engine::spawn_with(platform, Arc::new(ts), dir.clone(), opts, Duration::from_millis(50))
        .await
        .expect("spawn engine");
    let addr = handle.bound_addr().await.expect("bootstrap binds a listener");
    Rig { handle, mock, dial_ports, addr, data_dir: dir }
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
    wait_until("all three machines connect to each other", || {
        connected_to(&a, "bbb")
            && connected_to(&a, "ccc")
            && connected_to(&b, "aaa")
            && connected_to(&b, "ccc")
            && connected_to(&c, "aaa")
            && connected_to(&c, "bbb")
    })
    .await;
    a.handle.send(Command::SetArrangement(vec![
        (mid("aaa"), Vec2I { x: 0, y: 0 }),
        (mid("bbb"), Vec2I { x: 1920, y: 0 }),
        (mid("ccc"), Vec2I { x: 3840, y: 0 }),
    ]));
    wait_until("trio layout converges", || {
        [&a, &b, &c].iter().all(|rig| {
            let state = rig.handle.state();
            let state = state.borrow();
            state.edges.len() == 2
                && state.edges.iter().all(|edge| edge.crossable)
                && ["aaa", "bbb", "ccc"].iter().enumerate().all(|(index, id)| {
                    state.machines.iter().any(|m| m.id == mid(id) && m.offset == Vec2I { x: index as i32 * 1920, y: 0 })
                })
        })
    })
    .await;
    (a, b, c)
}

#[tokio::test]
async fn three_machines_share_a_workspace_and_keep_every_connection() {
    let (a, b, c) = spawn_trio().await;
    wait_until("every machine sees the same three-machine row", || {
        [&a, &b, &c].iter().all(|rig| {
            let state = rig.handle.state();
            let state = state.borrow();
            ["aaa", "bbb", "ccc"].iter().enumerate().all(|(index, id)| {
                state.machines.iter().any(|m| m.id == mid(id) && m.offset == Vec2I { x: index as i32 * 1920, y: 0 })
            })
        })
    })
    .await;
    let until = Instant::now() + Duration::from_millis(6250);
    while Instant::now() < until {
        assert!([(&a, ["bbb", "ccc"]), (&b, ["aaa", "ccc"]), (&c, ["aaa", "bbb"])]
            .iter()
            .all(|(rig, peers)| peers.iter().all(|id| connected_to(rig, id))));
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn concurrent_clipboard_fetches_from_two_peers_do_not_collide() {
    let (a, b, c) = spawn_trio().await;
    let mime = "image/png";
    a.mock.state.lock().local_clip.insert(mime.into(), b"first computer".to_vec());
    c.mock.state.lock().local_clip.insert(mime.into(), b"third computer".to_vec());
    a.mock.events.send(PlatformEvent::ClipboardChanged { mimes: vec![mime.into()], inline_text: None }).unwrap();
    wait_until("first clipboard offer", || b.mock.last_fetch.lock().is_some()).await;
    let first = b.mock.last_fetch.lock().clone().unwrap();
    wait_until("third computer observes first offer", || c.mock.last_fetch.lock().is_some()).await;
    c.mock.events.send(PlatformEvent::ClipboardChanged { mimes: vec![mime.into()], inline_text: None }).unwrap();
    wait_until("second clipboard offer", || b.mock.state.lock().remote_offers.len() == 2).await;
    let second = b.mock.last_fetch.lock().clone().unwrap();
    let (one, two) = tokio::join!(first.fetch(mime), second.fetch(mime));
    assert_eq!(one, Some(b"first computer".to_vec()));
    assert_eq!(two, Some(b"third computer".to_vec()));
}

#[tokio::test]
async fn panic_on_the_third_machine_releases_the_active_pair() {
    let (a, b, c) = spawn_trio().await;
    drive_a_to_b(&a, &b).await;
    c.handle.send(Command::Panic);
    wait_until("third-machine panic releases both source and target", || {
        matches!(focus_of(&a), UiFocus::Local)
            && matches!(focus_of(&b), UiFocus::Local)
            && !a.mock.state.lock().capturing
    })
    .await;
}

#[tokio::test]
async fn a_disabled_third_machine_does_not_steal_control() {
    let (a, b, c) = spawn_trio().await;
    c.handle.send(Command::SetMasterEnabled(false));
    wait_until("third machine disabled", || !c.handle.state().borrow().master_enabled).await;
    drive_a_to_b(&a, &b).await;
    c.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(matches!(focus_of(&a), UiFocus::Remote(id) if id == mid("bbb")));
    assert!(matches!(focus_of(&b), UiFocus::Driven(id) if id == mid("aaa")));
}

#[tokio::test]
async fn held_modifiers_follow_the_pointer_across_three_machines() {
    let (a, b, c) = spawn_trio().await;
    drive_a_to_b(&a, &b).await;
    let press = InputEvent::Key { code: 42, pressed: true };
    a.mock.events.send(PlatformEvent::Capture(CaptureEvent::Input(press))).unwrap();
    wait_until("second machine receives Shift", || b.mock.state.lock().injected.contains(&press)).await;
    push_motion(&a, 2200.0, 0.0);
    wait_until("pointer enters third machine with Shift held", || {
        matches!(focus_of(&c), UiFocus::Driven(id) if id == mid("aaa")) && c.mock.state.lock().injected.contains(&press)
    })
    .await;
    let release = InputEvent::Key { code: 42, pressed: false };
    assert!(b.mock.state.lock().injected.contains(&release));
    a.mock.events.send(PlatformEvent::Capture(CaptureEvent::Input(release))).unwrap();
    wait_until("third machine receives Shift release", || c.mock.state.lock().injected.contains(&release)).await;
    push_motion(&a, -2200.0, 0.0);
    wait_until("pointer returns to the middle", || matches!(focus_of(&b), UiFocus::Driven(id) if id == mid("aaa")))
        .await;
    push_motion(&a, -2200.0, 0.0);
    wait_until("pointer returns home", || matches!(focus_of(&a), UiFocus::Local) && !a.mock.state.lock().capturing)
        .await;
}

#[tokio::test]
async fn joining_an_established_workspace_preserves_the_new_machine() {
    use splice_proto::{MachinePlacement, Stamp};
    let a_node = node_at("aaa", Ipv4Addr::new(127, 0, 0, 1));
    let b_node = node_at("bbb", Ipv4Addr::new(127, 0, 0, 2));
    let cfg = splice_core::config::Config {
        layout: Some(LayoutDoc {
            stamp: Stamp { lamport: 100, writer: mid("aaa") },
            machines: [(mid("aaa"), MachinePlacement { offset: Vec2I::default(), enabled: true })].into(),
            sensitivity: Default::default(),
        }),
        ..Default::default()
    };
    let a = spawn_rig_configured(a_node.clone(), vec![b_node.clone()], cfg).await;
    let b = spawn_rig_with(b_node, vec![a_node]).await;
    a.dial_ports.write().unwrap().insert(mid("bbb"), b.addr.port());
    b.dial_ports.write().unwrap().insert(mid("aaa"), a.addr.port());
    wait_until("both machines have a shared crossable edge", || {
        [&a, &b].iter().all(|rig| rig.handle.state().borrow().edges.iter().any(|edge| edge.crossable))
    })
    .await;
    drive_a_to_b(&a, &b).await;
}

#[tokio::test]
async fn newer_two_machine_layout_cannot_erase_connected_middle_machine() {
    use splice_core::net::{NetManager, PeerEvent};
    use splice_proto::{Frame, MachineInfo, MachinePlacement, Os, Stamp};
    let a_node = node_at("aaa", Ipv4Addr::new(127, 0, 0, 1));
    let b_node = node_at("bbb", Ipv4Addr::new(127, 0, 0, 2));
    let b = spawn_rig_with(b_node.clone(), vec![a_node.clone()]).await;
    let opts = test_opts();
    opts.dial_ports.write().unwrap().insert(mid("bbb"), b.addr.port());
    let (mut a, control) = NetManager::spawn_with(
        MachineInfo { build: splice_proto::BuildInfo::current(), id: mid("aaa"), hostname: "aaa".into(), os: Os::Linux, displays: mock::one_display() },
        SocketAddr::new(a_node.ips[0], 0),
        Arc::new(FakeTs {
            self_node: a_node,
            peers: vec![b_node.clone()],
            whois: Arc::new(HashMap::from([(b_node.ips[0], ("bbb".into(), USER))])),
        }),
        opts,
    )
    .await
    .unwrap();
    control.update_dial_targets(vec![(mid("bbb"), b_node.ips[0])]);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(a.events.recv().await, Some(PeerEvent::Connected { .. })) {}
    })
    .await
    .unwrap();
    wait_until("initial workspace includes both machines", || !b.handle.state().borrow().edges.is_empty()).await;
    let offset = b.handle.state().borrow().machines.iter().find(|m| m.id == mid("aaa")).unwrap().offset;
    let incoming = LayoutDoc {
        stamp: Stamp { lamport: 100, writer: mid("aaa") },
        machines: [
            (mid("aaa"), MachinePlacement { offset, enabled: true }),
            (mid("ccc"), MachinePlacement { offset: Vec2I { x: 3840, y: 0 }, enabled: true }),
        ]
        .into(),
        sensitivity: Default::default(),
    };
    assert!(control.send_to(&mid("bbb"), Frame::LayoutSync(incoming)));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(PeerEvent::Frame(_, Frame::LayoutSync(doc))) = a.events.recv().await {
                if doc.stamp.lamport > 100 {
                    assert!(doc.machines.contains_key(&mid("bbb")), "middle machine was removed");
                    assert!(doc.machines.contains_key(&mid("ccc")), "new member was removed");
                    break;
                }
            }
        }
    })
    .await
    .expect("merged workspace must retain and republish the middle machine");
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
    assert!(entered[0].x == 1.0 || entered[0].x == 1919.0, "entered at {entered:?}");

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
    assert!((0.0..1920.0).contains(&warp.x) && (0.0..1080.0).contains(&warp.y));
    wait_until("B released and left", || {
        let st = b.mock.state.lock();
        st.left >= 1 && st.release_all_calls >= 1
    })
    .await;
    wait_until("A back to Local focus", || matches!(focus_of(&a), UiFocus::Local)).await;
}

#[tokio::test]
async fn active_motion_uses_updated_cached_sensitivity() {
    let (a, b) = spawn_pair().await;
    let (edge, _) = drive_a_to_b(&a, &b).await;
    let link_key = LayoutDoc::link_key(&mid("aaa"), &mid("bbb"));
    a.handle.send(Command::SetSensitivity { link_key: link_key.clone(), factor: 1.5 });
    wait_until("sensitivity update is published", || {
        a.handle.state().borrow().sensitivity.get(&link_key) == Some(&1.5)
    })
    .await;
    let into = into_sign(&edge);
    push_motion(&a, 20.0 * into, 4.0);
    let want = InputEvent::Motion { dx: 30.0 * into, dy: 6.0 };
    wait_until("updated sensitivity is applied", || injected(&b).contains(&want)).await;
}

#[tokio::test]
async fn motion_burst_preserves_total_delta() {
    let (a, b) = spawn_pair().await;
    let (edge, _) = drive_a_to_b(&a, &b).await;
    let into = into_sign(&edge);
    for _ in 0..32 {
        push_motion(&a, into, 1.0);
    }
    wait_until("motion burst reaches the target", || {
        let (dx, dy) = injected(&b)
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Motion { dx, dy } => Some((dx, dy)),
                _ => None,
            })
            .fold((0.0, 0.0), |(sum_x, sum_y), (dx, dy)| {
                (sum_x + dx, sum_y + dy)
            });
        dx == 32.0 * into && dy == 32.0
    })
    .await;
}

#[tokio::test]
async fn target_emulation_failure_releases_source_capture() {
    let (a, b) = spawn_pair().await;
    let (edge, _) = drive_a_to_b(&a, &b).await;
    b.mock.state.lock().inject_error = Some("test emulation failure".into());
    push_motion(&a, 25.0 * into_sign(&edge), 0.0);
    wait_until("source capture releases after target emulation failure", || {
        !a.mock.state.lock().capturing
    })
    .await;
    wait_until("both sides return local after target emulation failure", || {
        matches!(focus_of(&a), UiFocus::Local) && matches!(focus_of(&b), UiFocus::Local)
    })
    .await;
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
async fn return_to_source_lands_inside_its_left_edge() {
    let (a, b) = spawn_pair().await;
    wait_until("both machines arm their shared edge", || {
        !a.mock.state.lock().edges.is_empty() && !b.mock.state.lock().edges.is_empty()
    })
    .await;
    let (source, target, source_id) = if a.mock.state.lock().edges[0].side == EdgeSide::Left {
        (&a, &b, mid("aaa"))
    } else {
        (&b, &a, mid("bbb"))
    };
    source.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    wait_until("left machine owns the source claim", || {
        a.handle.state().borrow().source == Some(source_id.clone())
            && b.handle.state().borrow().source == Some(source_id.clone())
    })
    .await;
    let edge = source.mock.state.lock().edges[0].clone();
    assert_eq!(edge.side, EdgeSide::Left);
    let along = f64::from(edge.from + edge.to) / 2.0;
    source
        .mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: edge.id, along }))
        .unwrap();
    wait_until("left source captures", || source.mock.state.lock().capturing).await;
    wait_until("right target accepts source", || {
        !target.mock.state.lock().entered.is_empty()
    })
    .await;
    push_motion(source, 25.0 * into_sign(&edge), 0.0);
    push_motion(source, -4000.0 * into_sign(&edge), 0.0);
    wait_until("left source returns locally", || !source.mock.state.lock().capturing).await;
    let warp = source
        .mock
        .state
        .lock()
        .capture_ends
        .last()
        .copied()
        .flatten()
        .expect("return warp");
    assert!((0.0..1920.0).contains(&warp.x));
    assert!((0.0..1080.0).contains(&warp.y));
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
    let warp = ends.last().copied().flatten().expect("sourceness loss return warp");
    assert!((0.0..1920.0).contains(&warp.x));
    assert!((0.0..1080.0).contains(&warp.y));
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
    let warp = a
        .mock
        .state
        .lock()
        .capture_ends
        .last()
        .copied()
        .flatten()
        .expect("rejected activation warp");
    assert!((0.0..1920.0).contains(&warp.x));
    assert!((0.0..1080.0).contains(&warp.y));
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

#[tokio::test]
async fn target_barrier_activation_after_leave_does_not_steal_sourceness() {
    let (a, b) = spawn_pair().await;
    let (edge, _) = drive_a_to_b(&a, &b).await;
    wait_until("B arms its edge toward A", || !b.mock.state.lock().edges.is_empty()).await;

    push_motion(&a, -4000.0 * into_sign(&edge), 0.0);
    wait_until("A returned home", || {
        !a.mock.state.lock().capturing && matches!(focus_of(&a), UiFocus::Local)
    })
    .await;
    wait_until("B left the driven session", || b.mock.state.lock().left >= 1).await;

    // The compositor reports the barrier hit caused by the last injected motion
    // only after the Leave has already been processed.
    let b_edge = b.mock.state.lock().edges[0].clone();
    let along = f64::from(b_edge.from + b_edge.to) / 2.0;
    let ends_before = b.mock.state.lock().capture_ends.len();
    b.mock.state.lock().capturing = true;
    b.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: b_edge.id, along }))
        .unwrap();
    wait_until("B released the activation", || {
        let st = b.mock.state.lock();
        !st.capturing && st.capture_ends.len() > ends_before
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(a.mock.state.lock().entered.is_empty(), "A must never be driven by B");
    assert!(matches!(focus_of(&a), UiFocus::Local));
    assert!(matches!(focus_of(&b), UiFocus::Local));
    assert_eq!(a.handle.state().borrow().source, Some(mid("aaa")));
    assert_eq!(b.handle.state().borrow().source, Some(mid("aaa")));

    // Real physical input on B lifts the guard immediately.
    b.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    b.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: b_edge.id, along }))
        .unwrap();
    wait_until("B captures after physical input", || b.mock.state.lock().capturing).await;
    wait_until("A is driven by B", || !a.mock.state.lock().entered.is_empty()).await;
}

#[tokio::test]
async fn a_display_geometry_change_keeps_the_arrangement_connected() {
    let (a, b) = spawn_pair().await;
    drive_a_to_b(&a, &b).await;

    b.mock
        .events
        .send(PlatformEvent::DisplaysChanged {
            displays: vec![DisplayRect {
                id: "d0".into(),
                x: 0,
                y: 5000,
                w: 1920,
                h: 1080,
                scale: 1.0,
            }],
        })
        .unwrap();
    wait_until("A sees B's new displays rested against it", || {
        let state = a.handle.state();
        let state = state.borrow();
        state
            .machines
            .iter()
            .find(|m| m.id == mid("bbb"))
            .is_some_and(|m| m.displays.first().is_some_and(|d| d.y == 5000))
            && state.edges.iter().any(|e| e.crossable)
    })
    .await;
    assert!(matches!(focus_of(&a), UiFocus::Remote(id) if id == mid("bbb")));
    assert!(a.mock.state.lock().capturing);
    assert_eq!(b.mock.state.lock().left, 0);
}

#[tokio::test]
async fn target_master_disable_returns_the_source_home() {
    let (a, b) = spawn_pair().await;
    drive_a_to_b(&a, &b).await;

    b.handle.send(Command::SetMasterEnabled(false));
    wait_until("A returns home when the target disables itself", || {
        !a.mock.state.lock().capturing && matches!(focus_of(&a), UiFocus::Local)
    })
    .await;
    let warp = a.mock.state.lock().capture_ends.last().copied().flatten();
    assert!(warp.is_some(), "an orderly teardown warps the cursor home");
    wait_until("B left the driven session", || b.mock.state.lock().left >= 1).await;
}

#[tokio::test]
async fn peer_master_off_makes_the_edge_uncrossable_until_reenabled() {
    let (a, b) = spawn_pair().await;
    wait_until("A arms an edge toward B", || !a.mock.state.lock().edges.is_empty()).await;
    b.handle.send(Command::SetMasterEnabled(false));
    wait_until("A sees B's master off as an uncrossable edge", || {
        let state = a.handle.state();
        let state = state.borrow();
        !state.edges.is_empty() && state.edges.iter().all(|e| !e.crossable)
    })
    .await;

    a.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    let edge = a.mock.state.lock().edges[0].clone();
    let along = f64::from(edge.from + edge.to) / 2.0;
    a.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: edge.id, along }))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!a.mock.state.lock().capturing, "A must not capture toward a master-off peer");
    assert!(b.mock.state.lock().entered.is_empty(), "B must never be entered");
    assert!(matches!(focus_of(&a), UiFocus::Local));

    b.handle.send(Command::SetMasterEnabled(true));
    wait_until("edge becomes crossable again", || {
        a.handle.state().borrow().edges.iter().all(|e| e.crossable)
    })
    .await;
    drive_a_to_b(&a, &b).await;
}

#[tokio::test]
async fn five_machine_workspace_converges_after_middle_machine_restarts() {
    let ids = ["aaa", "bbb", "ccc", "ddd", "eee"];
    let nodes: Vec<_> =
        ids.iter().enumerate().map(|(index, id)| node_at(id, Ipv4Addr::new(127, 0, 0, index as u8 + 1))).collect();
    let mut rigs = Vec::new();
    for node in &nodes {
        rigs.push(
            spawn_rig_with(
                node.clone(),
                nodes.iter().filter(|peer| peer.stable_id != node.stable_id).cloned().collect(),
            )
            .await,
        );
    }
    for rig in &rigs {
        for (id, peer) in ids.iter().zip(&rigs) {
            rig.dial_ports.write().unwrap().insert(mid(id), peer.addr.port());
        }
    }
    wait_until("all twenty directed peer connections are ready", || {
        rigs.iter()
            .enumerate()
            .all(|(index, rig)| ids.iter().enumerate().all(|(other, id)| index == other || connected_to(rig, id)))
    })
    .await;
    let row: Vec<_> =
        ids.iter().enumerate().map(|(index, id)| (mid(id), Vec2I { x: index as i32 * 1920, y: 0 })).collect();
    rigs[4].handle.send(Command::SetArrangement(row.clone()));
    wait_until("every machine adopts the fifth machine's arrangement", || {
        rigs.iter().all(|rig| {
            let state = rig.handle.state();
            let state = state.borrow();
            state.edges.len() == 4
                && state.edges.iter().all(|edge| edge.crossable)
                && row
                    .iter()
                    .all(|(id, pos)| state.machines.iter().any(|machine| &machine.id == id && &machine.offset == pos))
        })
    })
    .await;
    let middle = rigs.remove(2);
    let dir = middle.data_dir.clone();
    drop(middle);
    wait_until("survivors observe middle machine shutdown", || rigs.iter().all(|rig| !connected_to(rig, "ccc"))).await;
    let config = splice_core::config::load(&dir).unwrap();
    assert_eq!(config.layout.as_ref().unwrap().machines.len(), 5);
    let middle = spawn_rig_configured(
        nodes[2].clone(),
        nodes.iter().filter(|node| node.stable_id != "ccc").cloned().collect(),
        config,
    )
    .await;
    rigs.insert(2, middle);
    for rig in &rigs {
        for (id, peer) in ids.iter().zip(&rigs) {
            rig.dial_ports.write().unwrap().insert(mid(id), peer.addr.port());
        }
    }
    wait_until("all twenty connections and the arrangement recover", || {
        rigs.iter().enumerate().all(|(index, rig)| {
            let state = rig.handle.state();
            let state = state.borrow();
            state.edges.len() == 4
                && state.edges.iter().all(|edge| edge.crossable)
                && ids.iter().enumerate().all(|(other, id)| index == other || connected_to(rig, id))
                && row
                    .iter()
                    .all(|(id, pos)| state.machines.iter().any(|machine| &machine.id == id && &machine.offset == pos))
        })
    })
    .await;
}

#[tokio::test]
async fn repeated_clipboard_reads_are_independent_and_disabled_callbacks_expire() {
    let (a, b) = spawn_pair().await;
    let mime = "image/png";
    let bytes = vec![37; splice_proto::CLIP_CHUNK * 3 + 7];
    a.mock.state.lock().local_clip.insert(mime.into(), bytes.clone());
    a.mock.events.send(PlatformEvent::ClipboardChanged { mimes: vec![mime.into()], inline_text: None }).unwrap();
    wait_until("clipboard offer arrives", || b.mock.last_fetch.lock().is_some()).await;
    let fetch = b.mock.last_fetch.lock().clone().unwrap();
    let (first, second) = tokio::join!(fetch.fetch(mime), fetch.fetch(mime));
    assert_eq!(first, Some(bytes.clone()));
    assert_eq!(second, Some(bytes));
    b.handle.send(Command::SetClipboardSync(false));
    wait_until("clipboard sharing is disabled", || !b.handle.state().borrow().clipboard_sync).await;
    assert_eq!(fetch.fetch(mime).await, None);
    b.handle.send(Command::SetClipboardSync(true));
    wait_until("clipboard sharing is enabled", || b.handle.state().borrow().clipboard_sync).await;
    assert_eq!(fetch.fetch(mime).await, None);
}

#[tokio::test]
async fn failed_config_writes_are_visible_and_retry_after_storage_recovers() {
    let rig = spawn_rig_with(node("aaa"), Vec::new()).await;
    let blocked = rig.data_dir.join("config.json.tmp");
    std::fs::create_dir(&blocked).unwrap();
    rig.handle.send(Command::SetMasterEnabled(false));
    wait_until("failed save appears in UI", || rig.handle.state().borrow().config_error.is_some()).await;
    std::fs::remove_dir(blocked).unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while rig.handle.state().borrow().config_error.is_some() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("save retries and clears the error");
    assert!(!splice_core::config::load(&rig.data_dir).unwrap().master_enabled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_latency_stays_bounded_during_concurrent_large_clipboard_transfers() {
    let (a, b) = spawn_pair().await;
    let (edge, _) = drive_a_to_b(&a, &b).await;
    let payload = vec![0x5a; 8 * 1024 * 1024];
    a.mock.state.lock().local_clip.insert(TEXT_MIME.into(), payload.clone());
    a.mock.events.send(PlatformEvent::ClipboardChanged { mimes: vec![TEXT_MIME.into()], inline_text: None }).unwrap();
    wait_until("large clipboard offer arrives", || b.mock.last_fetch.lock().is_some()).await;
    let fetch = b.mock.last_fetch.lock().clone().unwrap();
    let mut transfers = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let fetch = fetch.clone();
        transfers.spawn(async move { fetch.fetch(TEXT_MIME).await });
    }
    tokio::task::yield_now().await;
    let mut delays = Vec::new();
    for index in 0..64 {
        let event = InputEvent::Motion { dx: into_sign(&edge) * 0.125, dy: (index + 1) as f64 / 1024.0 };
        let started = Instant::now();
        a.mock.events.send(PlatformEvent::Capture(CaptureEvent::Input(event))).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !b.mock.state.lock().injected.contains(&event) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.unwrap_or_else(|_| panic!("input stalled during clipboard transfer: source={:?}, target={:?}", a.handle.state().borrow().diagnostics.peers, b.handle.state().borrow().diagnostics.peers));
        delays.push(started.elapsed());
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    while let Some(result) = transfers.join_next().await {
        assert_eq!(result.unwrap().unwrap(), payload);
    }
    delays.sort();
    let p95 = delays[delays.len() * 95 / 100];
    let max = *delays.last().unwrap();
    eprintln!("motion with four 8 MiB clipboard transfers: p95={p95:?}, max={max:?}");
    assert!(p95 < Duration::from_millis(50), "loopback p95 input delay regressed: {p95:?}");
    assert!(max < Duration::from_millis(250), "loopback worst input delay regressed: {max:?}");
    assert!(connected_to(&a, "bbb") && connected_to(&b, "aaa"));
}

#[tokio::test]
async fn diagnostics_report_build_heartbeat_and_traffic_without_clipboard_contents() {
    use std::os::unix::fs::PermissionsExt;
    let (a, b) = spawn_pair().await;
    let (edge, _) = drive_a_to_b(&a, &b).await;
    let secret = "PRIVATE_CLIPBOARD_CONTENT_9c2f51";
    a.mock.events.send(PlatformEvent::ClipboardChanged { mimes: vec![TEXT_MIME.into()], inline_text: Some(secret.into()) }).unwrap();
    push_motion(&a, into_sign(&edge), 0.0);
    push_key(&a, 30, true);
    push_key(&a, 30, false);
    wait_until("diagnostics include measured traffic and heartbeat", || {
        let state = a.handle.state();
        let state = state.borrow();
        state.diagnostics.peers.get(&mid("bbb")).is_some_and(|p| p.last_heartbeat_ms.is_some() && p.traffic.input_frames_sent >= 3)
    }).await;
    a.handle.send(Command::ExportDiagnostics);
    wait_until("diagnostic export completes", || a.handle.state().borrow().diagnostics.export_path.is_some()).await;
    let path = a.handle.state().borrow().diagnostics.export_path.clone().unwrap();
    let bytes = std::fs::read_to_string(&path).unwrap();
    assert!(!bytes.contains(secret));
    assert!(!bytes.contains("held_keys") && !bytes.contains("tokens"));
    let value: serde_json::Value = serde_json::from_str(&bytes).unwrap();
    assert_eq!(value["state"]["diagnostics"]["peers"]["bbb"]["build"]["protocol"], splice_proto::PROTO_VERSION);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}
