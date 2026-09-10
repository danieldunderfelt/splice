use super::ids;
use super::journal::{Journal, ViewRecord};
use parking_lot::{Mutex, RwLock};
use splice_core::files::{FileCommand, FileHandle, FileOfferId, FileReply, Manifest, NativeDragId, ReceiveDestination, ReceivedLease, TransferId};
use splice_files::ipc::ReceiptStateDesc;
use splice_platform::files::{EntryId, FileContentSource, FileError, ViewId};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::OnceCell;

struct View {
    drag: Option<NativeDragId>,
    offer: FileOfferId,
    origin: String,
    manifest: Manifest,
    dropped: AtomicBool,
    committed: AtomicBool,
    cancelled: AtomicBool,
    drop_result: OnceCell<Result<(), String>>,
    received: OnceCell<Result<TransferId, String>>,
}

struct Receipt {
    offer: FileOfferId,
    origin: String,
    manifest: Manifest,
    lease: Option<ReceivedLease>,
    error: Option<String>,
    clearing: bool,
    persisted: HashSet<ViewId>,
    transient: HashSet<ViewId>,
}

impl Receipt {
    fn state(&self) -> ReceiptStateDesc {
        if self.clearing {
            ReceiptStateDesc::Clearing
        } else if self.lease.is_some() {
            ReceiptStateDesc::Retained
        } else {
            ReceiptStateDesc::Unavailable
        }
    }

    fn record(&self, transfer: TransferId) -> ViewRecord {
        let mut views: Vec<String> = self.persisted.iter().map(ViewId::to_string).collect();
        views.sort();
        ViewRecord { views, offer: self.offer, transfer, origin: self.origin.clone(), manifest: self.manifest.clone() }
    }

    fn restored(&self, id: ViewId, transfer: TransferId) -> RestoredView {
        RestoredView { id, transfer, offer: self.offer, origin: self.origin.clone(), manifest: self.manifest.clone() }
    }

    fn view(&self, transfer: TransferId) -> Arc<View> {
        Arc::new(View {
            drag: None,
            offer: self.offer,
            origin: self.origin.clone(),
            manifest: self.manifest.clone(),
            dropped: AtomicBool::new(true),
            committed: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            drop_result: OnceCell::new_with(Some(Ok(()))),
            received: OnceCell::new_with(Some(Ok(transfer))),
        })
    }
}

pub struct RestoredView {
    pub id: ViewId,
    pub transfer: TransferId,
    pub offer: FileOfferId,
    pub origin: String,
    pub manifest: Manifest,
}

pub struct ReceiptSummary {
    pub transfer: TransferId,
    pub offer: FileOfferId,
    pub origin: String,
    pub names: Vec<String>,
    pub total_bytes: u64,
    pub state: ReceiptStateDesc,
    pub error: Option<String>,
}

pub struct Receives {
    current: Mutex<HashMap<FileOfferId, TransferId>>,
    revision: AtomicU64,
}

impl Receives {
    pub fn new() -> Self {
        Self { current: Mutex::new(HashMap::new()), revision: AtomicU64::new(1) }
    }

    pub fn record(&self, offer: FileOfferId, transfer: TransferId) {
        self.current.lock().insert(offer, transfer);
        self.revision.fetch_add(1, Ordering::AcqRel);
    }

    pub fn current(&self, offer: FileOfferId) -> Option<TransferId> {
        self.current.lock().get(&offer).copied()
    }

    pub fn retain(&self, keep: impl Fn(FileOfferId) -> bool) {
        self.current.lock().retain(|offer, _| keep(*offer));
    }

