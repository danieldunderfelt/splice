mod manifest;
mod clipboard;
mod offers;
mod service;
mod storage;
mod transport;

use serde::{Deserialize, Serialize};
pub use splice_proto::files::{
    EntryId, EntryKind, FileOfferId, FileTimestamp, Manifest, ManifestEntry, NativeDragId,
    OfferOrigin, TransferId,
};
use splice_proto::MachineId;
use std::{collections::BTreeMap, net::IpAddr, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiveDestination {
    Directory(PathBuf),
    Cache,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FileCommand {
    Offer {
        paths: Vec<PathBuf>,
        recipient: MachineId,
        origin: OfferOrigin,
    },
    Receive {
        offer: FileOfferId,
        destination: ReceiveDestination,
    },
    ClearReceived {
        transfer: TransferId,
    },
    Revoke {
        offer: FileOfferId,
    },
    Cancel {
        transfer: TransferId,
    },
    Retry {
        transfer: TransferId,
    },
    PrepareDrag {
        offer: FileOfferId,
        entries: Vec<EntryId>,
    },
    DropDrag {
        drag: NativeDragId,
    },
    CommitDrag {
        drag: NativeDragId,
        destination: ReceiveDestination,
    },
    CancelDrag {
        drag: NativeDragId,
    },
    ClipboardChanged {
        generation: u64,
    },
    SetEnabled(bool),
    CaptureClipboard {
        paths: Vec<PathBuf>,
        generation: u64,
    },
    RouteClipboard {
        recipient: MachineId,
        generation: u64,
    },
    ClearClipboard,
    CancelDrags,
    CancelPeerDrags {
        peer: MachineId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileReply {
    Offered(FileOfferId),
    Receiving(TransferId),
    DragPrepared(NativeDragId),
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OfferState {
    Available,
    Revoked,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferRecord {
    pub id: FileOfferId,
    pub owner: MachineId,
    pub recipient: MachineId,
    pub origin: OfferOrigin,
    pub manifest: Manifest,
    pub expires_unix_ms: u64,
    pub state: OfferState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferDirection {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Cancelled | Self::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRecord {
    pub id: TransferId,
    pub offer: FileOfferId,
    pub peer: MachineId,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub bytes: u64,
    pub total_bytes: u64,
    pub paths: Vec<PathBuf>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub recovery_errors: Vec<RecoveryIssue>,
    pub recovery_error_count: usize,
    pub offers: Vec<OfferRecord>,
    pub transfers: Vec<TransferRecord>,
    pub error: Option<String>,
    pub enabled: bool,
    pub payload_bytes_sent: u64,
    pub payload_bytes_received: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedFiles {
    pub transfer: TransferId,
    pub paths: Vec<PathBuf>,
}

#[derive(Clone)]
pub enum SelectedRoot {
    Path(PathBuf),
    Open {
        name: String,
        file: Arc<std::fs::File>,
    },
}

#[derive(Clone)]
pub struct LocalSelection {
    pub roots: Vec<SelectedRoot>,
    pub access: Option<Arc<dyn Send + Sync>>,
}

impl LocalSelection {
    pub fn paths(paths: Vec<PathBuf>) -> Self {
        Self {
            roots: paths.into_iter().map(SelectedRoot::Path).collect(),
            access: None,
        }
    }

    /// Builds a selection from dropped paths. A path that came with an open file is a
    /// portal-granted document and is read through that file, never by path.
    pub fn from_descriptors(
        items: Vec<(PathBuf, Option<Arc<std::fs::File>>)>,
    ) -> Result<Self, String> {
        if items.is_empty() {
            return Err("the selection contains no local files".into());
        }
        if items.len() > splice_proto::files::MAX_ROOTS {
            return Err(format!(
                "the selection has more than {} items",
                splice_proto::files::MAX_ROOTS
            ));
        }
        let mut roots = Vec::with_capacity(items.len());
        for (path, file) in items {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| splice_proto::files::valid_name(name))
                .ok_or_else(|| format!("{} has an unsupported name", path.display()))?
                .to_owned();
            let kind = match &file {
                Some(file) => file
                    .metadata()
                    .map_err(|error| format!("{name}: {error}"))?
                    .file_type(),
                None => {
                    if !path.is_absolute() {
                        return Err(format!("{} is not an absolute path", path.display()));
                    }
                    std::fs::symlink_metadata(&path)
                        .map_err(|error| format!("{}: {error}", path.display()))?
                        .file_type()
                }
            };
            if !kind.is_file() && !kind.is_dir() {
                return Err(format!("{} is not a regular file or directory", path.display()));
            }
            roots.push(match file {
                Some(file) => SelectedRoot::Open { name, file },
                None => SelectedRoot::Path(path),
            });
        }
        Ok(LocalSelection { roots, access: None })
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.roots.is_empty() && self.roots.len() <= splice_proto::files::MAX_ROOTS,
            "invalid selection root count"
        );
        for root in &self.roots {
            match root {
                SelectedRoot::Path(path) => anyhow::ensure!(
                    path.is_absolute() && path.as_os_str().len() <= 4096,
                    "invalid local source path"
                ),
                SelectedRoot::Open { name, .. } => anyhow::ensure!(
                    splice_proto::files::valid_name(name),
                    "invalid selected descriptor name"
                ),
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ReceivedLease {
    files: ReceivedFiles,
    manifest: Arc<Manifest>,
    _pin: Arc<()>,
}

impl ReceivedLease {
    pub fn files(&self) -> &ReceivedFiles {
        &self.files
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferSummary {
    pub id: FileOfferId,
    pub owner: MachineId,
    pub recipient: MachineId,
    pub origin: OfferOrigin,
    pub state: OfferState,
    pub expires_unix_ms: u64,
    pub roots: Vec<ManifestEntry>,
    pub root_count: usize,
    pub entry_count: usize,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferSummary {
    pub id: TransferId,
    pub offer: FileOfferId,
    pub peer: MachineId,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub bytes: u64,
    pub total_bytes: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSummary {
    pub recovery_errors: Vec<RecoveryIssue>,
    pub recovery_error_count: usize,
    pub offers: Vec<OfferSummary>,
    pub transfers: Vec<TransferSummary>,
    pub error: Option<String>,
    pub enabled: bool,
    pub payload_bytes_sent: u64,
    pub payload_bytes_received: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryIssue {
    pub transfer: Option<TransferId>,
    pub message: String,
}

fn bounded_error(error: &str, limit: usize) -> String {
    let mut end = error.len().min(limit);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
}

fn summary_error(error: &Option<String>) -> Option<String> {
    error.as_ref().map(|error| bounded_error(error, 96))
}

impl From<&FileState> for FileSummary {
    fn from(state: &FileState) -> Self {
        Self {
            recovery_errors: state
                .recovery_errors
                .iter()
                .take(32)
                .map(|issue| RecoveryIssue {
                    transfer: issue.transfer,
                    message: bounded_error(&issue.message, 96),
                })
                .collect(),
            recovery_error_count: state.recovery_error_count,
            enabled: state.enabled,
            error: summary_error(&state.error),
            payload_bytes_sent: state.payload_bytes_sent,
            payload_bytes_received: state.payload_bytes_received,
            offers: state
                .offers
                .iter()
                .map(|o| OfferSummary {
                    id: o.id,
                    owner: o.owner.clone(),
                    recipient: o.recipient.clone(),
                    origin: o.origin.clone(),
                    state: o.state.clone(),
                    expires_unix_ms: o.expires_unix_ms,
                    roots: o
                        .manifest
                        .entries
                        .iter()
                        .filter(|e| e.parent.is_none())
                        .take(8)
                        .cloned()
                        .collect(),
                    root_count: o
                        .manifest
                        .entries
                        .iter()
                        .filter(|e| e.parent.is_none())
                        .count(),
                    entry_count: o.manifest.entries.len(),
                    total_bytes: o.manifest.total_bytes,
                })
                .collect(),
            transfers: state
                .transfers
                .iter()
                .map(|t| TransferSummary {
                    id: t.id,
                    offer: t.offer,
                    peer: t.peer.clone(),
                    direction: t.direction,
                    state: t.state,
                    bytes: t.bytes,
                    total_bytes: t.total_bytes,
                    error: summary_error(&t.error),
                })
                .collect(),
        }
    }
}

#[derive(Clone)]
pub struct FileHandle {
    clipboard: clipboard::Sender,
    commands: mpsc::Sender<Request>,
    state: watch::Receiver<FileState>,
    summary: watch::Receiver<FileSummary>,
}

impl FileHandle {
    pub fn summary(&self) -> watch::Receiver<FileSummary> {
        self.summary.clone()
    }

    pub async fn offer_local(
        &self,
        selection: LocalSelection,
        recipient: MachineId,
        origin: OfferOrigin,
    ) -> anyhow::Result<FileOfferId> {
        selection.validate()?;
        let (tx, rx) = oneshot::channel();
        self.commands
            .try_send(Request::Command {
                command: FileCommand::Offer {
                    paths: vec![],
                    recipient,
                    origin,
                },
                selection: Some(selection),
                reply: Some(tx),
            })
            .map_err(|_| anyhow::anyhow!("file service queue unavailable"))?;
        match rx
            .await
            .map_err(|_| anyhow::anyhow!("file service stopped"))?
            .map_err(anyhow::Error::msg)?
        {
            FileReply::Offered(id) => Ok(id),
            _ => anyhow::bail!("unexpected offer reply"),
        }
    }

    pub(crate) fn clipboard_epoch(&self) -> u64 { self.clipboard.epoch() }

    pub(crate) fn capture_clipboard(
        &self,
        selection: LocalSelection,
        generation: u64,
        epoch: u64,
    ) -> anyhow::Result<()> {
        selection.validate()?;
        self.clipboard.send(clipboard::Update::Capture { selection, generation, epoch })
    }

    pub(crate) fn clear_clipboard(&self) -> anyhow::Result<()> {
        self.clipboard.send(clipboard::Update::Clear)
    }

    pub async fn retain_received(&self, transfer: TransferId) -> anyhow::Result<ReceivedLease> {
        let (reply, rx) = oneshot::channel();
        self.commands
            .try_send(Request::Retain { transfer, reply })
            .map_err(|_| anyhow::anyhow!("file service queue unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("file service stopped"))?
            .map_err(anyhow::Error::msg)
    }

    pub fn state(&self) -> watch::Receiver<FileState> {
        self.state.clone()
    }

    pub fn send(&self, command: FileCommand) -> anyhow::Result<()> {
        self.commands
            .try_send(Request::Command {
                selection: None,
                command,
                reply: None,
            })
            .map_err(|e| anyhow::anyhow!("file service queue unavailable: {e}"))
    }

    pub async fn request(&self, command: FileCommand) -> anyhow::Result<FileReply> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .try_send(Request::Command {
                selection: None,
                command,
                reply: Some(tx),
            })
            .map_err(|e| anyhow::anyhow!("file service queue unavailable: {e}"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("file service stopped"))?
            .map_err(anyhow::Error::msg)
    }

    pub async fn wait(&self, transfer: TransferId) -> anyhow::Result<ReceivedFiles> {
        let mut state = self.state();
        loop {
            {
                let current = state.borrow_and_update();
                if let Some(record) = current.transfers.iter().find(|r| r.id == transfer) {
                    match record.state {
                        TransferState::Ready => {
                            return Ok(ReceivedFiles {
                                transfer,
                                paths: record.paths.clone(),
                            })
                        }
                        TransferState::Failed | TransferState::Cancelled => anyhow::bail!(
                            "{}",
                            record.error.as_deref().unwrap_or("transfer cancelled")
                        ),
                        _ => {}
                    }
                } else {
                    anyhow::bail!("unknown transfer");
                }
            }
            state
                .changed()
                .await
                .map_err(|_| anyhow::anyhow!("file service stopped"))?;
        }
    }
}

enum Request {
    Command {
        command: FileCommand,
        selection: Option<LocalSelection>,
        reply: Option<oneshot::Sender<Result<FileReply, String>>>,
    },
    Retain {
        transfer: TransferId,
        reply: oneshot::Sender<Result<ReceivedLease, String>>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Policy {
    pub epochs: BTreeMap<MachineId, u64>,
    pub peers: BTreeMap<MachineId, (u64, IpAddr)>,
    pub enabled: bool,
    pub clipboard_enabled: bool,
}

pub(crate) type WireSender = mpsc::Sender<(MachineId, u64, splice_proto::files::FileMessage)>;

pub(crate) struct Bridge {
    clipboard: Option<clipboard::Receiver>,
    pub handle: FileHandle,
    pub policy: watch::Sender<Policy>,
    wire: WireSender,
    commands: Option<mpsc::Receiver<Request>>,
    incoming: Option<mpsc::Receiver<(MachineId, u64, splice_proto::files::FileMessage)>>,
    state: watch::Sender<FileState>,
    summary: watch::Sender<FileSummary>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Bridge {
    pub fn new() -> Self {
        let (commands, rx) = mpsc::channel(32);
        let (clipboard_tx, clipboard_rx) = clipboard::channel();
        let (wire, incoming) = mpsc::channel(32);
        let (state, state_rx) = watch::channel(FileState::default());
        let (summary, summary_rx) = watch::channel(FileSummary::default());
        let (policy, _) = watch::channel(Policy::default());
        Self {
            clipboard: Some(clipboard_rx),
            handle: FileHandle {
                clipboard: clipboard_tx,
                commands,
                state: state_rx,
                summary: summary_rx,
            },
            policy,
            wire,
            commands: Some(rx),
            incoming: Some(incoming),
            state,
            summary,
            task: None,
        }
    }

    pub async fn start(
        &mut self,
        id: MachineId,
        net: crate::net::NetControl,
        ts: Arc<dyn crate::net::TsApi>,
        data_dir: PathBuf,
    ) {
        let Some(commands) = self.commands.take() else {
            return;
        };
        let Some(incoming) = self.incoming.take() else {
            return;
        };
        let clipboard = self.clipboard.take().expect("clipboard receiver belongs to file service");
        let policy = self.policy.subscribe();
        let state = self.state.clone();
        let summary = self.summary.clone();
        let (ready, initialized) = oneshot::channel();
        let wire = self.wire.clone();
        self.task = Some(tokio::spawn(async move {
            let control = net.clone();
            if let Err(error) = service::run(
                id,
                net,
                ts,
                data_dir,
                commands,
                incoming,
                clipboard,
                policy,
                state.clone(),
                summary.clone(),
                wire,
                ready,
            )
            .await
            {
                state.send_modify(|s| {
                    s.enabled = false;
                    let message = format!("file service: {error:#}");
                    s.error = Some(message.clone());
                    for transfer in &mut s.transfers {
                        if !transfer.state.terminal() {
                            transfer.state = TransferState::Failed;
                            transfer.error = Some(message.clone());
                        }
                    }
                });
            }
            summary.send_replace(FileSummary::from(&*state.borrow()));
            control.file_receiver(None);
        }));
        let _ = initialized.await;
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.policy.send_replace(Policy::default());
    }
}

fn random<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random identifier: {e}"))?;
    anyhow::ensure!(bytes != [0; N], "random identifier is zero");
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn now() -> u64 {
    crate::diagnostics::unix_ms()
}

#[derive(Clone, Default)]
struct Cancellation(Arc<std::sync::atomic::AtomicBool>);
impl Cancellation {
    fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.0.load(std::sync::atomic::Ordering::Acquire),
            "transfer cancelled"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileClipboardRef {
    pub stamp: splice_proto::Stamp,
    pub generation: u64,
}

pub fn is_file_mime(mime: &str) -> bool {
    crate::clipboard::is_file_reference_mime(mime)
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    #[test]
    fn maximal_compact_file_summary_fits_ipc_budget() {
        let mut state = FileState::default();
        for n in 0..16 {
            state.offers.push(OfferRecord {
                id: FileOfferId([n; 16]),
                owner: MachineId("\u{1}".repeat(256)),
                recipient: MachineId("\u{2}".repeat(256)),
                origin: OfferOrigin::Selection,
                state: OfferState::Available,
                expires_unix_ms: u64::MAX,
                manifest: Manifest {
                    generation: [255; 16],
                    total_bytes: u64::MAX,
                    entries: (0..128)
                        .map(|id| ManifestEntry {
                            mode: 0o700,
                            modified: splice_proto::files::FileTimestamp {
                                seconds: 1_700_000_000,
                                nanos: 0,
                            },
                            id: EntryId(id),
                            parent: None,
                            name: "\u{3}".repeat(255),
                            kind: EntryKind::File { size: u64::MAX },
                        })
                        .collect(),
                },
            });
        }
        for n in 0..256 {
            state.transfers.push(TransferRecord {
                id: TransferId([n as u8; 16]),
                offer: FileOfferId([255; 16]),
                peer: MachineId("\u{1}".repeat(256)),
                direction: TransferDirection::Receive,
                state: TransferState::Cancelled,
                bytes: u64::MAX,
                total_bytes: u64::MAX,
                paths: vec![PathBuf::from("secret-local-path"); 128],
                error: Some("\u{2}".repeat(1024)),
            });
        }
        state.recovery_errors = (0..32)
            .map(|_| RecoveryIssue {
                transfer: Some(TransferId([255; 16])),
                message: "\u{1}".repeat(1024),
            })
            .collect();
        state.recovery_error_count = 32;
        let summary = FileSummary::from(&state);
        let encoded = serde_json::to_vec(&summary).unwrap();
        assert!(encoded.len() < 900 * 1024, "{}", encoded.len());
        assert!(!String::from_utf8(encoded)
            .unwrap()
            .contains("secret-local-path"));
        assert_eq!(summary.offers[0].root_count, 128);
        assert_eq!(summary.offers[0].roots.len(), 8);
    }
}
