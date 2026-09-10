use super::{
    manifest::Selection, storage::Staging, Cancellation, EntryKind, Manifest, Policy, TransferId,
};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use splice_proto::{
    files::{BULK_MAGIC, FILE_CHUNK, FILE_PORT},
    MachineId,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{mpsc, watch, Semaphore},
    task::JoinSet,
};

pub(super) const DEADLINE: Duration = Duration::from_secs(30);
pub(super) const AUTH_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct Guard {
    pub cancel: Cancellation,
    pub policy: watch::Receiver<Policy>,
    pub net: crate::net::NetControl,
    pub peer: MachineId,
    pub generation: u64,
    pub epoch: u64,
}

impl Guard {
    pub fn check(&self) -> Result<()> {
        self.cancel.check()?;
        let policy = self.policy.borrow();
        anyhow::ensure!(
            policy.enabled
                && policy.epochs.get(&self.peer).copied().unwrap_or(0) == self.epoch
                && policy
                    .peers
                    .get(&self.peer)
                    .is_some_and(|p| p.0 == self.generation)
                && self.net.current_connection(&self.peer, self.generation),
            "file authorization revoked or connection replaced"
        );
        Ok(())
    }

    pub async fn protect<T>(
        &self,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::pin!(future);
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        loop {
            tokio::select! {
                biased;
                _ = tick.tick() => { if let Err(error) = self.check() { self.cancel.cancel(); return Err(error); } },
                result = &mut future => return result,
            }
        }
    }
}

pub(super) struct Accepted {
    pub socket: TcpStream,
    pub peer: MachineId,
    pub transfer: TransferId,
    pub token: [u8; 32],
}

pub(super) async fn authenticate(
    ts: &Arc<dyn crate::net::TsApi>,
    address: SocketAddr,
) -> Result<MachineId> {
    tokio::time::timeout(AUTH_DEADLINE, async {
        let (status, who) = tokio::try_join!(ts.status(), ts.whois(address))?;
        match splice_tailscale::authorize(&status, &who) {
            splice_tailscale::AuthDecision::Peer(id) => Ok(MachineId(id)),
            _ => anyhow::bail!("Tailnet file peer not authorized"),
        }
    })
    .await
    .context("file WhoIs deadline")?
}

pub(super) async fn listen(
    listener: TcpListener,
    ts: Arc<dyn crate::net::TsApi>,
    incoming: mpsc::Sender<Accepted>,
) {
    listen_from(|| listener.accept(), ts, incoming).await;
}

