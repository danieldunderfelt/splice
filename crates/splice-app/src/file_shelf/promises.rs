use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use splice_core::files::{EntryId, FileCommand, FileHandle, FileOfferId, FileReply, NativeDragId, OfferState, ReceiveDestination};
use splice_platform::file_shelf::{DragAttempt, DragOutcome, OfferId, PromiseCancellation, PromiseSource, PromiseWriter};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

struct RootPromise {
    drag: Mutex<Option<NativeDragId>>,
    transfer: Mutex<Option<splice_core::files::TransferId>>,
    cancellation: Mutex<Option<PromiseCancellation>>,
    requested: AtomicBool,
}

struct Attempt {
    offer: FileOfferId,
    roots: HashMap<u32, RootPromise>,
    cancelled: AtomicBool,
}

pub struct Promises {
    files: FileHandle,
    attempts: HashMap<DragAttempt, Arc<Attempt>>,
    deliveries: Arc<Semaphore>,
    scopes: Arc<RwLock<HashMap<splice_core::files::TransferId, Vec<u32>>>>,
}

impl Promises {
    pub fn new(files: FileHandle, scopes: Arc<RwLock<HashMap<splice_core::files::TransferId, Vec<u32>>>>) -> Self {
        Self { files, attempts: HashMap::new(), deliveries: Arc::new(Semaphore::new(1)), scopes }
    }

    pub fn register(&mut self, id: DragAttempt, offer: OfferId, roots: Vec<u32>) -> Result<()> {
        anyhow::ensure!(!self.attempts.contains_key(&id), "File drag is already registered");
        anyhow::ensure!(self.attempts.len() < 32, "Finish or cancel another file drag before starting a new one");
        let files = self.files.state();
        let state = files.borrow();
        let offer = state.offers.iter().find(|item| item.id.0 == offer.0 && item.state == OfferState::Available)
            .context("File offer is no longer available")?;
        let allowed: HashSet<_> = offer.manifest.entries.iter().filter(|entry| entry.parent.is_none())
            .map(|entry| entry.id.0).collect();
        anyhow::ensure!(!roots.is_empty() && roots.len() <= allowed.len(), "Invalid file drag selection");
        let unique: HashSet<_> = roots.iter().copied().collect();
        anyhow::ensure!(unique.len() == roots.len() && unique.is_subset(&allowed), "Invalid file drag selection");
        self.attempts.insert(id, Arc::new(Attempt {
            offer: offer.id,
            roots: roots.into_iter().map(|root| (root, RootPromise {
                drag: Mutex::new(None),
                transfer: Mutex::new(None),
                cancellation: Mutex::new(None),
                requested: AtomicBool::new(false),
            })).collect(),
            cancelled: AtomicBool::new(false),
        }));
        Ok(())
    }

    pub fn requested(&self, id: DragAttempt, root: u32, writer: PromiseWriter) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
        let attempt = self.attempts.get(&id).cloned();
        let files = self.files.clone();
        let deliveries = self.deliveries.clone();
        let scopes = self.scopes.clone();
        async move {
            let outcome = async {
                let attempt = attempt.context("File drag is no longer available")?;
                let promise = attempt.roots.get(&root).context("File was not part of this drag")?;
                anyhow::ensure!(!promise.requested.swap(true, Ordering::AcqRel), "This promised file was already requested");
                *promise.cancellation.lock() = Some(writer.cancellation());
                receive(&files, &attempt, promise, root, deliveries, scopes).await
            }.await;
            match outcome {
                Ok(lease) => {
                    writer.complete(Ok(PromiseSource {
                        path: lease.files().paths[0].clone(),
                        access: Some(Arc::new(lease)),
                    }));
                    Ok(())
                }
                Err(error) => {
                    writer.complete(Err(format!("{error:#}")));
                    Err(error)
                }
            }
        }
    }

    pub fn ended(&mut self, id: DragAttempt, outcome: DragOutcome) {
        let Some(attempt) = self.attempts.get(&id).cloned() else { return };
        if outcome == DragOutcome::Cancelled {
            attempt.cancelled.store(true, Ordering::Release);
            for promise in attempt.roots.values() {
                if let Some(cancellation) = &*promise.cancellation.lock() { cancellation.cancel(); }
                if let Some(drag) = *promise.drag.lock() {
                    let files = self.files.clone();
                    tokio::spawn(async move {
                        if let Err(error) = files.request(FileCommand::CancelDrag { drag }).await {
                            tracing::warn!(%error, "could not cancel the promised file transfer");
                        }
                    });
                }
            }
            self.attempts.remove(&id);
        }
    }

    pub fn retired(&mut self, id: DragAttempt) {
        self.attempts.remove(&id);
    }

    pub fn cancel_transfer(&self, transfer: splice_core::files::TransferId) {
        for attempt in self.attempts.values() {
            for promise in attempt.roots.values() {
                if *promise.transfer.lock() == Some(transfer) {
                    if let Some(cancellation) = &*promise.cancellation.lock() { cancellation.cancel(); }
                }
            }
        }
    }
}

async fn receive(files: &FileHandle, attempt: &Attempt, promise: &RootPromise, root: u32, deliveries: Arc<Semaphore>, scopes: Arc<RwLock<HashMap<splice_core::files::TransferId, Vec<u32>>>>) -> Result<splice_core::files::ReceivedLease> {
    let _permit = deliveries.acquire_owned().await.context("File promise queue stopped")?;
    anyhow::ensure!(!attempt.cancelled.load(Ordering::Acquire), "File drag was cancelled");
    let reply = files.request(FileCommand::PrepareDrag {
        offer: attempt.offer,
        entries: vec![EntryId(root)],
    }).await?;
    let FileReply::DragPrepared(drag) = reply else { anyhow::bail!("Unexpected file preparation response") };
    *promise.drag.lock() = Some(drag);
    if attempt.cancelled.load(Ordering::Acquire) {
        files.request(FileCommand::CancelDrag { drag }).await?;
        anyhow::bail!("File drag was cancelled");
    }
    files.request(FileCommand::DropDrag { drag }).await?;
    let reply = files.request(FileCommand::CommitDrag { drag, destination: ReceiveDestination::Cache }).await?;
    let FileReply::Receiving(transfer) = reply else { anyhow::bail!("Unexpected file receipt response") };
    *promise.transfer.lock() = Some(transfer);
    scopes.write().insert(transfer, vec![root]);
    let received = files.wait(transfer).await?;
    anyhow::ensure!(!attempt.cancelled.load(Ordering::Acquire), "File drag was cancelled");
    anyhow::ensure!(received.paths.len() == 1, "Promised file receipt returned an unexpected number of roots");
    files.retain_received(transfer).await
}
