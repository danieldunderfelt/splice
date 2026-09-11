use splice_core::{
    files::*,
    net::{NetOpts, TsApi},
    Command, Engine, EngineHandle, UiConnection, UiFocus,
};
use splice_platform::{
    mock::{self, MockHandle},
    CaptureEvent, PlatformEvent,
};
use splice_proto::{InputEvent, MachineId};
use splice_tailscale::{Node, Status, TsError, WhoIs, WhoIsUser};
use std::{
    collections::HashMap,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

static FILE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct FakeTs {
    own: Node,
    peers: Vec<Node>,
}
impl TsApi for FakeTs {
    fn status(&self) -> Pin<Box<dyn Future<Output = Result<Status, TsError>> + Send + '_>> {
        Box::pin(async {
            Ok(Status {
                self_node: self.own.clone(),
                peers: self.peers.clone(),
            })
        })
    }
    fn whois(
        &self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Result<WhoIs, TsError>> + Send + '_>> {
        Box::pin(async move {
            if let Some(peer) = self.peers.iter().find(|p| p.ips.contains(&address.ip())) {
                Ok(WhoIs {
                    node_stable_id: peer.stable_id.clone(),
                    user: WhoIsUser {
                        id: 7,
                        login_name: "test".into(),
                    },
                })
            } else {
                Err(TsError::PeerNotFound(address))
            }
        })
    }
}

fn node(id: &str, octet: u8) -> Node {
    Node {
        stable_id: id.into(),
        hostname: id.into(),
        user_id: 7,
        os: "linux".into(),
        ips: vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, octet))],
        online: true,
        ..Default::default()
    }
}