async fn listen_from<F, Fut>(
    mut accept: F,
    ts: Arc<dyn crate::net::TsApi>,
    incoming: mpsc::Sender<Accepted>,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<(TcpStream, SocketAddr)>>,
{
    let admission = Arc::new(Semaphore::new(8));
    let mut jobs = JoinSet::new();
    let mut backoff = Duration::from_millis(25);
    loop {
        tokio::select! {
            _ = incoming.closed() => break,
            _ = jobs.join_next(), if !jobs.is_empty() => {},
            accepted = accept() => {
                let (mut socket, remote) = match accepted {
                    Ok(connection) => { backoff = Duration::from_millis(25); connection }
                    Err(error) => {
                        tracing::warn!(%error, "file listener accept failed");
                        tokio::select! { _ = incoming.closed() => break, _ = tokio::time::sleep(backoff) => {} }
                        backoff = (backoff * 2).min(Duration::from_secs(1));
                        continue;
                    }
                };
                let Ok(permit) = admission.clone().try_acquire_owned() else { continue; };
                let ts = ts.clone();
                let incoming = incoming.clone();
                jobs.spawn(async move {
                    let _permit = permit;
                    let result = tokio::time::timeout(AUTH_DEADLINE, async {
                        let peer = authenticate(&ts, remote).await?;
                        let mut header = [0; 56];
                        socket.read_exact(&mut header).await?;
                        anyhow::ensure!(&header[..8] == BULK_MAGIC, "invalid file stream protocol");
                        let mut transfer = [0; 16];
                        transfer.copy_from_slice(&header[8..24]);
                        let mut token = [0; 32];
                        token.copy_from_slice(&header[24..]);
                        Ok::<_, anyhow::Error>((peer, TransferId(transfer), token))
                    }).await;
                    if let Ok(Ok((peer, transfer, token))) = result {
                        let _ = incoming.try_send(Accepted { socket, peer, transfer, token });
                    }
                });
            }
        }
    }
    jobs.abort_all();
}

pub(super) async fn connect(
    bind: IpAddr,
    peer_ip: IpAddr,
    peer: &MachineId,
    transfer: TransferId,
    token: [u8; 32],
    ts: &Arc<dyn crate::net::TsApi>,
) -> Result<TcpStream> {
    tokio::time::timeout(AUTH_DEADLINE, async {
        let socket = if bind.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        socket.bind(SocketAddr::new(bind, 0))?;
        let mut stream = socket.connect(SocketAddr::new(peer_ip, FILE_PORT)).await?;
        anyhow::ensure!(
            &authenticate(ts, stream.peer_addr()?).await? == peer,
            "file server identity mismatch"
        );
        stream.write_all(BULK_MAGIC).await?;
        stream.write_all(&transfer.0).await?;
        stream.write_all(&token).await?;
        Ok(stream)
    })
    .await
    .context("file connection deadline")?
}

async fn write_payload(
    socket: &mut TcpStream,
    mut bytes: &[u8],
    counter: &AtomicU64,
    guard: &Guard,
) -> Result<()> {
    while !bytes.is_empty() {
        guard.check()?;
        let n = tokio::time::timeout(DEADLINE, socket.write(bytes))
            .await
            .context("file send progress deadline")??;
        anyhow::ensure!(n > 0, "file socket closed while sending");
        counter.fetch_add(n as u64, Ordering::Relaxed);
        bytes = &bytes[n..];
    }
    Ok(())
}

pub(super) async fn send(
    mut socket: TcpStream,
    source: Arc<Selection>,
    manifest: Manifest,
    guard: Guard,
    bytes: Arc<AtomicU64>,
) -> Result<()> {
    guard
        .protect(async {
            let selection = source.clone();
            let selected = manifest.clone();
            let cancellation = guard.cancel.clone();
            tokio::task::spawn_blocking(move || selection.validate(&selected, &cancellation))
                .await??;
            let mut buffer = vec![0; FILE_CHUNK];
            for entry in &manifest.entries {
                guard.check()?;
                let EntryKind::File { size } = entry.kind else {
                    continue;
                };
                let selection = source.clone();
                let id = entry.id;
                let file = tokio::task::spawn_blocking(move || selection.open(id)).await??;
                let file = Arc::new(file);
                let mut remaining = size;
                let mut hash = Sha256::new();
                while remaining > 0 {
                    guard.check()?;
                    let wanted = remaining.min(FILE_CHUNK as u64) as usize;
                    let input = file.clone();
                    let selection = source.clone();
                    let offset = size - remaining;
                    let read = tokio::task::spawn_blocking(move || {
                        use std::os::unix::fs::FileExt;
                        let _selection = selection;
                        let n = input.read_at(&mut buffer[..wanted], offset);
                        (buffer, n)
                    });
                    let (returned, n) = tokio::time::timeout(DEADLINE, read)
                        .await
                        .context("source read deadline")??;
                    buffer = returned;
                    let n = n?;
                    anyhow::ensure!(n > 0, "source truncated during transfer");
                    hash.update(&buffer[..n]);
                    write_payload(&mut socket, &buffer[..n], &bytes, &guard).await?;
                    remaining -= n as u64;
                }
                let selection = source.clone();
                tokio::task::spawn_blocking(move || selection.verify(id, &file)).await??;
                tokio::time::timeout(DEADLINE, socket.write_all(&hash.finalize())).await??;
            }
            let cancellation = guard.cancel.clone();
            tokio::task::spawn_blocking(move || source.validate(&manifest, &cancellation))
                .await??;
            guard.check()?;
            tokio::time::timeout(DEADLINE, socket.write_all(b"DONE")).await??;
            socket.shutdown().await?;
            Ok(())
        })
        .await
}

pub(super) async fn receive(
    socket: &mut TcpStream,
    staging: &Staging,
    guard: &Guard,
    bytes: &AtomicU64,
) -> Result<()> {
    guard
        .protect(receive_data(socket, staging, || guard.check(), bytes))
        .await
}

async fn receive_data(
    socket: &mut TcpStream,
    staging: &Staging,
    check: impl Fn() -> Result<()>,
    bytes: &AtomicU64,
) -> Result<()> {
    let mut buffer = vec![0; FILE_CHUNK];
    for entry in &staging.journal.manifest.entries {
        check()?;
        let EntryKind::File { size } = entry.kind else {
            continue;
        };
        let mut file = tokio::fs::File::from_std(staging.create_file(entry.id).await?);
        let mut remaining = size;
        let mut hash = Sha256::new();
        while remaining > 0 {
            check()?;
            let wanted = remaining.min(FILE_CHUNK as u64) as usize;
            let n = tokio::time::timeout(DEADLINE, socket.read(&mut buffer[..wanted]))
                .await
                .context("file receive progress deadline")??;
            anyhow::ensure!(n > 0, "file stream ended before declared size");
            bytes.fetch_add(n as u64, Ordering::Relaxed);
            hash.update(&buffer[..n]);
            tokio::time::timeout(DEADLINE, file.write_all(&buffer[..n]))
                .await
                .context("destination write deadline")??;
            remaining -= n as u64;
        }
        let mut expected = [0; 32];
        tokio::time::timeout(DEADLINE, socket.read_exact(&mut expected)).await??;
        anyhow::ensure!(
            hash.finalize().as_slice() == expected,
            "file SHA-256 mismatch"
        );
        tokio::time::timeout(DEADLINE, file.sync_all())
            .await
            .context("destination sync deadline")??;
    }
    let mut done = [0; 4];
    tokio::time::timeout(DEADLINE, socket.read_exact(&mut done)).await??;
    anyhow::ensure!(
        &done == b"DONE",
        "source did not verify completed selection"
    );
    let mut trailing = [0];
    anyhow::ensure!(
        tokio::time::timeout(DEADLINE, socket.read(&mut trailing)).await?? == 0,
        "unexpected trailing file data"
    );
    check()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{
        storage, EntryId, FileOfferId, ManifestEntry, ReceiveDestination, TransferDirection,
        TransferRecord, TransferState,
    };

    struct Identity;
    impl crate::net::TsApi for Identity {
        fn status(
            &self,
        ) -> futures::future::BoxFuture<'_, splice_tailscale::Result<splice_tailscale::Status>>
        {
            Box::pin(async {
                Ok(splice_tailscale::Status {
                    self_node: splice_tailscale::Node {
                        stable_id: "receiver".into(),
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
        ) -> futures::future::BoxFuture<'_, splice_tailscale::Result<splice_tailscale::WhoIs>>
        {
            Box::pin(async {
                Ok(splice_tailscale::WhoIs {
                    node_stable_id: "sender".into(),
                    user: splice_tailscale::WhoIsUser {
                        id: 7,
                        login_name: "test".into(),
                    },
                })
            })
        }
    }

    #[tokio::test]
    async fn listener_recovers_after_transient_accept_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let (sender, mut accepted) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            listen_from(
                || {
                    let call = counter.fetch_add(1, Ordering::Relaxed);
                    let listener = &listener;
                    async move {
                        if call < 3 {
                            Err(std::io::Error::from_raw_os_error(libc::EMFILE))
                        } else {
                            listener.accept().await
                        }
                    }
                },
                Arc::new(Identity),
                sender,
            )
            .await;
        });
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(BULK_MAGIC).await.unwrap();
        socket.write_all(&[1; 16]).await.unwrap();
        socket.write_all(&[2; 32]).await.unwrap();
        let connection = tokio::time::timeout(Duration::from_secs(3), accepted.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(connection.peer, MachineId("sender".into()));
        assert_eq!(connection.transfer, TransferId([1; 16]));
        assert!(calls.load(Ordering::Relaxed) >= 4);
        drop(accepted);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn corrupt_truncated_and_extended_streams_never_publish_unverified_files() {
        for mode in ["valid", "hash", "truncated", "trailing"] {
            let base = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            storage::initialize(base.path()).unwrap();
            let record = TransferRecord {
                id: TransferId(crate::files::random().unwrap()),
                offer: FileOfferId([1; 16]),
                peer: MachineId("source".into()),
                direction: TransferDirection::Receive,
                state: TransferState::Receiving,
                bytes: 0,
                total_bytes: 7,
                paths: vec![],
                error: None,
            };
            let manifest = Manifest {
                generation: [2; 16],
                entries: vec![ManifestEntry {
                    mode: 0o700,
                    modified: splice_proto::files::FileTimestamp {
                        seconds: 1_700_000_000,
                        nanos: 0,
                    },
                    id: EntryId(0),
                    parent: None,
                    name: "received".into(),
                    kind: EntryKind::File { size: 7 },
                }],
                total_bytes: 7,
            };
            let mut staging = Staging::new(
                base.path().into(),
                record,
                ReceiveDestination::Directory(destination.path().into()),
                manifest,
                &Cancellation::default(),
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let writer = tokio::spawn(async move {
                let mut stream = TcpStream::connect(address).await.unwrap();
                stream.write_all(b"payload").await.unwrap();
                if mode != "truncated" {
                    let digest: [u8; 32] = if mode == "hash" {
                        [0; 32]
                    } else {
                        Sha256::digest(b"payload").into()
                    };
                    stream.write_all(&digest).await.unwrap();
                    stream.write_all(b"DONE").await.unwrap();
                    if mode == "trailing" {
                        stream.write_all(b"extra").await.unwrap();
                    }
                }
                stream.shutdown().await.unwrap();
            });
            let (mut socket, _) = listener.accept().await.unwrap();
            let bytes = AtomicU64::new(0);
            let result = receive_data(&mut socket, &staging, || Ok(()), &bytes).await;
            writer.await.unwrap();
            assert_eq!(bytes.load(Ordering::Relaxed), 7);
            assert!(!destination.path().join("received").exists());
            if mode == "valid" {
                result.unwrap();
                staging.complete(&Cancellation::default()).unwrap();
                assert_eq!(
                    std::fs::read(destination.path().join("received")).unwrap(),
                    b"payload"
                );
            } else {
                assert!(result.is_err(), "{mode}");
                staging.fail(format!("{mode} failed"), false, 7).unwrap();
                assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
            }
        }
    }
}