    pub fn clear(&self) {
        self.current.lock().clear();
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
}

pub struct Content {
    files: RwLock<Option<FileHandle>>,
    receives: Receives,
    runtime: tokio::runtime::Handle,
    views: RwLock<HashMap<ViewId, Arc<View>>>,
    receipts: RwLock<HashMap<TransferId, Receipt>>,
    journal: Journal,
    version: AtomicU64,
}

impl Content {
    pub fn new(runtime: tokio::runtime::Handle, journal: Journal) -> Arc<Self> {
        let receipts: HashMap<TransferId, Receipt> = journal
            .records()
            .into_iter()
            .map(|record| {
                let persisted = record.views.iter().filter_map(|view| view.parse::<ViewId>().ok()).collect();
                (record.transfer, Receipt { offer: record.offer, origin: record.origin, manifest: record.manifest, lease: None, error: None, clearing: false, persisted, transient: HashSet::new() })
            })
            .collect();
        let views = receipts.iter().flat_map(|(transfer, receipt)| receipt.persisted.iter().map(|id| (*id, receipt.view(*transfer)))).collect();
        Arc::new(Self {
            files: RwLock::new(None),
            receives: Receives::new(),
            runtime,
            views: RwLock::new(views),
            receipts: RwLock::new(receipts),
            journal,
            version: AtomicU64::new(1),
        })
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    pub fn receives(&self) -> &Receives {
        &self.receives
    }

    pub fn set_engine(&self, files: FileHandle) {
        *self.files.write() = Some(files);
    }

    pub fn detach(&self) {
        *self.files.write() = None;
        for receipt in self.receipts.write().values_mut() {
            receipt.lease = None;
            receipt.error = Some("Splice engine is offline".into());
        }
        self.bump();
    }

    pub fn restored_views(&self) -> Vec<RestoredView> {
        self.receipts.read().iter().flat_map(|(transfer, receipt)| receipt.persisted.iter().map(|id| receipt.restored(*id, *transfer))).collect()
    }

    pub fn persisted_views(&self, transfer: TransferId) -> Vec<RestoredView> {
        self.receipts.read().get(&transfer).map(|receipt| receipt.persisted.iter().map(|id| receipt.restored(*id, transfer)).collect()).unwrap_or_default()
    }

    pub fn unrestored(&self) -> Vec<TransferId> {
        self.receipts.read().iter().filter(|(_, receipt)| receipt.lease.is_none() && !receipt.clearing).map(|(transfer, _)| *transfer).collect()
    }

    pub fn install_lease(&self, transfer: TransferId, result: Result<ReceivedLease, String>) {
        let mut receipts = self.receipts.write();
        let Some(receipt) = receipts.get_mut(&transfer) else { return };
        if receipt.clearing {
            return;
        }
        match result {
            Ok(lease) => {
                receipt.lease = Some(lease);
                receipt.error = None;
            }
            Err(error) => {
                if receipt.lease.is_none() {
                    receipt.error = Some(error);
                }
            }
        }
        drop(receipts);
        self.bump();
    }

    pub fn register(&self, id: ViewId, drag: NativeDragId, offer: FileOfferId, origin: String, manifest: Manifest) {
        self.views.write().insert(id, Arc::new(View {
            drag: Some(drag),
            offer,
            origin,
            manifest,
            dropped: AtomicBool::new(false),
            committed: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            drop_result: OnceCell::new(),
            received: OnceCell::new(),
        }));
    }

    pub fn add_receipt(&self, transfer: TransferId, offer: FileOfferId, origin: String, manifest: Manifest, lease: ReceivedLease) -> Result<(), String> {
        self.journal.insert(ViewRecord { views: Vec::new(), offer, transfer, origin: origin.clone(), manifest: manifest.clone() })?;
        self.receipts.write().insert(transfer, Receipt { offer, origin, manifest, lease: Some(lease), error: None, clearing: false, persisted: HashSet::new(), transient: HashSet::new() });
        self.bump();
        Ok(())
    }

    pub fn forget_receipt(&self, transfer: TransferId) -> (Vec<ViewId>, Result<(), String>) {
        let journal = self.journal.remove(transfer);
        let Some(receipt) = self.receipts.write().remove(&transfer) else {
            return (Vec::new(), journal);
        };
        let mut views = self.views.write();
        let mut retired = Vec::new();
        for id in receipt.persisted.iter().chain(receipt.transient.iter()) {
            if let Some(view) = views.remove(id) {
                self.cancel(&view);
                retired.push(*id);
            }
        }
        drop(views);
        drop(receipt);
        self.bump();
        (retired, journal)
    }

    pub fn summaries(&self) -> Vec<ReceiptSummary> {
        let mut summaries: Vec<ReceiptSummary> = self
            .receipts
            .read()
            .iter()
            .map(|(transfer, receipt)| ReceiptSummary {
                transfer: *transfer,
                offer: receipt.offer,
                origin: receipt.origin.clone(),
                names: receipt.manifest.entries.iter().filter(|entry| entry.parent.is_none()).map(|entry| entry.name.clone()).collect(),
                total_bytes: receipt.manifest.total_bytes,
                state: receipt.state(),
                error: receipt.error.clone(),
            })
            .collect();
        summaries.sort_by_key(|summary| summary.transfer);
        summaries
    }

    pub fn receipt_state(&self, transfer: TransferId) -> Option<ReceiptStateDesc> {
        self.receipts.read().get(&transfer).map(Receipt::state)
    }

    pub fn receipt_count(&self) -> usize {
        self.receipts.read().len()
    }

    pub fn receipt_lease(&self, transfer: TransferId) -> Option<ReceivedLease> {
        self.receipts.read().get(&transfer).filter(|receipt| !receipt.clearing).and_then(|receipt| receipt.lease.clone())
    }

    pub fn receipt_paths(&self, transfer: TransferId) -> Option<Vec<PathBuf>> {
        self.receipt_lease(transfer).map(|lease| lease.files().paths.clone())
    }

    pub fn receipt_manifest(&self, transfer: TransferId) -> Option<(FileOfferId, String, Manifest)> {
        let receipts = self.receipts.read();
        let receipt = receipts.get(&transfer)?;
        (receipt.state() == ReceiptStateDesc::Retained).then(|| (receipt.offer, receipt.origin.clone(), receipt.manifest.clone()))
    }

    pub fn register_receipt_view(&self, id: ViewId, transfer: TransferId) -> Result<(), String> {
        let mut receipts = self.receipts.write();
        let Some(receipt) = receipts.get_mut(&transfer) else {
            return Err("This receipt is no longer retained".into());
        };
        match receipt.state() {
            ReceiptStateDesc::Retained => {}
            ReceiptStateDesc::Clearing => return Err("These files are being cleared".into()),
            ReceiptStateDesc::Unavailable => return Err(receipt.error.clone().unwrap_or_else(|| "The received files are not available right now".into())),
        }
        let view = receipt.view(transfer);
        receipt.transient.insert(id);
        drop(receipts);
        self.views.write().insert(id, view);
        Ok(())
    }

    pub fn persist_receipt_view(&self, id: ViewId) -> Result<(), String> {
        let Some(view) = self.views.read().get(&id).cloned() else {
            return Err("The drag has expired".into());
        };
        let Some(Ok(transfer)) = view.received.get() else {
            return Ok(());
        };
        let mut receipts = self.receipts.write();
        let Some(receipt) = receipts.get_mut(transfer) else {
            return Err("This receipt is no longer retained".into());
        };
        if receipt.persisted.contains(&id) {
            return Ok(());
        }
        receipt.transient.remove(&id);
        receipt.persisted.insert(id);
        if let Err(error) = self.journal.insert(receipt.record(*transfer)) {
            receipt.persisted.remove(&id);
            receipt.transient.insert(id);
            return Err(error);
        }
        Ok(())
    }

    pub fn receipt_views(&self, transfer: TransferId) -> Vec<ViewId> {
        let receipts = self.receipts.read();
        let Some(receipt) = receipts.get(&transfer) else {
            return Vec::new();
        };
        receipt.persisted.iter().chain(receipt.transient.iter()).copied().collect()
    }

    pub fn begin_clear(&self, transfer: TransferId) -> Result<Vec<ViewId>, String> {
        let mut receipts = self.receipts.write();
        let Some(receipt) = receipts.get_mut(&transfer) else {
            return Err("This receipt is no longer retained".into());
        };
        if receipt.clearing {
            return Err("These files are already being cleared".into());
        }
        receipt.clearing = true;
        receipt.lease = None;
        receipt.error = None;
        let ids: Vec<ViewId> = receipt.persisted.iter().copied().chain(receipt.transient.drain()).collect();
        drop(receipts);
        let mut views = self.views.write();
        for id in &ids {
            if let Some(view) = views.remove(id) {
                self.cancel(&view);
            }
        }
        drop(views);
        self.bump();
        Ok(ids)
    }

    pub fn end_clear(&self, transfer: TransferId) {
        let mut receipts = self.receipts.write();
        if let Some(receipt) = receipts.get_mut(&transfer) {
            receipt.clearing = false;
            let mut views = self.views.write();
            for id in &receipt.persisted {
                views.insert(*id, receipt.view(transfer));
            }
        }
        drop(receipts);
        self.bump();
    }

    pub fn dropped(self: &Arc<Self>, id: ViewId) -> bool {
        let Some(view) = self.views.read().get(&id).cloned() else { return false };
        if view.cancelled.load(Ordering::Acquire) { return false; }
        view.dropped.store(true, Ordering::Release);
        let content = self.clone();
        self.runtime.spawn(async move {
            if let Err(error) = content.acknowledge_drop(&view).await {
                tracing::warn!(%error, "file drop was not accepted by its source");
            }
        });
        true
    }

    async fn acknowledge_drop<'a>(&self, view: &'a View) -> &'a Result<(), String> {
        view.drop_result.get_or_init(|| async {
            if view.cancelled.load(Ordering::Acquire) { return Err("File drag was cancelled".into()); }
            let Some(drag) = view.drag else { return Ok(()) };
            let files = self.files.read().clone().ok_or_else(|| "Splice engine is offline".to_string())?;
            files.request(FileCommand::DropDrag { drag }).await
                .map(|_| ())
                .map_err(|error| format!("{error:#}"))
        }).await
    }

