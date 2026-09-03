//! Diagnostic: drive THIS machine's real Linux platform from an in-process mock peer.
//! Engine "aaa" (mock platform, the remote source) enters engine "bbb" (the real
//! platform: uinput injection, evdev activity monitor) over loopback, types, clicks and
//! moves, and reports whether bbb stays driven. Stop the Splice service first.
//!
//!   cargo run -p splice-core --example real_target

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use splice_core::engine::{Command, Engine, EngineHandle};
    use splice_core::net::{NetOpts, TsApi};
    use splice_core::ui_state::{UiConnection, UiFocus};
    use splice_platform::mock::{self, MockHandle};
    use splice_platform::{
        BackendPrefs, CaptureEvent, InjectPref, PlatformEvent, PlatformOpts,
    };
    use splice_proto::{InputEvent, MachineId, PointerButton, Vec2I};
    use splice_tailscale::{Node, Status, TsError, WhoIs, WhoIsUser};
    use std::collections::HashMap;
    use std::future::Future;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    tracing_subscriber::fmt()
        .with_env_filter("info,splice_core=debug,splice_platform=debug")
        .init();

    const USER: u64 = 7;

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
        fn whois(&self, addr: SocketAddr) -> Pin<Box<dyn Future<Output = Result<WhoIs, TsError>> + Send + '_>> {
            let found = self.whois.get(&addr.ip()).cloned();
            Box::pin(async move {
                match found {
                    Some((id, user)) => Ok(WhoIs { node_stable_id: id, user: WhoIsUser { id: user, login_name: "tester".into() } }),
                    None => Err(TsError::PeerNotFound(addr)),
                }
            })
        }
    }
    fn node(id: &str, ip: Ipv4Addr) -> Node {
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
    fn ts(self_node: Node, peers: Vec<Node>) -> Arc<FakeTs> {
        let whois = peers.iter().filter_map(|p| p.ips.first().map(|ip| (*ip, (p.stable_id.clone(), USER)))).collect();
        Arc::new(FakeTs { self_node, peers, whois: Arc::new(whois) })
    }
    fn opts() -> NetOpts {
        NetOpts {
            backoff_min: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
            dial_timeout: Duration::from_millis(500),
            handshake_timeout: Duration::from_secs(2),
            ..Default::default()
        }
    }
    async fn wait_until(what: &str, mut f: impl FnMut() -> bool) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            if Instant::now() > deadline {
                anyhow::bail!("timed out waiting for {what}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
    fn connected(h: &EngineHandle, peer: &str) -> bool {
        h.state().borrow().machines.iter().any(|m| m.id.0 == peer && matches!(m.connection, UiConnection::Direct { .. } | UiConnection::Derp { .. }))
    }
    fn focus(h: &EngineHandle) -> UiFocus {
        h.state().borrow().focus.clone()
    }

    let na = node("aaa", Ipv4Addr::new(127, 0, 0, 1));
    let nb = node("bbb", Ipv4Addr::new(127, 0, 0, 2));
    let tmp = std::env::temp_dir().join(format!("splice-real-target-{}", std::process::id()));
    std::fs::create_dir_all(tmp.join("a"))?;
    std::fs::create_dir_all(tmp.join("b"))?;

    let (mock_platform, mock): (_, MockHandle) = mock::create(mock::one_display());
    let a_opts = opts();
    let a_ports = a_opts.dial_ports.clone();
    let a = Engine::spawn_with(mock_platform, ts(na.clone(), vec![nb.clone()]), tmp.join("a"), a_opts, Duration::from_millis(50)).await?;

    let data_dir = splice_core::config::config_dir()?;
    let real = splice_platform::create(PlatformOpts {
        data_dir,
        panic_chord: Vec::new(),
        backends: BackendPrefs { inject: InjectPref::Uinput, ..Default::default() },
    })
    .await?;
    let b_opts = opts();
    let b_ports = b_opts.dial_ports.clone();
    let b = Engine::spawn_with(real, ts(nb.clone(), vec![na.clone()]), tmp.join("b"), b_opts, Duration::from_millis(50)).await?;

    let a_addr = a.bound_addr().await.ok_or_else(|| anyhow::anyhow!("a bind"))?;
    let b_addr = b.bound_addr().await.ok_or_else(|| anyhow::anyhow!("b bind"))?;
    a_ports.write().unwrap().insert(MachineId("bbb".into()), b_addr.port());
    b_ports.write().unwrap().insert(MachineId("aaa".into()), a_addr.port());
    wait_until("peers connect", || connected(&a, "bbb") && connected(&b, "aaa")).await?;
    a.send(Command::SetArrangement(vec![
        (MachineId("aaa".into()), Vec2I { x: -1920, y: 0 }),
        (MachineId("bbb".into()), Vec2I { x: 0, y: 0 }),
    ]));
    wait_until("A arms an edge toward B", || !mock.state.lock().edges.is_empty()).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    mock.events.send(PlatformEvent::PhysicalActivity)?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let edge = mock.state.lock().edges[0].clone();
    let along = f64::from(edge.from + edge.to) / 2.0;
    mock.events.send(PlatformEvent::Capture(CaptureEvent::EdgeHit { edge_id: edge.id, along }))?;
    wait_until("B is driven", || matches!(focus(&b), UiFocus::Driven(_))).await?;
    println!("B driven; typing, clicking and moving through the real backend");

    let push = |ev: InputEvent| mock.events.send(PlatformEvent::Capture(CaptureEvent::Input(ev))).map_err(|e| anyhow::anyhow!("{e}"));
    let mut verdict = Ok(());
    'outer: for round in 0..4 {
        for (code, hold) in [(30u32, 90u64), (31, 400), (42, 1500), (125, 700), (57, 60)] {
            push(InputEvent::Key { code, pressed: true })?;
            tokio::time::sleep(Duration::from_millis(hold)).await;
            push(InputEvent::Key { code, pressed: false })?;
            tokio::time::sleep(Duration::from_millis(150)).await;
            if !matches!(focus(&b), UiFocus::Driven(_)) {
                verdict = Err(anyhow::anyhow!("B stopped being driven after key {code} in round {round}"));
                break 'outer;
            }
        }
        for _ in 0..20 {
            push(InputEvent::Motion { dx: 3.0, dy: 1.0 })?;
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
        push(InputEvent::Button { button: PointerButton::Middle, pressed: true })?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        push(InputEvent::Button { button: PointerButton::Middle, pressed: false })?;
        push(InputEvent::Scroll120 { dx: 0, dy: 120 })?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        if !matches!(focus(&b), UiFocus::Driven(_)) {
            verdict = Err(anyhow::anyhow!("B stopped being driven after mouse input in round {round}"));
            break;
        }
    }
    a.send(Command::Panic);
    tokio::time::sleep(Duration::from_millis(300)).await;
    match &verdict {
        Ok(()) => println!("OK: B stayed driven through 4 rounds of keys, motion, clicks and scroll"),
        Err(err) => println!("FAIL: {err}"),
    }
    verdict
}

#[cfg(not(target_os = "linux"))]
fn main() {}
