use super::*;

struct LocalApi {
    own: String,
    peer: String,
}

impl TsApi for LocalApi {
    fn status(
        &self,
    ) -> futures::future::BoxFuture<'_, splice_tailscale::Result<splice_tailscale::Status>> {
        Box::pin(async {
            Ok(splice_tailscale::Status {
                self_node: splice_tailscale::Node {
                    stable_id: self.own.clone(),
                    user_id: 7,
                    ..Default::default()
                },
                peers: vec![],
            })
        })
    }

    fn whois(
        &self,
        _: SocketAddr,
    ) -> futures::future::BoxFuture<'_, splice_tailscale::Result<splice_tailscale::WhoIs>> {
        Box::pin(async {
            Ok(splice_tailscale::WhoIs {
                node_stable_id: self.peer.clone(),
                user: splice_tailscale::WhoIsUser {
                    id: 7,
                    login_name: "test".into(),
                },
            })
        })
    }
}

#[tokio::test]
async fn full_file_dispatch_queue_does_not_disconnect_authenticated_control() {
    let ports = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let mut nodes = Vec::new();
    for (own, peer) in [
        ("file-queue-a", "file-queue-b"),
        ("file-queue-b", "file-queue-a"),
    ] {
        let info = splice_proto::MachineInfo {
            id: MachineId(own.into()),
            hostname: own.into(),
            build: splice_proto::BuildInfo::current(),
            os: splice_proto::Os::Linux,
            displays: vec![],
        };
        let (manager, control) = NetManager::spawn_with(
            info,
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(LocalApi {
                own: own.into(),
                peer: peer.into(),
            }),
            NetOpts {
                dial_ports: ports.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        ports
            .write()
            .unwrap()
            .insert(MachineId(own.into()), manager.local_addr.port());
        nodes.push((manager, control));
    }
    let (sender, stalled) = mpsc::channel(1);
    nodes[1].1.file_receiver(Some(sender));
    nodes[0].1.update_dial_targets(vec![(
        MachineId("file-queue-b".into()),
        "127.0.0.1".parse().unwrap(),
    )]);
    for (manager, _) in &mut nodes {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    manager.events.recv().await.unwrap(),
                    PeerEvent::Connected { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
    let peer = MachineId("file-queue-b".into());
    let generation = nodes[0].1.connection_generation(&peer).unwrap();
    for n in 0..100 {
        assert!(
            nodes[0]
                .1
                .send_to_wait(
                    &peer,
                    Frame::Files(splice_proto::files::FileMessage::Cancel {
                        transfer: splice_proto::files::TransferId([n; 16])
                    })
                )
                .await
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(nodes[0].1.send_to_wait(&peer, Frame::ReleaseAll).await);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match nodes[1].0.events.recv().await.unwrap() {
                PeerEvent::Disconnected(_, reason) => panic!("control disconnected: {reason}"),
                PeerEvent::Frame(_, Frame::ReleaseAll) => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(stalled.len(), 1);
    assert!(nodes[0].1.current_connection(&peer, generation));
}