    pub fn is_committed(&self, id: ViewId) -> bool {
        self.views.read().get(&id).is_some_and(|view| view.committed.load(Ordering::Acquire) && !view.cancelled.load(Ordering::Acquire))
    }

    pub fn retire(&self, id: ViewId) {
        let Some(view) = self.views.write().remove(&id) else { return };
        if let Some(Ok(transfer)) = view.received.get() {
            let mut receipts = self.receipts.write();
            if let Some(receipt) = receipts.get_mut(transfer) {
                receipt.transient.remove(&id);
                if receipt.persisted.remove(&id) {
                    if let Err(error) = self.journal.insert(receipt.record(*transfer)) {
                        tracing::warn!(%error, "retired receipt view could not be removed from the journal");
                    }
                }
            }
        }
        self.cancel(&view);
    }

    fn cancel(&self, view: &View) {
        if view.cancelled.swap(true, Ordering::AcqRel) { return; }
        let Some(drag) = view.drag else { return };
        let Some(files) = self.files.read().clone() else { return };
        self.runtime.spawn(async move {
            if let Err(error) = files.request(FileCommand::CancelDrag { drag }).await {
                tracing::debug!(%error, "native file drag was already released");
            }
        });
    }

    fn persist(&self, id: ViewId, view: &View, transfer: TransferId, lease: ReceivedLease) -> Result<(), String> {
        let receipt = Receipt {
            offer: view.offer,
            origin: view.origin.clone(),
            manifest: view.manifest.clone(),
            lease: Some(lease),
            error: None,
            clearing: false,
            persisted: HashSet::from([id]),
            transient: HashSet::new(),
        };
        self.journal.insert(receipt.record(transfer))?;
        self.receipts.write().insert(transfer, receipt);
        self.bump();
        Ok(())
    }
}

