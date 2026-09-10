use splice_proto::MachineId;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};

pub const EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct OfferId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct TransferId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct DragAttempt(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ReceiveIntent(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryKind {
    Directory,
    File { size: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShelfEntry {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OfferState {
    Available,
    Revoked,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShelfOffer {
    pub id: OfferId,
    pub owner: MachineId,
    pub recipient: MachineId,
    pub entries: Vec<ShelfEntry>,
    pub total_bytes: u64,
    pub expires_unix_ms: u64,
    pub state: OfferState,
}

impl ShelfOffer {
    pub fn roots(&self) -> impl Iterator<Item = &ShelfEntry> {
        self.entries.iter().filter(|e| e.parent.is_none())
    }

    pub fn has_directory_root(&self) -> bool {
        self.roots().any(|r| r.kind == EntryKind::Directory)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TransferDirection {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TransferState {
    Preparing,
    Committed,
    Receiving,
    Verifying,
    Ready,
    Cancelled,
    Failed,
}

impl TransferState {
    pub fn active(self) -> bool {
        matches!(self, Self::Preparing | Self::Committed | Self::Receiving | Self::Verifying)
    }
}

pub type Access = Arc<dyn Send + Sync>;

#[derive(Clone)]
pub struct ReceivedSelection {
    pub transfer: TransferId,
    pub paths: Vec<PathBuf>,
    pub access: Option<Access>,
}

impl ReceivedSelection {
    pub fn new(transfer: TransferId, paths: Vec<PathBuf>, access: Option<Access>) -> Self {
        Self { transfer, paths, access }
    }
}

impl PartialEq for ReceivedSelection {
    fn eq(&self, other: &Self) -> bool {
        self.transfer == other.transfer
            && self.paths == other.paths
            && match (&self.access, &other.access) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for ReceivedSelection {}

impl fmt::Debug for ReceivedSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceivedSelection")
            .field("transfer", &self.transfer)
            .field("paths", &self.paths)
            .field("pinned", &self.access.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShelfTransfer {
    pub id: TransferId,
    pub offer: OfferId,
    pub peer: MachineId,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub roots: Vec<u32>,
    pub bytes: u64,
    pub total_bytes: u64,
    pub paths: Vec<PathBuf>,
    pub received: Option<ReceivedSelection>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Recipient {
    pub id: MachineId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MachineName {
    pub id: MachineId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShelfSnapshot {
    pub self_id: MachineId,
    pub enabled: bool,
    pub recipients: Vec<Recipient>,
    pub names: Vec<MachineName>,
    pub offers: Vec<ShelfOffer>,
    pub transfers: Vec<ShelfTransfer>,
    pub error: Option<String>,
}

impl ShelfSnapshot {
    pub fn name_of(&self, id: &MachineId) -> String {
        self.names
            .iter()
            .find(|n| &n.id == id)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| id.0.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceGesture {
    NativeDrop,
    OpenPanel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOutcome {
    Copied,
    Cancelled,
}

pub struct SourceLease {
    release: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl SourceLease {
    pub fn new(release: impl FnOnce() + Send + 'static) -> Arc<Self> {
        Arc::new(Self { release: Mutex::new(Some(Box::new(release))) })
    }

    pub fn none() -> Arc<Self> {
        Arc::new(Self { release: Mutex::new(None) })
    }

    pub fn chain(first: Arc<Self>, second: Arc<Self>) -> Arc<Self> {
        Self::new(move || {
            drop(first);
            drop(second);
        })
    }
}

impl Drop for SourceLease {
    fn drop(&mut self) {
        if let Some(release) = self.release.get_mut().ok().and_then(|r| r.take()) {
            release();
        }
    }
}

impl fmt::Debug for SourceLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceLease")
    }
}

pub struct PromiseSource {
    pub path: PathBuf,
    pub access: Option<Access>,
}

impl fmt::Debug for PromiseSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromiseSource").field("path", &self.path).field("pinned", &self.access.is_some()).finish()
    }
}

pub type PromiseResult = Result<PromiseSource, String>;

#[derive(Clone, Default)]
pub struct PromiseCancellation {
    state: Arc<AtomicU8>,
}

impl PromiseCancellation {
    pub fn cancel(&self) -> bool {
        matches!(self.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire), Ok(_) | Err(1))
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == 1
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn begin_publication(&self) -> Result<(), String> {
        self.state.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|state| if state == 1 {
                "promised file transfer was cancelled".into()
            } else {
                "promised file publication was already claimed".into()
            })
    }
}

struct PromiseWriterInner {
    reply: mpsc::SyncSender<PromiseResult>,
    completed: AtomicBool,
    cancellation: PromiseCancellation,
}

#[derive(Clone)]
pub struct PromiseWriter {
    inner: Arc<PromiseWriterInner>,
}

impl PromiseWriter {
    pub fn channel() -> (Self, mpsc::Receiver<PromiseResult>) {
        let (reply, rx) = mpsc::sync_channel(1);
        let inner = Arc::new(PromiseWriterInner {
            reply,
            completed: AtomicBool::new(false),
            cancellation: PromiseCancellation::default(),
        });
        (Self { inner }, rx)
    }

    pub fn complete(&self, result: PromiseResult) -> bool {
        if self.inner.completed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner.reply.try_send(result).is_ok()
    }

    pub fn cancel(&self) {
        if self.inner.cancellation.cancel() {
            self.complete(Err("promised file transfer was cancelled".into()));
        }
    }

    pub fn cancellation(&self) -> PromiseCancellation {
        self.inner.cancellation.clone()
    }

    pub fn is_completed(&self) -> bool {
        self.inner.completed.load(Ordering::Acquire)
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancellation.is_cancelled()
    }
}

impl Drop for PromiseWriterInner {
    fn drop(&mut self) {
        if !self.completed.load(Ordering::Acquire) {
            let _ = self
                .reply
                .try_send(Err("promise writer dropped before the transfer completed".into()));
        }
    }
}

impl fmt::Debug for PromiseWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromiseWriter")
            .field("completed", &self.is_completed())
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum FileEvent {
    SourceSelected {
        paths: Vec<PathBuf>,
        recipient: MachineId,
        gesture: SourceGesture,
        lease: Arc<SourceLease>,
    },
    ClipboardInvalidated {
        generation: u64,
    },
    ClipboardChanged {
        generation: u64,
        mimes: Vec<String>,
        inline_text: Option<String>,
    },
    ClipboardFiles {
        paths: Vec<PathBuf>,
        generation: u64,
        lease: Arc<SourceLease>,
    },
    Receive {
        offer: OfferId,
        clipboard_generation: u64,
        intent: ReceiveIntent,
    },
    Save {
        offer: OfferId,
        directory: PathBuf,
        lease: Arc<SourceLease>,
    },
    Dismiss {
        offer: OfferId,
    },
    Cancel {
        transfer: TransferId,
    },
    Retry {
        transfer: TransferId,
    },
    ClearReceived {
        transfer: TransferId,
    },
    Republish {
        transfer: TransferId,
        clipboard_generation: u64,
        intent: ReceiveIntent,
    },
    DragStarted {
        attempt: DragAttempt,
        offer: OfferId,
        roots: Vec<u32>,
    },
    PromiseRequested {
        attempt: DragAttempt,
        root: u32,
        destination: PathBuf,
        writer: PromiseWriter,
    },
    PromiseFinished {
        attempt: DragAttempt,
        root: u32,
        result: Result<PathBuf, String>,
    },
    DragEnded {
        attempt: DragAttempt,
        outcome: DragOutcome,
    },
    DragRetired {
        attempt: DragAttempt,
    },
    Overflow {
        dropped: u64,
    },
}

pub trait FileShelf: Send + Sync {
    fn sync(&self, snapshot: ShelfSnapshot);
    fn set_visible(&self, visible: bool);
    fn publish_clipboard_files(
        &self,
        selection: ReceivedSelection,
        generation: u64,
        intent: ReceiveIntent,
    ) -> crate::Result<u64>;
    fn reveal(&self, paths: Vec<PathBuf>);
}

pub struct FileAdapter {
    pub shelf: Arc<dyn FileShelf>,
    pub events: tokio::sync::mpsc::Receiver<FileEvent>,
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn summarize_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    match names.as_slice() {
        [] => String::from("Empty selection"),
        [single] => (*single).to_string(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
    }
}

pub fn summarize_roots(offer: &ShelfOffer) -> String {
    summarize_names(offer.roots().map(|r| r.name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(roots: &[(&str, EntryKind)]) -> ShelfOffer {
        ShelfOffer {
            id: OfferId([1; 16]),
            owner: MachineId("a".into()),
            recipient: MachineId("b".into()),
            entries: roots
                .iter()
                .enumerate()
                .map(|(i, (name, kind))| ShelfEntry {
                    id: i as u32,
                    parent: None,
                    name: (*name).into(),
                    kind: kind.clone(),
                })
                .collect(),
            total_bytes: 0,
            expires_unix_ms: 0,
            state: OfferState::Available,
        }
    }

    #[test]
    fn promise_writer_completes_exactly_once() {
        let (writer, rx) = PromiseWriter::channel();
        assert!(writer.complete(Ok(PromiseSource { path: PathBuf::from("/tmp/a"), access: None })));
        assert!(!writer.complete(Err("again".into())));
        assert_eq!(rx.recv().unwrap().unwrap().path, PathBuf::from("/tmp/a"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dropped_promise_writer_reports_an_error() {
        let (writer, rx) = PromiseWriter::channel();
        let clone = writer.clone();
        drop(writer);
        assert!(rx.try_recv().is_err());
        drop(clone);
        assert!(rx.recv().unwrap().is_err());
    }

    #[test]
    fn cancel_marks_the_writer_and_answers_once() {
        let (writer, rx) = PromiseWriter::channel();
        let probe = writer.clone();
        writer.cancel();
        assert!(probe.is_cancelled());
        assert!(rx.recv().unwrap().is_err());
        assert!(!probe.complete(Ok(PromiseSource { path: PathBuf::from("/tmp/a"), access: None })));
    }

    #[test]
    fn cancellation_state_does_not_keep_abandoned_reply_senders_alive() {
        let mut workers = Vec::new();
        for _ in 0..4 {
            let (writer, reply) = PromiseWriter::channel();
            let cancellation = writer.cancellation();
            let parent = writer.clone();
            drop(writer);
            workers.push(std::thread::spawn(move || {
                let result = reply.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
                assert!(result.unwrap_err().contains("dropped"));
                assert!(!cancellation.is_cancelled());
            }));
            drop(parent);
        }
        for worker in workers { worker.join().unwrap(); }
    }

    #[test]
    fn cancellation_and_publication_have_one_winner() {
        for _ in 0..64 {
            let (writer, _) = PromiseWriter::channel();
            let gate = writer.cancellation();
            let start = Arc::new(std::sync::Barrier::new(2));
            let other = start.clone();
            let worker = std::thread::spawn(move || {
                other.wait();
                writer.cancel();
            });
            start.wait();
            let publication = gate.begin_publication().is_ok();
            worker.join().unwrap();
            assert_ne!(publication, gate.is_cancelled());
            assert!(gate.begin_publication().is_err());
        }
    }

    #[test]
    fn source_lease_releases_once_when_last_clone_drops() {
        let released = Arc::new(AtomicBool::new(false));
        let flag = released.clone();
        let lease = SourceLease::new(move || flag.store(true, Ordering::SeqCst));
        let clone = lease.clone();
        drop(lease);
        assert!(!released.load(Ordering::SeqCst));
        drop(clone);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn chained_leases_release_both_parts() {
        let a = Arc::new(AtomicBool::new(false));
        let b = Arc::new(AtomicBool::new(false));
        let (fa, fb) = (a.clone(), b.clone());
        let chained = SourceLease::chain(
            SourceLease::new(move || fa.store(true, Ordering::SeqCst)),
            SourceLease::new(move || fb.store(true, Ordering::SeqCst)),
        );
        drop(chained);
        assert!(a.load(Ordering::SeqCst) && b.load(Ordering::SeqCst));
    }

    #[test]
    fn received_selection_equality_is_by_pin_identity() {
        let pin: Access = Arc::new(());
        let a = ReceivedSelection::new(TransferId([1; 16]), vec![PathBuf::from("/x")], Some(pin.clone()));
        let b = ReceivedSelection::new(TransferId([1; 16]), vec![PathBuf::from("/x")], Some(pin));
        let c = ReceivedSelection::new(TransferId([1; 16]), vec![PathBuf::from("/x")], Some(Arc::new(())));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn directory_roots_are_detected() {
        assert!(!offer(&[("a.txt", EntryKind::File { size: 1 })]).has_directory_root());
        assert!(offer(&[("a.txt", EntryKind::File { size: 1 }), ("dir", EntryKind::Directory)])
            .has_directory_root());
    }

    #[test]
    fn root_summary_and_byte_formatting() {
        assert_eq!(summarize_roots(&offer(&[("a.txt", EntryKind::File { size: 1 })])), "a.txt");
        assert_eq!(
            summarize_roots(&offer(&[
                ("a.txt", EntryKind::File { size: 1 }),
                ("b.txt", EntryKind::File { size: 1 }),
                ("c", EntryKind::Directory)
            ])),
            "a.txt and 2 more"
        );
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(250_000_000), "250 MB");
    }
}
