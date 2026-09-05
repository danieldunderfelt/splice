use crate::net::NetControl;
use parking_lot::Mutex;
use splice_platform::ClipFetch;
use splice_proto::{Frame, MachineId, CLIP_MAX_TOTAL};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING: usize = 64;
const MAX_BUFFERED: usize = 64 * 1024 * 1024;

type FetchKey = (MachineId, u64);

struct PendingFetch {
    bytes: Vec<u8>,
    done: oneshot::Sender<Option<Vec<u8>>>,
}

#[derive(Clone, Default)]
pub(crate) struct Transfers {
    next_request: Arc<AtomicU64>,
    generation: Arc<AtomicU64>,
    origins: Arc<Mutex<HashMap<MachineId, Arc<AtomicBool>>>>,
    pending: Arc<Mutex<HashMap<FetchKey, PendingFetch>>>,
}

impl Transfers {
    pub fn offer(
        &self,
        net: NetControl,
        origin: MachineId,
        id: u64,
        mimes: Vec<String>,
    ) -> Arc<dyn ClipFetch> {
        let connected = self
            .origins
            .lock()
            .entry(origin.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(true)))
            .clone();
        Arc::new(RemoteFetch {
            net,
            origin,
            id,
            mimes,
            connected,
            generation: self.generation.load(Ordering::Relaxed),
            transfers: self.clone(),
        })
    }

    pub fn chunk(&self, origin: &MachineId, request: u64, data: Vec<u8>, last: bool) {
        let key = (origin.clone(), request);
        let mut pending = self.pending.lock();
        let Some(mut fetch) = pending.remove(&key) else {
            return;
        };
        let buffered = fetch.bytes.len()
            + pending
                .values()
                .map(|fetch| fetch.bytes.len())
                .sum::<usize>();
        if data.len() > CLIP_MAX_TOTAL.saturating_sub(fetch.bytes.len())
            || data.len() > MAX_BUFFERED.saturating_sub(buffered)
        {
            tracing::warn!(peer = %origin, request, "clipboard transfer exceeds size limit");
            return;
        }
        fetch.bytes.extend_from_slice(&data);
        if last {
            let _ = fetch.done.send(Some(fetch.bytes));
        } else {
            pending.insert(key, fetch);
        }
    }

    pub fn abort(&self, origin: &MachineId, request: u64) {
        self.pending.lock().remove(&(origin.clone(), request));
    }

    pub fn disconnect(&self, origin: &MachineId) {
        let mut pending = self.pending.lock();
        if let Some(connected) = self.origins.lock().remove(origin) {
            connected.store(false, Ordering::Relaxed);
        }
        pending.retain(|(peer, _), _| peer != origin);
    }

    pub fn clear(&self) {
        let mut pending = self.pending.lock();
        self.generation.fetch_add(1, Ordering::Relaxed);
        pending.clear();
    }
}

struct RequestGuard {
    transfers: Transfers,
    key: FetchKey,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.transfers.pending.lock().remove(&self.key);
    }
}

struct RemoteFetch {
    net: NetControl,
    origin: MachineId,
    id: u64,
    mimes: Vec<String>,
    generation: u64,
    connected: Arc<AtomicBool>,
    transfers: Transfers,
}

#[async_trait::async_trait]
impl ClipFetch for RemoteFetch {
    async fn fetch(&self, mime: &str) -> Option<Vec<u8>> {
        if !self.mimes.iter().any(|offered| offered == mime) {
            return None;
        }
        let request = self.transfers.next_request.fetch_add(1, Ordering::Relaxed);
        let key = (self.origin.clone(), request);
        let (done, reply) = oneshot::channel();
        {
            let mut pending = self.transfers.pending.lock();
            if !self.connected.load(Ordering::Relaxed)
                || self.generation != self.transfers.generation.load(Ordering::Relaxed)
            {
                return None;
            }
            if pending.len() >= MAX_PENDING {
                tracing::warn!("too many pending clipboard transfers");
                return None;
            }
            pending.insert(
                key.clone(),
                PendingFetch {
                    bytes: Vec::new(),
                    done,
                },
            );
        }
        let _guard = RequestGuard {
            transfers: self.transfers.clone(),
            key,
        };
        if !self.net.send_to(
            &self.origin,
            Frame::ClipRequest {
                id: self.id,
                request,
                mime: mime.into(),
            },
        ) {
            return None;
        }
        match tokio::time::timeout(FETCH_TIMEOUT, reply).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => None,
            Err(_) => {
                tracing::warn!(peer = %self.origin, request, "clipboard request timed out");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(
        transfers: &Transfers,
        peer: &str,
        request: u64,
    ) -> oneshot::Receiver<Option<Vec<u8>>> {
        let (done, reply) = oneshot::channel();
        transfers.pending.lock().insert(
            (MachineId(peer.into()), request),
            PendingFetch {
                bytes: Vec::new(),
                done,
            },
        );
        reply
    }

    #[tokio::test]
    async fn another_peer_cannot_complete_or_abort_a_request() {
        let transfers = Transfers::default();
        let mut reply = pending(&transfers, "aaa", 1);
        transfers.chunk(&MachineId("bbb".into()), 1, b"wrong".to_vec(), true);
        transfers.abort(&MachineId("bbb".into()), 1);
        assert!(matches!(
            reply.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        transfers.chunk(&MachineId("aaa".into()), 1, b"right".to_vec(), true);
        assert_eq!(reply.await.unwrap(), Some(b"right".to_vec()));
    }

    #[tokio::test]
    async fn disconnect_cancels_only_that_peers_requests_and_handles() {
        let transfers = Transfers::default();
        let old_connection = Arc::new(AtomicBool::new(true));
        transfers
            .origins
            .lock()
            .insert(MachineId("aaa".into()), old_connection.clone());
        let canceled = pending(&transfers, "aaa", 1);
        let retained = pending(&transfers, "bbb", 2);
        transfers.disconnect(&MachineId("aaa".into()));
        assert!(!old_connection.load(Ordering::Relaxed));
        assert!(canceled.await.is_err());
        transfers.chunk(&MachineId("bbb".into()), 2, b"retained".to_vec(), true);
        assert_eq!(retained.await.unwrap(), Some(b"retained".to_vec()));
    }

    #[tokio::test]
    async fn oversized_transfers_are_canceled_and_freed() {
        let transfers = Transfers::default();
        let reply = pending(&transfers, "aaa", 1);
        transfers.chunk(&MachineId("aaa".into()), 1, vec![0; CLIP_MAX_TOTAL], false);
        transfers.chunk(&MachineId("aaa".into()), 1, vec![1], true);
        assert!(reply.await.is_err());
        assert!(transfers.pending.lock().is_empty());
    }
}