impl FileContentSource for Content {
    fn materialize(&self, id: ViewId, entry: EntryId) -> Result<PathBuf, FileError> {
        let view = self.views.read().get(&id).cloned()
            .ok_or_else(|| FileError::NotFound("File drag has expired".into()))?;
        if view.cancelled.load(Ordering::Acquire) { return Err(FileError::Cancelled); }
        if !view.dropped.load(Ordering::Acquire) || !view.committed.load(Ordering::Acquire) {
            return Err(FileError::Denied("Drop the file before reading its contents".into()));
        }
        let entry = ids::entry_from_native(entry).ok_or_else(|| FileError::NotFound("Unknown file".into()))?;
        let (root, relative) = relative_path(&view.manifest, entry)?;
        let received = self.runtime.block_on(view.received.get_or_init(|| async {
            self.acknowledge_drop(&view).await.clone()?;
            if view.cancelled.load(Ordering::Acquire) { return Err("File drag was cancelled".into()); }
            let drag = view.drag.ok_or_else(|| "File drag has expired".to_string())?;
            let files = self.files.read().clone().ok_or_else(|| "Splice engine is offline".to_string())?;
            let reply = files.request(FileCommand::CommitDrag {
                drag,
                destination: ReceiveDestination::Cache,
            }).await.map_err(|error| format!("{error:#}"))?;
            let FileReply::Receiving(transfer) = reply else {
                return Err("Unexpected response to the accepted file drop".into());
            };
            self.receives.record(view.offer, transfer);
            files.wait(transfer).await.map_err(|error| format!("{error:#}"))?;
            let lease = files.retain_received(transfer).await.map_err(|error| format!("{error:#}"))?;
            self.persist(id, &view, transfer, lease)?;
            Ok(transfer)
        }));
        if view.cancelled.load(Ordering::Acquire) { return Err(FileError::Cancelled); }
        let transfer = received.as_ref().map_err(|error| FileError::Unavailable(error.clone()))?;
        let lease = {
            let receipts = self.receipts.read();
            let receipt = receipts.get(transfer).ok_or_else(|| FileError::NotFound("The received files were cleared".into()))?;
            if receipt.clearing {
                return Err(FileError::Unavailable("The received files are being cleared".into()));
            }
            receipt.lease.clone().ok_or_else(|| FileError::Unavailable(receipt.error.clone().unwrap_or_else(|| "The received files are not available right now".into())))?
        };
        let base = lease.files().paths.iter()
            .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(root.as_str()))
            .ok_or_else(|| FileError::NotFound("Received file root is missing".into()))?;
        Ok(base.join(relative))
    }

    fn on_commit(&self, id: ViewId) {
        if let Some(view) = self.views.read().get(&id) {
            view.committed.store(true, Ordering::Release);
        }
    }

    fn on_cancel(&self, id: ViewId, _committed: bool) {
        if let Some(view) = self.views.read().get(&id).cloned() {
            self.cancel(&view);
        }
    }
}