async fn until(label: &str, mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {label}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct Rig {
    engine: EngineHandle,
    mock: MockHandle,
    _dir: tempfile::TempDir,
}
async fn pair() -> (Rig, Rig) {
    pair_with_corrupt_receipt(false).await
}

async fn pair_with_corrupt_receipt(corrupt: bool) -> (Rig, Rig) {
    let na = node("files-a", 4);
    let nb = node("files-b", 5);
    let ports = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let mut rigs = vec![];
    for (own, other) in [(na.clone(), nb.clone()), (nb, na)] {
        let (platform, mock) = mock::create(mock::one_display());
        let dir = tempfile::tempdir().unwrap();
        if corrupt && own.stable_id == "files-b" {
            let records = dir.path().join("files/records");
            std::fs::create_dir_all(&records).unwrap();
            std::fs::write(
                records.join("01010101010101010101010101010101.json"),
                b"invalid journal",
            )
            .unwrap();
        }
        let options = NetOpts {
            dial_ports: ports.clone(),
            backoff_min: Duration::from_millis(25),
            backoff_max: Duration::from_millis(50),
            ..Default::default()
        };
        let id = MachineId(own.stable_id.clone());
        let engine = Engine::spawn_with(
            platform,
            Arc::new(FakeTs {
                own,
                peers: vec![other],
            }),
            dir.path().to_path_buf(),
            options,
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let address = engine.bound_addr().await.unwrap();
        ports.write().unwrap().insert(id, address.port());
        rigs.push(Rig {
            engine,
            mock,
            _dir: dir,
        });
    }
    let b = rigs.pop().unwrap();
    let a = rigs.pop().unwrap();
    until("file engines connected", || {
        [&a, &b].iter().all(|r| {
            let state = r.engine.state();
            let state = state.borrow();
            state.machines.iter().any(|m| {
                matches!(
                    m.connection,
                    UiConnection::Direct { .. } | UiConnection::Derp { .. }
                )
            }) && state.files.enabled
        })
    })
    .await;
    (a, b)
}

async fn offer(a: &Rig, b: &Rig, paths: Vec<PathBuf>) -> FileOfferId {
    let reply = a
        .engine
        .files()
        .request(FileCommand::Offer {
            paths,
            recipient: MachineId("files-b".into()),
            origin: OfferOrigin::Selection,
        })
        .await
        .unwrap();
    let FileReply::Offered(id) = reply else {
        panic!("wrong reply");
    };
    until("offer metadata delivered", || {
        b.engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .any(|o| o.id == id)
    })
    .await;
    id
}

async fn receive(rig: &Rig, offer: FileOfferId, destination: ReceiveDestination) -> TransferId {
    let reply = rig
        .engine
        .files()
        .request(FileCommand::Receive { offer, destination })
        .await
        .unwrap();
    let FileReply::Receiving(id) = reply else {
        panic!("wrong receive reply");
    };
    id
}

async fn wait(rig: &Rig, id: TransferId) -> anyhow::Result<ReceivedFiles> {
    tokio::time::timeout(Duration::from_secs(120), rig.engine.files().wait(id))
        .await
        .unwrap()
}

fn digest(path: &Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut source = std::fs::File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = source.read(&mut buffer).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    hash.finalize().to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_file_service_moves_verified_roots_and_keeps_input_responsive() {
    let _serial = FILE_TEST_LOCK.lock().await;
    let (a, b) = pair().await;
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let tree = source.path().join("folder café %\n");
    std::fs::create_dir_all(tree.join("empty")).unwrap();
    std::fs::create_dir(tree.join("nested")).unwrap();
    std::fs::write(tree.join("nested/value"), b"verified bytes").unwrap();
    std::fs::write(tree.join("zero"), []).unwrap();
    std::os::unix::fs::symlink("nested/value", tree.join("alias")).unwrap();
    let id = offer(&a, &b, vec![tree.clone()]).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, 0);
    assert_eq!(b.engine.files().state().borrow().payload_bytes_received, 0);
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.5:0".parse().unwrap()).unwrap();
        let mut connection = socket
            .connect("127.0.0.4:41720".parse().unwrap())
            .await
            .unwrap();
        connection
            .write_all(splice_proto::files::BULK_MAGIC)
            .await
            .unwrap();
        connection.write_all(&[9; 16]).await.unwrap();
        connection.write_all(&[0; 32]).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            connection.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.is_empty());
        assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, 0);
    }

    let FileReply::DragPrepared(drag) = b
        .engine
        .files()
        .request(FileCommand::PrepareDrag {
            offer: id,
            entries: vec![],
        })
        .await
        .unwrap()
    else {
        panic!("drag reply");
    };
    assert!(b
        .engine
        .files()
        .request(FileCommand::CommitDrag {
            drag,
            destination: ReceiveDestination::Cache
        })
        .await
        .is_err());
    b.engine
        .files()
        .request(FileCommand::CancelDrag { drag })
        .await
        .unwrap();
    assert!(b
        .engine
        .files()
        .request(FileCommand::DropDrag { drag })
        .await
        .is_err());
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, 0);
    let transfer = receive(
        &b,
        id,
        ReceiveDestination::Directory(destination.path().into()),
    )
    .await;
    let received = wait(&b, transfer).await.unwrap();
    assert_eq!(received.paths.len(), 1);
    assert_eq!(
        std::fs::read(received.paths[0].join("nested/value")).unwrap(),
        b"verified bytes"
    );
    assert_eq!(
        std::fs::read(received.paths[0].join("alias")).unwrap(),
        b"verified bytes"
    );
    assert!(std::fs::symlink_metadata(received.paths[0].join("alias"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_link(received.paths[0].join("alias")).unwrap(),
        PathBuf::from("nested/value")
    );
    assert!(received.paths[0].join("empty").is_dir());
    assert_eq!(
        std::fs::metadata(received.paths[0].join("zero"))
            .unwrap()
            .len(),
        0
    );
    let duplicate = receive(
        &b,
        id,
        ReceiveDestination::Directory(destination.path().into()),
    )
    .await;
    assert!(wait(&b, duplicate).await.is_err());
    assert_eq!(
        std::fs::read(received.paths[0].join("nested/value")).unwrap(),
        b"verified bytes"
    );
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
    a.engine
        .files()
        .request(FileCommand::Revoke { offer: id })
        .await
        .unwrap();

    let small = source.path().join("native-file");
    std::fs::write(&small, b"scoped native receive").unwrap();
    let ignored = source.path().join("unrequested");
    std::fs::write(&ignored, b"must not be served").unwrap();
    let id = offer(&a, &b, vec![small.clone(), ignored]).await;
    let root = b
        .engine
        .files()
        .state()
        .borrow()
        .offers
        .iter()
        .find(|o| o.id == id)
        .unwrap()
        .manifest
        .entries
        .iter()
        .find(|e| e.name == "native-file")
        .unwrap()
        .id;
    let FileReply::DragPrepared(drag) = b
        .engine
        .files()
        .request(FileCommand::PrepareDrag {
            offer: id,
            entries: vec![root],
        })
        .await
        .unwrap()
    else {
        panic!();
    };
    b.engine
        .files()
        .request(FileCommand::DropDrag { drag })
        .await
        .unwrap();
    let before = a.engine.files().state().borrow().payload_bytes_sent;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, before);
    let command = FileCommand::CommitDrag {
        drag,
        destination: ReceiveDestination::Cache,
    };
    let first = b.engine.files().request(command.clone()).await.unwrap();
    assert_eq!(b.engine.files().request(command).await.unwrap(), first);
    let FileReply::Receiving(transfer) = first else {
        panic!();
    };
    let ready = wait(&b, transfer).await.unwrap();
    assert_eq!(ready.paths.len(), 1);
    assert_eq!(
        std::fs::read(&ready.paths[0]).unwrap(),
        b"scoped native receive"
    );
    assert!(ready.paths[0].exists());
    a.engine
        .files()
        .request(FileCommand::Revoke { offer: id })
        .await
        .unwrap();

    let retry_source = source.path().join("retry-source");
    std::fs::write(&retry_source, b"retry with fresh grant").unwrap();
    let retry_offer = offer(&a, &b, vec![retry_source]).await;
    let retry_destination = destination.path().join("created-after-failure");
    let failed = receive(
        &b,
        retry_offer,
        ReceiveDestination::Directory(retry_destination.clone()),
    )
    .await;
    assert!(wait(&b, failed).await.is_err());
    until("failed receive invalidates source grant", || {
        a.engine
            .files()
            .state()
            .borrow()
            .transfers
            .iter()
            .any(|t| t.id == failed && t.state.terminal())
    })
    .await;
    std::fs::create_dir(&retry_destination).unwrap();
    let FileReply::Receiving(retried) = b
        .engine
        .files()
        .request(FileCommand::Retry { transfer: failed })
        .await
        .unwrap()
    else {
        panic!();
    };
    assert_ne!(failed, retried);
    let result = wait(&b, retried).await.unwrap();
    assert_eq!(
        std::fs::read(&result.paths[0]).unwrap(),
        b"retry with fresh grant"
    );
    a.engine
        .files()
        .request(FileCommand::Revoke { offer: retry_offer })
        .await
        .unwrap();

    let changed = source.path().join("changed");
    std::fs::write(&changed, b"original").unwrap();
    let id = offer(&a, &b, vec![changed.clone()]).await;
    std::fs::write(&changed, b"mutation").unwrap();
    let transfer = receive(&b, id, ReceiveDestination::Cache).await;
    assert!(wait(&b, transfer).await.is_err());
    assert!(b
        .engine
        .files()
        .state()
        .borrow()
        .transfers
        .iter()
        .find(|t| t.id == transfer)
        .unwrap()
        .paths
        .is_empty());
    a.engine
        .files()
        .request(FileCommand::Revoke { offer: id })
        .await
        .unwrap();

    let big = source.path().join("large-stream");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let id = offer(&a, &b, vec![big.clone()]).await;
    a.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    until("source edge armed", || {
        !a.mock.state.lock().edges.is_empty()
    })
    .await;
    let edge = a.mock.state.lock().edges[0].clone();
    a.mock
        .events
        .send(PlatformEvent::Capture(CaptureEvent::EdgeHit {
            edge_id: edge.id,
            along: f64::from(edge.from + edge.to) / 2.0,
        }))
        .unwrap();
    until("desktop input active", || {
        a.engine.state().borrow().focus == UiFocus::Remote(MachineId("files-b".into()))
    })
    .await;
    let transfer = receive(&b, id, ReceiveDestination::Cache).await;
    let started = Instant::now();
    for index in 0..100 {
        let count = b.mock.state.lock().injected.len();
        a.mock
            .events
            .send(PlatformEvent::Capture(CaptureEvent::Input(
                InputEvent::Key {
                    code: 30,
                    pressed: index % 2 == 0,
                },
            )))
            .unwrap();
        let latency = Instant::now();
        until("input continues during bulk transfer", || {
            b.mock.state.lock().injected.len() > count
        })
        .await;
        assert!(latency.elapsed() < Duration::from_millis(750));
    }
    let ready = wait(&b, transfer).await.unwrap();
    assert_eq!(
        std::fs::metadata(&ready.paths[0]).unwrap().len(),
        128 * 1024 * 1024
    );
    let original = big.clone();
    let copied = ready.paths[0].clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || digest(&original))
            .await
            .unwrap(),
        tokio::task::spawn_blocking(move || digest(&copied))
            .await
            .unwrap()
    );
    eprintln!(
        "128 MiB streamed and verified in {:?}, 100 UDP key transitions delivered",
        started.elapsed()
    );
    a.engine
        .files()
        .request(FileCommand::Revoke { offer: id })
        .await
        .unwrap();

    let huge = source.path().join("cancel-large");
    std::fs::File::create(&huge)
        .unwrap()
        .set_len(5 * 1024 * 1024 * 1024)
        .unwrap();
    let id = offer(&a, &b, vec![huge.clone()]).await;
    let transfer = receive(&b, id, ReceiveDestination::Cache).await;
    until("large transfer starts", || {
        b.engine
            .files()
            .state()
            .borrow()
            .transfers
            .iter()
            .any(|t| t.id == transfer && t.bytes > 0)
    })
    .await;
    b.engine
        .files()
        .request(FileCommand::Cancel { transfer })
        .await
        .unwrap();
    assert!(wait(&b, transfer).await.is_err());
    until("source cancellation propagates", || {
        a.engine
            .files()
            .state()
            .borrow()
            .transfers
            .iter()
            .any(|t| t.id == transfer && t.state.terminal())
    })
    .await;
    assert!(b
        .engine
        .files()
        .state()
        .borrow()
        .transfers
        .iter()
        .find(|t| t.id == transfer)
        .unwrap()
        .paths
        .is_empty());
    assert_eq!(
        std::fs::metadata(&huge).unwrap().len(),
        5 * 1024 * 1024 * 1024
    );
    let revoked = receive(&b, id, ReceiveDestination::Cache).await;
    until("policy test transfer starts", || {
        b.engine
            .files()
            .state()
            .borrow()
            .transfers
            .iter()
            .any(|t| t.id == revoked && t.bytes > 0)
    })
    .await;
    a.engine
        .files()
        .request(FileCommand::SetEnabled(false))
        .await
        .unwrap();
    assert!(wait(&b, revoked).await.is_err());
    until("policy stops source bytes", || {
        a.engine
            .files()
            .state()
            .borrow()
            .transfers
            .iter()
            .any(|t| t.id == revoked && t.state.terminal())
    })
    .await;
    let sent = a.engine.files().state().borrow().payload_bytes_sent;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, sent);
    a.engine
        .files()
        .request(FileCommand::SetEnabled(true))
        .await
        .unwrap();
    a.engine
        .files()
        .request(FileCommand::Revoke { offer: id })
        .await
        .unwrap();

    let before = a.engine.files().state().borrow().payload_bytes_sent;
    a.engine.send(Command::FileClipboardSelection {
        paths: vec![small],
        generation: 101,
    });
    until("clipboard selection routes to controlled peer", || {
        b.engine.files().state().borrow().offers.iter().any(|o| {
            o.origin == (OfferOrigin::Clipboard { generation: 101 })
                && o.state == OfferState::Available
        })
    })
    .await;
    let clipboard = b
        .engine
        .files()
        .state()
        .borrow()
        .offers
        .iter()
        .find(|o| o.origin == (OfferOrigin::Clipboard { generation: 101 }))
        .unwrap()
        .id;
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, before);
    let FileReply::DragPrepared(drag) = b
        .engine
        .files()
        .request(FileCommand::PrepareDrag {
            offer: clipboard,
            entries: vec![],
        })
        .await
        .unwrap()
    else {
        panic!();
    };
    b.engine
        .files()
        .request(FileCommand::DropDrag { drag })
        .await
        .unwrap();
    a.mock
        .events
        .send(PlatformEvent::ClipboardChanged {
            mimes: vec!["text/plain".into()],
            inline_text: Some("new local copy".into()),
        })
        .unwrap();
    until("replaced clipboard offer revoked", || {
        b.engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .any(|o| o.id == clipboard && o.state == OfferState::Revoked)
    })
    .await;
    assert!(b
        .engine
        .files()
        .request(FileCommand::Receive {
            offer: clipboard,
            destination: ReceiveDestination::Cache
        })
        .await
        .is_err());
    let FileReply::Receiving(transfer) = b
        .engine
        .files()
        .request(FileCommand::CommitDrag {
            drag,
            destination: ReceiveDestination::Cache,
        })
        .await
        .unwrap()
    else {
        panic!();
    };
    let clipboard_files = wait(&b, transfer).await.unwrap();
    assert_eq!(
        std::fs::read(&clipboard_files.paths[0]).unwrap(),
        b"scoped native receive"
    );
    until("clipboard owner cleared in UI", || {
        b.engine.state().borrow().file_clipboard.is_none()
    })
    .await;

    b.engine
        .files()
        .request(FileCommand::SetEnabled(false))
        .await
        .unwrap();
    let retained = ready.paths[0].clone();
    let Rig {
        engine,
        mock,
        _dir: cache,
    } = b;
    drop(a);
    drop(engine);
    drop(mock);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(retained.is_file());
    let (platform, _) = mock::create(mock::one_display());
    let restored = Engine::spawn_with(
        platform,
        Arc::new(FakeTs {
            own: node("files-b", 5),
            peers: vec![node("files-a", 4)],
        }),
        cache.path().into(),
        NetOpts::default(),
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    restored.bound_addr().await.unwrap();
    until("durable files recovered", || {
        restored
            .files()
            .state()
            .borrow()
            .transfers
            .iter()
            .any(|t| t.paths.contains(&retained) && t.state == TransferState::Ready)
    })
    .await;
    assert!(!restored.files().state().borrow().enabled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_owner_routes_directly_to_third_machine_without_sending_names_to_controller() {
    let _serial = FILE_TEST_LOCK.lock().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut nodes = vec![node("files-a", 3), node("files-b", 4), node("files-c", 5)];
    nodes[0].os = "macos".into();
    let ports = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let mut rigs = Vec::new();
    for own in &nodes {
        let (platform, mock) = mock::create(mock::one_display());
        let dir = tempfile::tempdir().unwrap();
        let peers = nodes
            .iter()
            .filter(|p| p.stable_id != own.stable_id)
            .cloned()
            .collect();
        let options = NetOpts {
            dial_ports: ports.clone(),
            backoff_min: Duration::from_millis(25),
            backoff_max: Duration::from_millis(50),
            ..Default::default()
        };
        let engine = Engine::spawn_with(
            platform,
            Arc::new(FakeTs {
                own: own.clone(),
                peers,
            }),
            dir.path().into(),
            options,
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let address = engine.bound_addr().await.unwrap();
        ports
            .write()
            .unwrap()
            .insert(MachineId(own.stable_id.clone()), address.port());
        rigs.push(Rig {
            engine,
            mock,
            _dir: dir,
        });
    }
    until("three file engines connected", || {
        rigs.iter().all(|r| {
            r.engine
                .state()
                .borrow()
                .machines
                .iter()
                .filter(|m| {
                    matches!(
                        m.connection,
                        UiConnection::Direct { .. } | UiConnection::Derp { .. }
                    )
                })
                .count()
                == 2
        })
    })
    .await;
    let [controller, owner, recipient] = rigs.as_slice() else {
        panic!();
    };
    let selection = tempfile::tempdir().unwrap();
    let path = selection.path().join("private-selection-name");
    std::fs::write(&path, b"owner to recipient").unwrap();
    controller
        .mock
        .events
        .send(PlatformEvent::PhysicalActivity)
        .unwrap();
    controller
        .engine
        .send(Command::SelectTarget(MachineId("files-c".into())));
    until("controller focused on third machine", || {
        matches!(controller.engine.state().borrow().focus, UiFocus::Remote(_))
    })
    .await;
    let native_access = Arc::new(());
    let native_weak = Arc::downgrade(&native_access);
    owner
        .engine
        .capture_file_clipboard(
            LocalSelection {
                roots: vec![SelectedRoot::Open {
                    name: "private-selection-name".into(),
                    file: Arc::new(std::fs::File::open(path).unwrap()),
                }],
                access: Some(native_access),
            },
            55,
        )
        .unwrap();
    until("controller learns opaque clipboard owner", || {
        controller
            .engine
            .state()
            .borrow()
            .file_clipboard
            .as_ref()
            .is_some_and(|r| r.stamp.writer == MachineId("files-b".into()))
    })
    .await;
    assert!(controller.engine.files().state().borrow().offers.is_empty());
    until("third machine gets targeted offer", || {
        recipient
            .engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .any(|o| o.owner == MachineId("files-b".into()))
    })
    .await;
    assert!(controller.engine.files().state().borrow().offers.is_empty());
    assert_eq!(owner.engine.files().state().borrow().payload_bytes_sent, 0);
    assert_eq!(
        recipient
            .engine
            .files()
            .state()
            .borrow()
            .payload_bytes_received,
        0
    );
    let offer = recipient.engine.files().state().borrow().offers[0].id;
    let id = receive(recipient, offer, ReceiveDestination::Cache).await;
    let received = wait(recipient, id).await.unwrap();
    assert_eq!(
        std::fs::read(&received.paths[0]).unwrap(),
        b"owner to recipient"
    );
    assert_eq!(
        controller
            .engine
            .files()
            .state()
            .borrow()
            .payload_bytes_sent,
        0
    );
    assert_eq!(
        controller
            .engine
            .files()
            .state()
            .borrow()
            .payload_bytes_received,
        0
    );
    let current = recipient
        .engine
        .files()
        .state()
        .borrow()
        .transfers
        .iter()
        .find(|t| t.id == id)
        .unwrap()
        .clone();
    assert_eq!(current.peer, MachineId("files-b".into()));
    controller
        .engine
        .send(Command::SelectTarget(MachineId("files-a".into())));
    until("remote owner routes home", || {
        controller
            .engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .any(|o| {
                o.owner == MachineId("files-b".into()) && o.recipient == MachineId("files-a".into())
            })
    })
    .await;
    assert_eq!(
        controller
            .engine
            .files()
            .state()
            .borrow()
            .payload_bytes_received,
        0
    );
    let home_offer = controller
        .engine
        .files()
        .state()
        .borrow()
        .offers
        .iter()
        .find(|o| o.owner == MachineId("files-b".into()))
        .unwrap()
        .id;
    let home_transfer = receive(controller, home_offer, ReceiveDestination::Cache).await;
    assert_eq!(
        std::fs::read(&wait(controller, home_transfer).await.unwrap().paths[0]).unwrap(),
        b"owner to recipient"
    );
    let FileReply::DragPrepared(unrelated_drag) = recipient
        .engine
        .files()
        .request(FileCommand::PrepareDrag {
            offer,
            entries: vec![],
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    recipient
        .engine
        .files()
        .request(FileCommand::DropDrag {
            drag: unrelated_drag,
        })
        .await
        .unwrap();
    controller.engine.send(Command::Panic);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let FileReply::Receiving(unrelated_transfer) = recipient
        .engine
        .files()
        .request(FileCommand::CommitDrag {
            drag: unrelated_drag,
            destination: ReceiveDestination::Cache,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        std::fs::read(&wait(recipient, unrelated_transfer).await.unwrap().paths[0]).unwrap(),
        b"owner to recipient"
    );
    controller
        .mock
        .events
        .send(PlatformEvent::ClipboardChanged {
            mimes: vec!["text/uri-list".into(), "text/plain".into()],
            inline_text: Some("ordinary link".into()),
        })
        .unwrap();
    until("mixed MIME copy clears file owner", || {
        controller.engine.state().borrow().file_clipboard.is_none()
    })
    .await;
    until("engine clipboard source access released", || {
        native_weak.upgrade().is_none()
    })
    .await;
    owner.engine.send(Command::SetMasterEnabled(false));
    until("source disabled", || {
        !owner.engine.state().borrow().master_enabled
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        recipient
            .engine
            .files()
            .state()
            .borrow()
            .transfers
            .iter()
            .find(|t| t.id == id)
            .unwrap()
            .state,
        TransferState::Ready
    );
    assert!(received.paths[0].exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_descriptors_leases_sequential_promises_and_reclamation() {
    let _serial = FILE_TEST_LOCK.lock().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (a, b) = pair().await;
    let source = tempfile::tempdir().unwrap();
    let root = source.path().join("portal-root");
    std::fs::create_dir(&root).unwrap();
    for n in 0..40 {
        std::fs::write(root.join(format!("item-{n:02}")), format!("item {n}")).unwrap();
    }
    let access = Arc::new(());
    let weak = Arc::downgrade(&access);
    let roots = (0..40)
        .map(|n| SelectedRoot::Open {
            name: format!("item-{n:02}"),
            file: Arc::new(std::fs::File::open(root.join(format!("item-{n:02}"))).unwrap()),
        })
        .collect();
    std::fs::rename(&root, source.path().join("portal-path-gone")).unwrap();
    let id = a
        .engine
        .files()
        .offer_local(
            LocalSelection {
                roots,
                access: Some(access),
            },
            MachineId("files-b".into()),
            OfferOrigin::Selection,
        )
        .await
        .unwrap();
    until("descriptor offer delivered", || {
        b.engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .any(|o| o.id == id)
    })
    .await;
    assert!(weak.upgrade().is_some());
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, 0);
    let roots: Vec<_> = b
        .engine
        .files()
        .state()
        .borrow()
        .offers
        .iter()
        .find(|o| o.id == id)
        .unwrap()
        .manifest
        .entries
        .iter()
        .filter(|e| e.parent.is_none())
        .map(|e| e.id)
        .collect();
    for (n, entry) in roots.into_iter().enumerate() {
        let FileReply::DragPrepared(drag) = b
            .engine
            .files()
            .request(FileCommand::PrepareDrag {
                offer: id,
                entries: vec![entry],
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        b.engine
            .files()
            .request(FileCommand::DropDrag { drag })
            .await
            .unwrap();
        let FileReply::Receiving(transfer) = b
            .engine
            .files()
            .request(FileCommand::CommitDrag {
                drag,
                destination: ReceiveDestination::Cache,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        let received = wait(&b, transfer).await.unwrap();
        assert_eq!(
            std::fs::read(&received.paths[0]).unwrap(),
            format!("item {n}").as_bytes()
        );
        let lease = b.engine.files().retain_received(transfer).await.unwrap();
        assert_eq!(lease.files(), &received);
        assert_eq!(lease.manifest().entries.len(), 1);
        assert_eq!(lease.manifest().entries[0].id, entry);
        lease.manifest().validate().unwrap();
        assert!(b
            .engine
            .files()
            .request(FileCommand::ClearReceived { transfer })
            .await
            .is_err());
        let second = lease.clone();
        drop(lease);
        assert!(b
            .engine
            .files()
            .request(FileCommand::ClearReceived { transfer })
            .await
            .is_err());
        drop(second);
        b.engine
            .files()
            .request(FileCommand::ClearReceived { transfer })
            .await
            .unwrap();
        assert!(!received.paths[0].exists());
    }
    a.engine
        .files()
        .request(FileCommand::Revoke { offer: id })
        .await
        .unwrap();
    until("terminal source leases released", || {
        weak.upgrade().is_none()
    })
    .await;
    let path = source.path().join("empty");
    std::fs::write(&path, []).unwrap();
    let id = offer(&a, &b, vec![path]).await;
    for _ in 0..270 {
        let transfer = receive(&b, id, ReceiveDestination::Cache).await;
        wait(&b, transfer).await.unwrap();
        b.engine
            .files()
            .request(FileCommand::ClearReceived { transfer })
            .await
            .unwrap();
    }
    assert!(a.engine.files().state().borrow().transfers.len() < 256);
    assert_eq!(
        std::fs::read_dir(b._dir.path().join("files/records"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(b._dir.path().join("files/received"))
            .unwrap()
            .count(),
        0
    );
    for _ in 0..270 {
        let transfer = receive(
            &b,
            id,
            ReceiveDestination::Directory(source.path().join("missing-destination")),
        )
        .await;
        assert!(wait(&b, transfer).await.is_err());
    }
    assert!(b.engine.files().state().borrow().transfers.len() < 256);
    let destination = tempfile::tempdir().unwrap();
    let transfer = receive(
        &b,
        id,
        ReceiveDestination::Directory(destination.path().into()),
    )
    .await;
    let receipt = wait(&b, transfer).await.unwrap();
    b.engine
        .files()
        .request(FileCommand::ClearReceived { transfer })
        .await
        .unwrap();
    assert!(receipt.paths[0].exists());
    a.engine.send(Command::SetMachineEnabled(
        MachineId("files-b".into()),
        false,
    ));
    a.engine.send(Command::SetMachineEnabled(
        MachineId("files-b".into()),
        true,
    ));
    until("brief policy revocation invalidates offer", || {
        a.engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .find(|o| o.id == id)
            .is_some_and(|o| o.state == OfferState::Revoked)
    })
    .await;
    let id = offer(&a, &b, vec![source.path().join("empty")]).await;
    b.engine
        .files()
        .request(FileCommand::SetEnabled(false))
        .await
        .unwrap();
    b.engine
        .files()
        .request(FileCommand::SetEnabled(true))
        .await
        .unwrap();
    assert!(b
        .engine
        .files()
        .request(FileCommand::Receive {
            offer: id,
            destination: ReceiveDestination::Cache
        })
        .await
        .is_err());
    until("remote disable revoked source offer", || {
        a.engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .find(|o| o.id == id)
            .is_some_and(|o| o.state == OfferState::Revoked)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panic_retires_preparations_and_preserves_committed_native_receives() {
    let _serial = FILE_TEST_LOCK.lock().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (a, b) = pair().await;
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("payload");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    let id = offer(&a, &b, vec![path]).await;
    let FileReply::DragPrepared(uncommitted) = b
        .engine
        .files()
        .request(FileCommand::PrepareDrag {
            offer: id,
            entries: vec![],
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let FileReply::DragPrepared(accepted) = b
        .engine
        .files()
        .request(FileCommand::PrepareDrag {
            offer: id,
            entries: vec![],
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    b.engine
        .files()
        .request(FileCommand::DropDrag { drag: accepted })
        .await
        .unwrap();
    let FileReply::Receiving(transfer) = b
        .engine
        .files()
        .request(FileCommand::CommitDrag {
            drag: accepted,
            destination: ReceiveDestination::Cache,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(b
        .engine
        .files()
        .request(FileCommand::ClearReceived { transfer })
        .await
        .is_err());
    b.engine
        .files()
        .request(FileCommand::CancelPeerDrags {
            peer: MachineId("third-party".into()),
        })
        .await
        .unwrap();
    b.engine
        .files()
        .request(FileCommand::DropDrag { drag: uncommitted })
        .await
        .unwrap();
    b.engine
        .files()
        .request(FileCommand::CancelDrags)
        .await
        .unwrap();
    assert!(b
        .engine
        .files()
        .request(FileCommand::CommitDrag {
            drag: uncommitted,
            destination: ReceiveDestination::Cache
        })
        .await
        .is_err());
    let received = wait(&b, transfer).await.unwrap();
    assert_eq!(
        std::fs::metadata(&received.paths[0]).unwrap().len(),
        32 * 1024 * 1024
    );
    assert_eq!(
        b.engine
            .files()
            .request(FileCommand::CommitDrag {
                drag: accepted,
                destination: ReceiveDestination::Cache
            })
            .await
            .unwrap(),
        FileReply::Receiving(transfer)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_clipboard_bridge_keeps_own_capture_and_rejects_late_resolution() {
    let _lock = FILE_TEST_LOCK.lock().await;
    let (a, b) = pair().await;
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("selected");
    std::fs::write(&path, b"selection").unwrap();
    let capture = |generation| {
        a.engine
            .capture_file_clipboard(LocalSelection::paths(vec![path.clone()]), generation)
            .unwrap();
    };
    let generation = || {
        a.engine
            .state()
            .borrow()
            .file_clipboard
            .as_ref()
            .map(|reference| reference.generation)
    };
    capture(10);
    a.engine.clipboard_changed(10, vec![], None).unwrap();
    until("capture survives its own changed event", || {
        generation() == Some(10)
    })
    .await;
    a.engine.clipboard_changed(11, vec![], None).unwrap();
    capture(11);
    until("changed before capture also succeeds", || {
        generation() == Some(11)
    })
    .await;
    a.engine
        .clipboard_changed(
            12,
            vec!["text/plain".into()],
            Some("new ordinary copy".into()),
        )
        .unwrap();
    capture(11);
    until("new ordinary copy revokes older file selection", || {
        generation().is_none()
    })
    .await;
    a.engine.clipboard_changed(13, vec![], None).unwrap();
    capture(13);
    until("next file generation succeeds", || generation() == Some(13)).await;
    capture(12);
    a.engine.clipboard_changed(12, vec![], None).unwrap();
    a.mock
        .events
        .send(PlatformEvent::ClipboardChanged {
            mimes: vec![],
            inline_text: None,
        })
        .unwrap();
    a.engine.clipboard_changed(14, vec![], None).unwrap();
    capture(14);
    until("old and untagged events cannot undo new selection", || {
        generation() == Some(14)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(generation(), Some(14));
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, 0);
    assert_eq!(b.engine.files().state().borrow().payload_bytes_received, 0);
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_clear_of_unreadable_cache_folder_exposes_relocated_paths_until_repaired() {
    use std::os::unix::fs::PermissionsExt;
    let _lock = FILE_TEST_LOCK.lock().await;
    let (a, b) = pair().await;
    let source = tempfile::tempdir().unwrap();
    let root = source.path().join("bundle");
    std::fs::create_dir_all(root.join("locked")).unwrap();
    std::fs::write(root.join("locked/inner"), b"inner").unwrap();
    std::fs::write(root.join("payload"), b"relocated payload").unwrap();
    let id = offer(&a, &b, vec![root]).await;
    let FileReply::Receiving(transfer) = b
        .engine
        .files()
        .request(FileCommand::Receive {
            offer: id,
            destination: ReceiveDestination::Cache,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let received = wait(&b, transfer).await.unwrap();
    let locked = received.paths[0].join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let error = b
        .engine
        .files()
        .request(FileCommand::ClearReceived { transfer })
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("not readable"), "{error:#}");
    let relocated = {
        let state = b.engine.files().state();
        let state = state.borrow();
        let record = state
            .transfers
            .iter()
            .find(|record| record.id == transfer)
            .unwrap();
        assert_eq!(record.state, TransferState::Ready);
        assert!(record.error.is_some());
        assert_ne!(record.paths, received.paths);
        assert_eq!(record.paths.len(), 1);
        record.paths.clone()
    };
    assert!(!received.paths[0].exists());
    let retained = b.engine.files().retain_received(transfer).await.unwrap();
    assert_eq!(retained.files().paths, relocated);
    assert_eq!(
        std::fs::read(relocated[0].join("payload")).unwrap(),
        b"relocated payload"
    );
    assert!(b
        .engine
        .files()
        .request(FileCommand::ClearReceived { transfer })
        .await
        .is_err());
    drop(retained);
    std::fs::set_permissions(
        relocated[0].join("locked"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    b.engine
        .files()
        .request(FileCommand::ClearReceived { transfer })
        .await
        .unwrap();
    assert!(!relocated[0].exists());
    assert!(b
        .engine
        .files()
        .state()
        .borrow()
        .transfers
        .iter()
        .all(|record| record.id != transfer));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_recovery_and_failed_clear_do_not_disable_future_receives() {
    let _lock = FILE_TEST_LOCK.lock().await;
    let (a, b) = pair_with_corrupt_receipt(true).await;
    assert_eq!(b.engine.files().state().borrow().recovery_error_count, 1);
    assert_eq!(
        b.engine.files().state().borrow().recovery_errors[0].transfer,
        Some(TransferId([1; 16]))
    );
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("payload");
    std::fs::write(&path, b"future transfers remain usable").unwrap();
    let id = offer(&a, &b, vec![path]).await;
    let FileReply::Receiving(transfer) = b
        .engine
        .files()
        .request(FileCommand::Receive {
            offer: id,
            destination: ReceiveDestination::Cache,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let received = wait(&b, transfer).await.unwrap();
    let parent = received.paths[0].parent().unwrap();
    std::fs::remove_file(&received.paths[0]).unwrap();
    std::fs::write(&received.paths[0], b"future transfers remain usable").unwrap();
    assert!(b
        .engine
        .files()
        .request(FileCommand::ClearReceived { transfer })
        .await
        .is_err());
    {
        let state = b.engine.files().state();
        let state = state.borrow();
        let record = state
            .transfers
            .iter()
            .find(|record| record.id == transfer)
            .unwrap();
        assert_eq!(record.state, TransferState::Ready);
        assert_eq!(record.paths, received.paths);
        assert!(record.error.is_some());
        assert!(state.enabled);
    }
    let retained = b.engine.files().retain_received(transfer).await.unwrap();
    assert_eq!(
        std::fs::read(&retained.files().paths[0]).unwrap(),
        b"future transfers remain usable"
    );
    let FileReply::Receiving(next) = b
        .engine
        .files()
        .request(FileCommand::Receive {
            offer: id,
            destination: ReceiveDestination::Cache,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    wait(&b, next).await.unwrap();
    drop(retained);
    std::fs::remove_file(&received.paths[0]).unwrap();
    b.engine
        .files()
        .request(FileCommand::ClearReceived { transfer })
        .await
        .unwrap();
    assert!(!parent.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_publication_invalidation_preserves_remote_providers_and_newer_file_reference() {
    let _lock = FILE_TEST_LOCK.lock().await;
    let (a, b) = pair().await;
    let text = b"remote text provider".to_vec();
    let image = vec![0x75; 2 * 1024 * 1024];
    a.mock
        .state
        .lock()
        .local_clip
        .insert("text/plain".into(), text.clone());
    a.mock
        .state
        .lock()
        .local_clip
        .insert("image/png".into(), image.clone());
    a.engine
        .clipboard_changed(1, vec!["text/plain".into(), "image/png".into()], None)
        .unwrap();
    until("remote text and image provider installed", || {
        b.mock.last_fetch.lock().is_some()
    })
    .await;
    let provider = b.mock.last_fetch.lock().clone().unwrap();
    let image_fetch = {
        let provider = provider.clone();
        tokio::spawn(async move { provider.fetch("image/png").await })
    };
    for generation in 1..=5000 {
        b.engine.invalidate_file_clipboard(generation).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(image_fetch.await.unwrap(), Some(image));
    assert_eq!(provider.fetch("text/plain").await, Some(text));
    assert_eq!(b.mock.state.lock().remote_offers.len(), 1);
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("remote-files");
    std::fs::write(&path, b"file selection").unwrap();
    a.engine
        .capture_file_clipboard(LocalSelection::paths(vec![path.clone()]), 2)
        .unwrap();
    until("newer remote file reference", || {
        b.engine
            .state()
            .borrow()
            .file_clipboard
            .as_ref()
            .is_some_and(|reference| reference.stamp.writer == MachineId("files-a".into()))
    })
    .await;
    let reference = b.engine.state().borrow().file_clipboard.clone();
    b.engine.invalidate_file_clipboard(5001).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(b.engine.state().borrow().file_clipboard, reference);
    b.engine
        .clipboard_changed(
            5001,
            vec!["text/plain".into()],
            Some("a genuine local change".into()),
        )
        .unwrap();
    until(
        "same-generation Changed after Invalidated replaces remote reference",
        || b.engine.state().borrow().file_clipboard.is_none(),
    )
    .await;
    b.engine
        .capture_file_clipboard(LocalSelection::paths(vec![path]), 5001)
        .unwrap();
    until(
        "same-generation capture after invalidation and change",
        || {
            b.engine
                .state()
                .borrow()
                .file_clipboard
                .as_ref()
                .is_some_and(|reference| {
                    reference.generation == 5001
                        && reference.stamp.writer == MachineId("files-b".into())
                })
        },
    )
    .await;
    b.engine.invalidate_file_clipboard(5001).unwrap();
    b.engine.clipboard_changed(5001, vec![], None).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(b
        .engine
        .state()
        .borrow()
        .file_clipboard
        .as_ref()
        .is_some_and(|reference| reference.generation == 5001));
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn early_native_invalidation_retires_source_before_slow_capture_finishes() {
    let _lock = FILE_TEST_LOCK.lock().await;
    let (a, b) = pair().await;
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("selected");
    std::fs::write(&path, b"file selection").unwrap();
    let access = Arc::new(());
    let weak = Arc::downgrade(&access);
    a.engine
        .capture_file_clipboard(
            LocalSelection {
                roots: vec![SelectedRoot::Path(path.clone())],
                access: Some(access),
            },
            1,
        )
        .unwrap();
    until("first local capture", || {
        a.engine.state().borrow().file_clipboard.is_some()
    })
    .await;
    let FileReply::Offered(old) = a
        .engine
        .files()
        .request(FileCommand::RouteClipboard {
            recipient: MachineId("files-b".into()),
            generation: 1,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    until("old offer reached receiver", || {
        b.engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .any(|offer| offer.id == old)
    })
    .await;
    for _ in 0..200 {
        let _ = a.engine.files().send(FileCommand::CancelDrags);
    }
    for generation in 2..=5000 {
        a.engine.invalidate_file_clipboard(generation).unwrap();
    }
    until(
        "early invalidation releases source and revokes available offer",
        || {
            weak.upgrade().is_none()
                && a.engine.state().borrow().file_clipboard.is_none()
                && a.engine
                    .files()
                    .state()
                    .borrow()
                    .offers
                    .iter()
                    .any(|offer| offer.id == old && offer.state == OfferState::Revoked)
        },
    )
    .await;
    a.engine
        .capture_file_clipboard(LocalSelection::paths(vec![path.clone()]), 4999)
        .unwrap();
    a.engine
        .capture_file_clipboard(LocalSelection::paths(vec![path]), 5000)
        .unwrap();
    until("slow capture completes at invalidation generation", || {
        a.engine
            .state()
            .borrow()
            .file_clipboard
            .as_ref()
            .is_some_and(|reference| reference.generation == 5000)
    })
    .await;
    assert!(a
        .engine
        .files()
        .request(FileCommand::RouteClipboard {
            recipient: MachineId("files-b".into()),
            generation: 1
        })
        .await
        .is_err());
    let FileReply::Offered(new) = a
        .engine
        .files()
        .request(FileCommand::RouteClipboard {
            recipient: MachineId("files-b".into()),
            generation: 5000,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_ne!(old, new);
    assert_eq!(a.engine.files().state().borrow().payload_bytes_sent, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_dropped_on_an_edge_are_offered_to_the_machine_across_it() {
    let _serial = FILE_TEST_LOCK.lock().await;
    let (a, b) = pair().await;
    let source = tempfile::tempdir().unwrap();
    let one = source.path().join("edge-one.txt");
    let two = source.path().join("edge-two.txt");
    std::fs::write(&one, b"first").unwrap();
    std::fs::write(&two, b"second").unwrap();

    a.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    until("source edge armed", || {
        !a.mock.state.lock().edges.is_empty()
    })
    .await;
    let edge = a.mock.state.lock().edges[0].clone();

    a.mock
        .events
        .send(PlatformEvent::FileDrop {
            edge_id: edge.id,
            items: vec![(one.clone(), None), (two.clone(), None)],
        })
        .unwrap();

    until("edge-drop offer delivered", || {
        b.engine
            .files()
            .state()
            .borrow()
            .offers
            .iter()
            .any(|offer| {
                offer.recipient == MachineId("files-b".into())
                    && offer.owner == MachineId("files-a".into())
                    && {
                        let roots: std::collections::BTreeSet<&str> = offer
                            .manifest
                            .entries
                            .iter()
                            .filter(|entry| entry.parent.is_none())
                            .map(|entry| entry.name.as_str())
                            .collect();
                        roots.contains("edge-one.txt") && roots.contains("edge-two.txt")
                    }
            })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_dropped_on_an_edge_to_a_disconnected_peer_make_no_offer() {
    let _serial = FILE_TEST_LOCK.lock().await;
    let (a, _b) = pair().await;
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("lonely.txt");
    std::fs::write(&file, b"nobody home").unwrap();

    a.mock.events.send(PlatformEvent::PhysicalActivity).unwrap();
    until("source edge armed", || {
        !a.mock.state.lock().edges.is_empty()
    })
    .await;

    a.mock
        .events
        .send(PlatformEvent::FileDrop {
            edge_id: 4242,
            items: vec![(file, None)],
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        a.engine.files().state().borrow().offers.is_empty(),
        "an unknown edge must not create an offer"
    );
}