fn relative_path(manifest: &Manifest, id: splice_core::files::EntryId) -> Result<(String, PathBuf), FileError> {
    let mut current = manifest.entries.iter().find(|entry| entry.id == id)
        .ok_or_else(|| FileError::NotFound("Unknown file".into()))?;
    let mut names = Vec::new();
    for _ in 0..=splice_proto::files::MAX_DEPTH {
        let Some(parent) = current.parent else {
            return Ok((current.name.clone(), names.into_iter().rev().collect()));
        };
        names.push(current.name.clone());
        current = manifest.entries.iter().find(|entry| entry.id == parent)
            .ok_or_else(|| FileError::NotFound("Unknown parent directory".into()))?;
    }
    Err(FileError::Denied("File directory tree is too deep".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use splice_core::files::{EntryId as CoreEntryId, EntryKind, ManifestEntry};

    fn manifest() -> Manifest {
        let stamp = splice_core::files::FileTimestamp { seconds: 1_700_000_000, nanos: 0 };
        Manifest {
            generation: [1; 16],
            total_bytes: 3,
            entries: vec![
                ManifestEntry { id: CoreEntryId(1), parent: None, name: "root".into(), kind: EntryKind::Directory, mode: 0o755, modified: stamp },
                ManifestEntry { id: CoreEntryId(2), parent: Some(CoreEntryId(1)), name: "sub".into(), kind: EntryKind::Directory, mode: 0o755, modified: stamp },
                ManifestEntry { id: CoreEntryId(3), parent: Some(CoreEntryId(2)), name: "file.txt".into(), kind: EntryKind::File { size: 3 }, mode: 0o644, modified: stamp },
                ManifestEntry { id: CoreEntryId(4), parent: None, name: "single.bin".into(), kind: EntryKind::File { size: 0 }, mode: 0o644, modified: stamp },
            ],
        }
    }

    fn journal(dir: &std::path::Path) -> Journal {
        Journal::open(dir.join("views.json")).unwrap().0
    }

    fn journaled_record(view: ViewId) -> ViewRecord {
        ViewRecord { views: vec![view.to_string()], offer: FileOfferId([6; 16]), transfer: TransferId([7; 16]), origin: "gamedev".into(), manifest: manifest() }
    }

    #[test]
    fn relative_paths_resolve_to_root_name_and_subpath() {
        let manifest = manifest();
        let (root, relative) = relative_path(&manifest, CoreEntryId(3)).unwrap();
        assert_eq!(root, "root");
        assert_eq!(relative, PathBuf::from("sub/file.txt"));
        let (root, relative) = relative_path(&manifest, CoreEntryId(4)).unwrap();
        assert_eq!(root, "single.bin");
        assert_eq!(relative, PathBuf::new());
        assert!(matches!(relative_path(&manifest, CoreEntryId(9)), Err(FileError::NotFound(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reads_are_denied_until_drop_and_commit_and_after_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let content = Content::new(tokio::runtime::Handle::current(), journal(dir.path()));
        let id = ViewId::new();
        content.register(id, NativeDragId([5; 16]), FileOfferId([6; 16]), "gamedev".into(), manifest());
        let entry = ids::entry_to_native(CoreEntryId(3));
        let content_for_read = content.clone();
        let denied = tokio::task::spawn_blocking(move || content_for_read.materialize(id, entry)).await.unwrap();
        assert!(matches!(denied, Err(FileError::Denied(_))));
        assert!(content.dropped(id));
        content.on_commit(id);
        assert!(content.is_committed(id));
        content.retire(id);
        let content_for_read = content.clone();
        let gone = tokio::task::spawn_blocking(move || content_for_read.materialize(id, entry)).await.unwrap();
        assert!(matches!(gone, Err(FileError::NotFound(_))));
        assert!(!content.dropped(id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_receipt_views_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let content = Content::new(tokio::runtime::Handle::current(), journal(dir.path()));
        let error = content.register_receipt_view(ViewId::new(), TransferId([9; 16])).unwrap_err();
        assert!(error.contains("no longer retained"), "{error}");
        assert!(content.summaries().is_empty());
        assert_eq!(content.persisted_views(TransferId([9; 16])).len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn journaled_receipts_load_unavailable_and_keep_their_views_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let view = ViewId::new();
        let transfer = TransferId([7; 16]);
        journal(dir.path()).insert(journaled_record(view)).unwrap();
        let content = Content::new(tokio::runtime::Handle::current(), journal(dir.path()));
        let summaries = content.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].state, ReceiptStateDesc::Unavailable);
        assert_eq!(summaries[0].names, vec!["root".to_string(), "single.bin".to_string()]);
        assert_eq!(content.unrestored(), vec![transfer]);
        let restored = content.restored_views();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, view);
        let error = content.register_receipt_view(ViewId::new(), transfer).unwrap_err();
        assert!(error.contains("not available"), "{error}");
        assert!(content.receipt_lease(transfer).is_none());
        let version = content.version();
        content.install_lease(transfer, Err("file service queue unavailable".into()));
        assert!(content.version() > version);
        assert_eq!(content.summaries()[0].error.as_deref(), Some("file service queue unavailable"));
        assert_eq!(journal(dir.path()).records().len(), 1);

        let cleared = content.begin_clear(transfer).unwrap();
        assert_eq!(cleared, vec![view]);
        assert_eq!(content.receipt_state(transfer), Some(ReceiptStateDesc::Clearing));
        assert!(content.unrestored().is_empty());
        assert!(content.begin_clear(transfer).is_err());
        content.end_clear(transfer);
        assert_eq!(content.receipt_state(transfer), Some(ReceiptStateDesc::Unavailable));
        assert_eq!(content.persisted_views(transfer).len(), 1);
        assert_eq!(journal(dir.path()).records()[0].views, vec![view.to_string()]);

        let (retired, journal_result) = content.forget_receipt(transfer);
        assert_eq!(retired, vec![view]);
        journal_result.unwrap();
        assert!(content.summaries().is_empty());
        assert!(journal(dir.path()).records().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retiring_a_persisted_view_updates_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let view = ViewId::new();
        journal(dir.path()).insert(journaled_record(view)).unwrap();
        let content = Content::new(tokio::runtime::Handle::current(), journal(dir.path()));
        let entry = ids::entry_to_native(CoreEntryId(3));
        let content_for_read = content.clone();
        let unavailable = tokio::task::spawn_blocking(move || content_for_read.materialize(view, entry)).await.unwrap();
        assert!(matches!(unavailable, Err(FileError::Unavailable(_))), "{unavailable:?}");
        content.retire(view);
        assert!(content.persisted_views(TransferId([7; 16])).is_empty());
        assert!(journal(dir.path()).records()[0].views.is_empty());
        assert!(content.persist_receipt_view(view).is_err());
        let content_for_read = content.clone();
        let gone = tokio::task::spawn_blocking(move || content_for_read.materialize(view, entry)).await.unwrap();
        assert!(matches!(gone, Err(FileError::NotFound(_))));
    }
}
