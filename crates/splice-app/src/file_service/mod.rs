pub mod clipboard;
mod content;
mod ids;
mod journal;
mod shelf;

use clipboard::{EngineFiles, Publication};
use content::{Content, RestoredView};
use shelf::{ClientId, ClientSender, HelperProcess, HelperSlot, Inbound, Listener};
use splice_core::files::{
    FileCommand, FileHandle, FileOfferId, FileReply, FileState, Manifest, NativeDragId,
    OfferOrigin, OfferState, ReceiveDestination, ReceivedLease, TransferDirection, TransferId,
    TransferRecord, TransferState,
};
use splice_core::ui_state::UiConnection;
use splice_core::{EngineHandle, UiState};
use splice_files::ipc::{
    HelperToService, OfferDesc, OfferStateDesc, PeerDesc, ReceiptDesc, ReceiptStateDesc,
    ServiceToHelper, PROTOCOL_VERSION,
};
use splice_files::mount::{Mount, MountConfig};
use splice_platform::files::{ViewId, ViewState};
use splice_proto::MachineId;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

pub const MAX_ARMED_PER_CLIENT: usize = 8;
pub const MAX_LIVE_VIEWS: usize = 32;
pub const MAX_RECEIPTS: usize = journal::MAX_RECORDS;
pub const MAX_JOBS: usize = 64;
pub const MOUNT_WORKERS: usize = 4;
pub const MOUNT_QUEUE: usize = 64;
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
pub const EVENT_QUEUE: usize = 256;
pub const VIEWS_JOURNAL: &str = "linux-views.json";
pub const DROPPED_VIEW_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone)]
pub struct Bridge {
    events: mpsc::UnboundedSender<Event>,
    open_pending: Arc<AtomicBool>,
    status: watch::Receiver<Option<String>>,
}

enum Event {
    Attach(EngineHandle, EngineFiles),
    Detach,
    OpenShelf,
    Shutdown(oneshot::Sender<()>),
}

impl Bridge {
    pub fn start() -> Bridge {
        let (events, rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(None);
        let open_pending = Arc::new(AtomicBool::new(false));
        tokio::spawn(run(rx, status_tx, open_pending.clone()));
        Bridge {
            events,
            open_pending,
            status,
        }
    }

    pub fn status(&self) -> watch::Receiver<Option<String>> {
        self.status.clone()
    }

    pub fn attach(&self, engine: EngineHandle, files: EngineFiles) {
        let _ = self.events.send(Event::Attach(engine, files));
    }

    pub fn detach(&self) {
        let _ = self.events.send(Event::Detach);
    }

    pub fn open_shelf(&self) {
        if !self.open_pending.swap(true, Ordering::AcqRel) {
            let _ = self.events.send(Event::OpenShelf);
        }
    }

    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.events.send(Event::Shutdown(tx)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(3), rx).await;
        }
    }
}

struct Attached {
    files: FileHandle,
    clip: EngineFiles,
    files_state: watch::Receiver<FileState>,
    ui_state: watch::Receiver<UiState>,
    notices: watch::Receiver<Option<String>>,
    portal: bool,
    ready: bool,
}

struct Client {
    sender: ClientSender,
    ready: bool,
    armed: HashMap<FileOfferId, ViewId>,
    armed_receipts: HashMap<String, ViewId>,
    preparing: HashSet<FileOfferId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Armed,
    Dragging,
    Dropped,
    Restored,
}

struct ViewInfo {
    client: Option<ClientId>,
    offer: FileOfferId,
    phase: Phase,
    receipt: Option<TransferId>,
    armed_at: Instant,
    dropped_at: Option<Instant>,
}

enum ReceiveTarget {
    Clipboard { intent: u64, epoch: u64, origin: String },
    Directory(PathBuf),
}

enum Done {
    Attached {
        incarnation: u64,
        portal: bool,
        leases: Vec<(TransferId, Result<ReceivedLease, String>)>,
    },
    Restored {
        incarnation: u64,
        transfer: TransferId,
        result: Result<ReceivedLease, String>,
    },
    Cleared {
        client: ClientId,
        transfer: TransferId,
        result: Result<(), String>,
    },
    Retried {
        client: ClientId,
        offer: FileOfferId,
        result: Result<TransferId, String>,
    },
    Receiving {
        client: ClientId,
        offer: FileOfferId,
        target: ReceiveTarget,
        result: Result<TransferId, String>,
    },
    Prepared {
        client: ClientId,
        offer: FileOfferId,
        result: Result<(NativeDragId, String, Manifest), String>,
    },
    Offered {
        client: ClientId,
        result: Result<(usize, String), String>,
    },
    Received {
        client: ClientId,
        offer: FileOfferId,
        intent: u64,
        epoch: u64,
        result: Result<(TransferId, ReceivedLease, String, Manifest), String>,
    },
    RetainedForReceipt {
        incarnation: u64,
        client: Option<ClientId>,
        offer: FileOfferId,
        origin: String,
        recovered: bool,
        result: Result<(TransferId, ReceivedLease), String>,
    },
    Published {
        client: ClientId,
        offer: FileOfferId,
        result: Result<Publication, String>,
    },
    Saved {
        client: ClientId,
        dest: PathBuf,
        result: Result<usize, String>,
    },
    Dismissed {
        client: ClientId,
        result: Result<(), String>,
    },
    TransferAction {
        client: ClientId,
        verb: &'static str,
        result: Result<(), String>,
    },
}

struct Runner {
    content: Arc<Content>,
    mount: Option<Mount>,
    mount_error: Option<String>,
    listener: Option<Listener>,
    helper: HelperSlot,
    helper_error: Option<String>,
    startup_error: Option<String>,
    log_dir: Option<PathBuf>,
    inbound_tx: mpsc::Sender<Inbound>,
    clients: HashMap<ClientId, Client>,
    views: HashMap<ViewId, ViewInfo>,
    attached: Option<Attached>,
    incarnation: u64,
    restoring: HashSet<TransferId>,
    next_reconcile: Instant,
    jobs: JoinSet<Done>,
    status: watch::Sender<Option<String>>,
    last_error: Option<String>,
    publication: Option<Publication>,
    receive_intent: Option<u64>,
    intent_seq: u64,
    latest_intent: Arc<AtomicU64>,
    retry_receipts: HashMap<TransferId, ClientId>,
    receives_seen: u64,
    progress: HashMap<FileOfferId, (String, u64, Option<u64>)>,
    sent_progress: HashMap<FileOfferId, (String, u64, Option<u64>)>,
    offers_snapshot: Option<String>,
    receipts_sent: Option<(u64, Option<TransferId>)>,
    peers_snapshot: Option<String>,
    announced_offers: HashSet<FileOfferId>,
}

async fn disabled(
    mut events: mpsc::UnboundedReceiver<Event>,
    status: watch::Sender<Option<String>>,
    open_pending: Arc<AtomicBool>,
    message: String,
) {
    tracing::error!(%message);
    status.send_replace(Some(message.clone()));
    loop {
        match events.recv().await {
            Some(Event::OpenShelf) => {
                open_pending.store(false, Ordering::Release);
                status.send_replace(Some(message.clone()));
            }
            Some(Event::Shutdown(done)) => {
                let _ = done.send(());
                return;
            }
            Some(Event::Attach(..)) | Some(Event::Detach) => {}
            None => return,
        }
    }
}

async fn run(
    mut events: mpsc::UnboundedReceiver<Event>,
    status: watch::Sender<Option<String>>,
    open_pending: Arc<AtomicBool>,
) {
    let data_dir = match splice_core::config::config_dir() {
        Ok(dir) => dir,
        Err(error) => return disabled(
            events,
            status,
            open_pending,
            format!(
                "File sharing is unavailable: retained-file storage cannot be opened: {error:#}"
            ),
        )
        .await,
    };
    let (journal, startup_error) = match journal::Journal::open(data_dir.join(VIEWS_JOURNAL)) {
        Ok(opened) => opened,
        Err(error) => {
            return disabled(
                events,
                status,
                open_pending,
                format!("File sharing is unavailable: {error}"),
            )
            .await
        }
    };
    let content = Content::new(tokio::runtime::Handle::current(), journal);
    let (inbound_tx, mut inbound) = mpsc::channel(EVENT_QUEUE);
    let listener = match Listener::bind(inbound_tx.clone()) {
        Ok(listener) => Some(listener),
        Err(error) => {
            tracing::warn!(%error, "file shelf socket unavailable");
            None
        }
    };
    let (mount, mount_error) = match listener
        .as_ref()
        .and_then(|listener| listener.path().parent().map(|dir| dir.join("mnt")))
    {
        Some(mountpoint) => {
            let source = content.clone();
            match tokio::task::spawn_blocking(move || {
                Mount::spawn(MountConfig {
                    mountpoint,
                    content: source,
                    workers: MOUNT_WORKERS,
                    queue: MOUNT_QUEUE,
                })
            })
            .await
            {
                Ok(Ok(mount)) => {
                    tracing::info!(mountpoint = %mount.mountpoint().display(), "file view mount ready");
                    (Some(mount), None)
                }
                Ok(Err(error)) => (None, Some(format!("File drags are unavailable: {error}"))),
                Err(error) => (None, Some(format!("File drags are unavailable: {error}"))),
            }
        }
        None => (
            None,
            Some(
                "File drags are unavailable: the file shelf socket could not be created"
                    .to_string(),
            ),
        ),
    };
    let helper_error = listener
        .is_none()
        .then(|| "The file shelf cannot start: its socket could not be created".to_string());
    let mut runner = Runner {
        content,
        mount,
        mount_error,
        listener,
        helper: HelperSlot::new(),
        helper_error,
        startup_error,
        log_dir: Some(data_dir),
        inbound_tx,
        clients: HashMap::new(),
        views: HashMap::new(),
        attached: None,
        incarnation: 0,
        restoring: HashSet::new(),
        next_reconcile: Instant::now(),
        jobs: JoinSet::new(),
        status,
        last_error: None,
        publication: None,
        receive_intent: None,
        intent_seq: 0,
        latest_intent: Arc::new(AtomicU64::new(0)),
        retry_receipts: HashMap::new(),
        receives_seen: 0,
        progress: HashMap::new(),
        sent_progress: HashMap::new(),
        offers_snapshot: None,
        receipts_sent: None,
        peers_snapshot: None,
        announced_offers: HashSet::new(),
    };
    for view in runner.content.restored_views() {
        runner.restore_view(view);
    }
    runner.publish_status();
    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Some(Event::Attach(engine, files)) => runner.attach(engine, files),
                    Some(Event::Detach) => runner.detach(),
                    Some(Event::OpenShelf) => {
                        open_pending.store(false, Ordering::Release);
                        runner.open_shelf();
                    }
                    Some(Event::Shutdown(done)) => {
                        runner.shutdown().await;
                        let _ = done.send(());
                        return;
                    }
                    None => {
                        runner.shutdown().await;
                        return;
                    }
                }
            }
            inbound = inbound.recv() => {
                match inbound {
                    Some(Inbound::Connected(id, sender)) => runner.connected(id, sender),
                    Some(Inbound::Message(id, message, fds)) => runner.message(id, message, fds),
                    Some(Inbound::Disconnected(id)) => runner.disconnected(id),
                    Some(Inbound::HelperExited(result)) => runner.helper_exited(result),
                    None => {}
                }
            }
            Some(done) = runner.jobs.join_next(), if !runner.jobs.is_empty() => {
                match done {
                    Ok(done) => runner.done(done),
                    Err(error) => {
                        runner.last_error = Some(format!("A file operation crashed: {error}"));
                        runner.publish_status();
                    }
                }
            }
            changed = attached_changed(runner.attached.as_mut()) => {
                match changed {
                    Changed::Files => runner.sync_files(),
                    Changed::Ui => runner.sync_peers(),
                    Changed::Notices => runner.publish_status(),
                }
            }
            _ = ticker.tick() => runner.tick().await,
        }
    }
}

enum Changed {
    Files,
    Ui,
    Notices,
}

async fn watch_changed<T>(receiver: &mut watch::Receiver<T>) {
    if receiver.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn attached_changed(attached: Option<&mut Attached>) -> Changed {
    let Some(attached) = attached else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = watch_changed(&mut attached.files_state) => Changed::Files,
        _ = watch_changed(&mut attached.ui_state) => Changed::Ui,
        _ = watch_changed(&mut attached.notices) => Changed::Notices,
    }
}

fn describe(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

impl Runner {
    fn publish_status(&self) {
        let notice = self
            .attached
            .as_ref()
            .and_then(|attached| attached.notices.borrow().clone());
        let parts: Vec<&str> = [
            self.mount_error.as_deref(),
            self.helper_error.as_deref(),
            self.startup_error.as_deref(),
            notice.as_deref(),
            self.last_error.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let status = (!parts.is_empty()).then(|| parts.join(" · "));
        self.status.send_replace(status);
    }

    fn restore_view(&mut self, view: RestoredView) {
        let Some(mount) = &self.mount else { return };
        if mount.restore_view(
            view.id,
            ids::native_manifest(view.offer, &view.origin, &view.manifest),
        ) {
            self.views.insert(
                view.id,
                ViewInfo {
                    client: None,
                    offer: view.offer,
                    phase: Phase::Restored,
                    receipt: Some(view.transfer),
                    armed_at: Instant::now(),
                    dropped_at: None,
                },
            );
        }
    }

    fn attach(&mut self, engine: EngineHandle, clip: EngineFiles) {
        let files = engine.files();
        self.incarnation += 1;
        let incarnation = self.incarnation;
        self.attached = Some(Attached {
            files_state: files.state(),
            ui_state: engine.state(),
            notices: clip.notices(),
            files: files.clone(),
            clip: clip.clone(),
            portal: false,
            ready: false,
        });
        self.offers_snapshot = None;
        self.peers_snapshot = None;
        self.sent_progress.clear();
        self.content.set_engine(files.clone());
        let transfers = self.content.unrestored();
        self.restoring = transfers.iter().copied().collect();
        self.jobs.spawn(async move {
            let portal = clipboard::portal_available(&clip).await;
            let mut leases = Vec::with_capacity(transfers.len());
            for transfer in transfers {
                leases.push((
                    transfer,
                    files
                        .retain_received(transfer)
                        .await
                        .map_err(|error| describe(&error)),
                ));
            }
            Done::Attached {
                incarnation,
                portal,
                leases,
            }
        });
        self.sync_files();
        self.sync_peers();
        self.sync_receipts();
        self.publish_status();
    }

    fn transient(&self, view: ViewId) -> bool {
        self.views
            .get(&view)
            .is_some_and(|info| matches!(info.phase, Phase::Armed | Phase::Dragging))
            || !self.content.is_committed(view)
    }

    fn detach(&mut self) {
        self.incarnation += 1;
        self.restoring.clear();
        let views: Vec<ViewId> = self
            .views
            .keys()
            .copied()
            .filter(|view| self.transient(*view))
            .collect();
        for view in views {
            self.retire_view(view, true);
        }
        self.content.detach();
        self.attached = None;
        self.progress.clear();
        self.retry_receipts.clear();
        self.content.receives().clear();
        if let Some(publication) = self.publication.take() {
            tokio::spawn(publication.release());
        }
        for client in self.clients.values().filter(|client| client.ready) {
            client.sender.send(ServiceToHelper::Offers {
                offers: Vec::new(),
                more: false,
            });
            client
                .sender
                .send(ServiceToHelper::Peers { peers: Vec::new() });
        }
        self.offers_snapshot = None;
        self.peers_snapshot = None;
        self.sync_receipts();
        self.publish_status();
    }

    async fn shutdown(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.close();
        }
        self.helper.terminate();
        for client in self.clients.values() {
            client.sender.disconnect();
        }
        self.clients.clear();
        let views: Vec<ViewId> = self
            .views
            .keys()
            .copied()
            .filter(|view| self.transient(*view))
            .collect();
        for view in views {
            self.retire_view(view, false);
        }
        if let Some(publication) = self.publication.take() {
            publication.release().await;
        }
        self.jobs.abort_all();
        self.mount.take();
    }

    fn open_shelf(&mut self) {
        if let Some(client) = self.clients.values().find(|client| client.ready) {
            client.sender.send(ServiceToHelper::Present);
            return;
        }
        if self.listener.is_none() {
            self.publish_status();
            return;
        }
        if let Some(age) = self.helper.running() {
            if age < shelf::CONNECT_GRACE {
                return;
            }
            self.helper_error = Some("The file shelf did not connect and was restarted".into());
        }
        match HelperProcess::spawn(self.log_dir.clone(), self.inbound_tx.clone()) {
            Ok(process) => {
                self.helper.replace(process);
            }
            Err(error) => {
                self.helper_error = Some(format!("Cannot start the file shelf: {error}"));
            }
        }
        self.publish_status();
    }

    fn helper_exited(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {}
            Err(message) => {
                self.helper_error = Some(message);
                self.publish_status();
            }
        }
    }

    fn connected(&mut self, id: ClientId, sender: ClientSender) {
        self.clients.insert(
            id,
            Client {
                sender,
                ready: false,
                armed: HashMap::new(),
                armed_receipts: HashMap::new(),
                preparing: HashSet::new(),
            },
        );
    }

    fn disconnected(&mut self, id: ClientId) {
        self.clients.remove(&id);
        let views: Vec<ViewId> = self
            .views
            .iter()
            .filter(|(_, info)| info.client == Some(id))
            .map(|(view, _)| *view)
            .collect();
        for view in views {
            if self.transient(view) {
                self.retire_view(view, false);
            } else if let Some(info) = self.views.get_mut(&view) {
                info.client = None;
            }
        }
    }

    fn client_status(&mut self, client: ClientId, message: String) {
        if let Some(target) = self.clients.get(&client) {
            target.sender.send(ServiceToHelper::Error {
                message: message.clone(),
            });
        }
        self.last_error = Some(message);
        self.publish_status();
    }

    fn client_note(&mut self, client: ClientId, message: String) {
        if let Some(target) = self.clients.get(&client) {
            target.sender.send(ServiceToHelper::Error { message });
        }
        self.last_error = None;
        self.publish_status();
    }

    fn note_all(&mut self, message: String) {
        for client in self.clients.values().filter(|client| client.ready) {
            client.sender.send(ServiceToHelper::Error {
                message: message.clone(),
            });
        }
        self.last_error = None;
        self.publish_status();
    }

    fn message(
        &mut self,
        client: ClientId,
        message: HelperToService,
        fds: Vec<std::os::unix::io::OwnedFd>,
    ) {
        let Some(state) = self.clients.get_mut(&client) else {
            return;
        };
        if let HelperToService::Hello { version } = &message {
            if *version != PROTOCOL_VERSION {
                state.sender.send(ServiceToHelper::Error {
                    message: format!(
                        "File shelf protocol {version} does not match {PROTOCOL_VERSION}"
                    ),
                });
                state.sender.disconnect();
                return;
            }
            state.ready = true;
            self.helper_error = None;
            self.offers_snapshot = None;
            self.receipts_sent = None;
            self.peers_snapshot = None;
            self.sent_progress.clear();
            self.sync_peers();
            self.sync_files();
            self.sync_receipts();
            self.publish_status();
            return;
        }
        if !state.ready {
            state.sender.disconnect();
            return;
        }
        if !message.is_control() && self.jobs.len() >= MAX_JOBS {
            self.client_status(client, "Too many file operations are waiting".into());
            return;
        }
        match message {
            HelperToService::Hello { .. } => {}
            HelperToService::SourceDrop {
                recipient, paths, ..
            } => self.source_drop(client, recipient, paths, fds),
            HelperToService::CreateView { offer } => {
                self.create_view(client, ids::offer_from_native(offer))
            }
            HelperToService::CreateReceiptView { receipt } => {
                self.create_receipt_view(client, receipt)
            }
            HelperToService::DragStarted { view } => {
                if let Some(info) = self.views.get_mut(&view) {
                    if info.client == Some(client) && info.phase == Phase::Armed {
                        info.phase = Phase::Dragging;
                        let offer = info.offer;
                        let receipt = info.receipt;
                        if let Some(mount) = &self.mount {
                            mount.drag_started(view);
                        }
                        if let Some(state) = self.clients.get_mut(&client) {
                            if state.armed.get(&offer) == Some(&view) {
                                state.armed.remove(&offer);
                            }
                            if let Some(receipt) = receipt {
                                let key = ids::transfer_key(receipt);
                                if state.armed_receipts.get(&key) == Some(&view) {
                                    state.armed_receipts.remove(&key);
                                }
                            }
                        }
                    }
                }
            }
            HelperToService::DropPerformed { view } => self.drop_performed(client, view),
            HelperToService::DragCancelled { view } => {
                if self.views.get(&view).is_some_and(|info| {
                    info.client == Some(client) && info.phase != Phase::Restored
                }) {
                    self.retire_view(view, false);
                }
            }
            HelperToService::DragFinished { view } => {
                let undropped = self.mount.as_ref().and_then(|mount| mount.view_state(view))
                    == Some(ViewState::Offered);
                if undropped
                    && self
                        .views
                        .get(&view)
                        .is_some_and(|info| info.client == Some(client))
                {
                    self.retire_view(view, false);
                }
            }
            HelperToService::ReleaseView { view } => {
                if self
                    .views
                    .get(&view)
                    .is_some_and(|info| info.client == Some(client) && info.phase == Phase::Armed)
                {
                    self.retire_view(view, false);
                }
            }
            HelperToService::ReceiveToClipboard { offer, .. } => {
                self.receive_to_clipboard(client, ids::offer_from_native(offer))
            }
            HelperToService::RepublishReceipt { receipt, .. } => {
                self.republish_receipt(client, receipt)
            }
            HelperToService::SaveTo { offer, dest } => {
                self.save_to(client, ids::offer_from_native(offer), dest)
            }
            HelperToService::ClearReceipt { receipt } => self.clear_receipt(client, receipt),
            HelperToService::ReceiptPaths { receipt } => self.receipt_paths(client, receipt),
            HelperToService::CancelReceive { offer } => {
                self.cancel_receive(client, ids::offer_from_native(offer))
            }
            HelperToService::RetryReceive { offer } => {
                self.retry_receive(client, ids::offer_from_native(offer))
            }
            HelperToService::Dismiss { offer } => {
                self.dismiss(client, ids::offer_from_native(offer))
            }
        }
    }

    fn require_attached(&mut self, client: ClientId) -> Option<FileHandle> {
        match &self.attached {
            Some(attached) => Some(attached.files.clone()),
            None => {
                self.client_status(client, "Splice engine is offline".into());
                None
            }
        }
    }

    fn peer_name(&self, id: &MachineId) -> Option<String> {
        let attached = self.attached.as_ref()?;
        let state = attached.ui_state.borrow();
        state
            .machines
            .iter()
            .find(|machine| {
                &machine.id == id
                    && machine.enabled
                    && matches!(
                        machine.connection,
                        UiConnection::Direct { .. } | UiConnection::Derp { .. }
                    )
            })
            .map(|machine| machine.hostname.clone())
    }

    fn source_drop(
        &mut self,
        client: ClientId,
        recipient: String,
        paths: Vec<PathBuf>,
        fds: Vec<std::os::unix::io::OwnedFd>,
    ) {
        let Some(files) = self.require_attached(client) else {
            return;
        };
        let recipient = MachineId(recipient);
        let Some(name) = self.peer_name(&recipient) else {
            self.client_status(
                client,
                "Choose a connected machine before dropping files".into(),
            );
            return;
        };
        let selection =
            clipboard::selection_with_descriptors(paths, fds).and_then(clipboard::local_selection);
        let selection = match selection {
            Ok(selection) => selection,
            Err(error) => {
                self.client_status(client, format!("Cannot offer this selection: {error}"));
                return;
            }
        };
        let count = selection.roots.len();
        self.jobs.spawn(async move {
            let result = files
                .offer_local(selection, recipient, OfferOrigin::Selection)
                .await
                .map(|_| (count, name))
                .map_err(|error| describe(&error));
            Done::Offered { client, result }
        });
    }

    fn incoming_offer(&self, offer: FileOfferId) -> Option<(String, Manifest)> {
        let attached = self.attached.as_ref()?;
        let self_id = attached.ui_state.borrow().self_id.clone();
        let state = attached.files_state.borrow();
        let record = state.offers.iter().find(|record| {
            record.id == offer
                && record.recipient == self_id
                && record.state == OfferState::Available
        })?;
        let origin = self.machine_name(&record.owner);
        Some((origin, record.manifest.clone()))
    }

    fn machine_name(&self, id: &MachineId) -> String {
        self.attached
            .as_ref()
            .and_then(|attached| {
                attached
                    .ui_state
                    .borrow()
                    .machines
                    .iter()
                    .find(|machine| &machine.id == id)
                    .map(|machine| machine.hostname.clone())
            })
            .unwrap_or_else(|| id.0.clone())
    }

    fn view_failed(&mut self, client: ClientId, offer: FileOfferId, error: String) {
        if let Some(state) = self.clients.get(&client) {
            state.sender.send(ServiceToHelper::ViewFailed {
                view: ViewId(0),
                offer: ids::offer_to_native(offer),
                error: error.clone(),
            });
        }
        self.last_error = Some(error);
        self.publish_status();
    }

    fn active_views(&self) -> usize {
        let prepared: usize = self
            .clients
            .values()
            .map(|state| state.preparing.len())
            .sum();
        let armed = self
            .views
            .iter()
            .filter(|(view, info)| {
                info.phase != Phase::Restored && !self.content.is_committed(**view)
            })
            .count();
        prepared + armed
    }

    fn make_room(&mut self, client: ClientId) -> bool {
        let Some(state) = self.clients.get(&client) else {
            return false;
        };
        if state.armed.len() + state.armed_receipts.len() + state.preparing.len()
            < MAX_ARMED_PER_CLIENT
        {
            return true;
        }
        let oldest = self
            .views
            .iter()
            .filter(|(_, info)| info.client == Some(client) && info.phase == Phase::Armed)
            .min_by_key(|(_, info)| info.armed_at)
            .map(|(view, _)| *view);
        match oldest {
            Some(view) => {
                self.retire_view(view, true);
                true
            }
            None => false,
        }
    }

    fn create_view(&mut self, client: ClientId, offer: FileOfferId) {
        let Some(files) = self.require_attached(client) else {
            return;
        };
        if self.mount.is_none() {
            let error = self
                .mount_error
                .clone()
                .unwrap_or_else(|| "File drags are unavailable".into());
            self.view_failed(client, offer, error);
            return;
        }
        let Some(state) = self.clients.get(&client) else {
            return;
        };
        if let Some(view) = state.armed.get(&offer).copied() {
            let uris = self
                .mount
                .as_ref()
                .map(|mount| mount.view_uris(view))
                .unwrap_or_default();
            state.sender.send(ServiceToHelper::ViewReady {
                view,
                offer: ids::offer_to_native(offer),
                uris,
                portal_key: None,
            });
            return;
        }
        if state.preparing.contains(&offer) {
            return;
        }
        if !self.make_room(client) {
            self.view_failed(
                client,
                offer,
                "Too many drags are in progress; finish some first".into(),
            );
            return;
        }
        if self.active_views() >= MAX_LIVE_VIEWS {
            self.view_failed(client, offer, "Too many file drags are active".into());
            return;
        }
        let Some((origin, manifest)) = self.incoming_offer(offer) else {
            self.view_failed(client, offer, "This offer is no longer available".into());
            return;
        };
        if let Some(state) = self.clients.get_mut(&client) {
            state.preparing.insert(offer);
        }
        self.jobs.spawn(async move {
            let result = match files
                .request(FileCommand::PrepareDrag {
                    offer,
                    entries: Vec::new(),
                })
                .await
            {
                Ok(FileReply::DragPrepared(drag)) => Ok((drag, origin, manifest)),
                Ok(_) => Err("Unexpected reply while preparing the drag".to_string()),
                Err(error) => Err(describe(&error)),
            };
            Done::Prepared {
                client,
                offer,
                result,
            }
        });
    }

    fn receipt_view_failed(&mut self, client: ClientId, receipt: &str, error: String) {
        if let Some(state) = self.clients.get(&client) {
            state.sender.send(ServiceToHelper::ReceiptViewFailed {
                receipt: receipt.to_owned(),
                error: error.clone(),
            });
        }
        self.last_error = Some(error);
        self.publish_status();
    }

    fn receipt_unavailable(&self, transfer: TransferId) -> String {
        match self.content.receipt_state(transfer) {
            None => "This receipt is no longer retained".into(),
            Some(ReceiptStateDesc::Clearing) => "These files are being cleared".into(),
            Some(ReceiptStateDesc::Unavailable) => {
                "The received files are not available right now".into()
            }
            Some(ReceiptStateDesc::Retained) => "The received files are busy".into(),
        }
    }

    fn create_receipt_view(&mut self, client: ClientId, receipt: String) {
        let Some(transfer) = ids::transfer_from_key(&receipt) else {
            self.receipt_view_failed(client, &receipt, "This receipt is not recognized".into());
            return;
        };
        if self.mount.is_none() {
            let error = self
                .mount_error
                .clone()
                .unwrap_or_else(|| "File drags are unavailable".into());
            self.receipt_view_failed(client, &receipt, error);
            return;
        }
        let Some(state) = self.clients.get(&client) else {
            return;
        };
        if let Some(view) = state.armed_receipts.get(&receipt).copied() {
            let uris = self
                .mount
                .as_ref()
                .map(|mount| mount.view_uris(view))
                .unwrap_or_default();
            state.sender.send(ServiceToHelper::ReceiptViewReady {
                receipt,
                view,
                uris,
            });
            return;
        }
        if !self.make_room(client) {
            self.receipt_view_failed(
                client,
                &receipt,
                "Too many drags are in progress; finish some first".into(),
            );
            return;
        }
        if self.active_views() >= MAX_LIVE_VIEWS {
            self.receipt_view_failed(client, &receipt, "Too many file drags are active".into());
            return;
        }
        let Some((offer, origin, manifest)) = self.content.receipt_manifest(transfer) else {
            let error = self.receipt_unavailable(transfer);
            self.receipt_view_failed(client, &receipt, error);
            return;
        };
        let Some(mount) = &self.mount else { return };
        let view = mount.create_view(ids::native_manifest(offer, &origin, &manifest));
        let uris = mount.view_uris(view);
        if let Err(error) = self.content.register_receipt_view(view, transfer) {
            mount.retire_view(view);
            self.receipt_view_failed(client, &receipt, error);
            return;
        }
        self.views.insert(
            view,
            ViewInfo {
                client: Some(client),
                offer,
                phase: Phase::Armed,
                receipt: Some(transfer),
                armed_at: Instant::now(),
                dropped_at: None,
            },
        );
        if let Some(state) = self.clients.get_mut(&client) {
            state.armed_receipts.insert(receipt.clone(), view);
            state.sender.send(ServiceToHelper::ReceiptViewReady {
                receipt,
                view,
                uris,
            });
        }
    }

    fn drop_performed(&mut self, client: ClientId, view: ViewId) {
        let accepted = self.views.get(&view).is_some_and(|info| {
            info.client == Some(client) && matches!(info.phase, Phase::Armed | Phase::Dragging)
        });
        if !accepted {
            self.client_status(
                client,
                "The dropped files belong to a drag that is no longer active".into(),
            );
            return;
        }
        let receipt = self.views.get(&view).and_then(|info| info.receipt);
        if receipt.is_some() {
            if let Err(error) = self.content.persist_receipt_view(view) {
                self.client_status(
                    client,
                    format!("The drop was refused because it could not be recorded durably: {error}"),
                );
                self.retire_view(view, true);
                return;
            }
        }
        let recorded = self
            .mount
            .as_ref()
            .is_some_and(|mount| mount.drop_performed(view))
            && self.content.dropped(view);
        if !recorded {
            self.client_status(
                client,
                "The drop could not be recorded; the files were not sent".into(),
            );
            self.retire_view(view, true);
            return;
        }
        let Some(info) = self.views.get_mut(&view) else {
            return;
        };
        info.phase = Phase::Dropped;
        info.dropped_at = Some(Instant::now());
        let offer = info.offer;
        if let Some(state) = self.clients.get_mut(&client) {
            if state.armed.get(&offer) == Some(&view) {
                state.armed.remove(&offer);
            }
            if let Some(receipt) = receipt {
                let key = ids::transfer_key(receipt);
                if state.armed_receipts.get(&key) == Some(&view) {
                    state.armed_receipts.remove(&key);
                }
            }
        }
    }

    fn drop_view_info(&mut self, view: ViewId, notify: bool) {
        let Some(info) = self.views.remove(&view) else {
            return;
        };
        if let Some(client) = info.client.and_then(|id| self.clients.get_mut(&id)) {
            if client.armed.get(&info.offer) == Some(&view) {
                client.armed.remove(&info.offer);
            }
            if let Some(receipt) = info.receipt {
                let key = ids::transfer_key(receipt);
                if client.armed_receipts.get(&key) == Some(&view) {
                    client.armed_receipts.remove(&key);
                }
            }
            if notify {
                client.sender.send(ServiceToHelper::RetireView { view });
            }
        }
    }

    fn retire_view(&mut self, view: ViewId, notify: bool) {
        if let Some(mount) = &self.mount {
            if self.content.is_committed(view) {
                mount.retire_view(view);
            } else {
                mount.cancel_view(view);
            }
        }
        self.content.retire(view);
        self.drop_view_info(view, notify);
    }

    fn forget_receipt(&mut self, transfer: TransferId) -> Result<(), String> {
        let (retired, journal) = self.content.forget_receipt(transfer);
        for view in retired {
            if let Some(mount) = &self.mount {
                mount.retire_view(view);
            }
            self.drop_view_info(view, true);
        }
        journal
    }

    fn next_intent(&mut self) -> u64 {
        self.intent_seq += 1;
        self.receive_intent = Some(self.intent_seq);
        self.latest_intent.store(self.intent_seq, Ordering::Release);
        self.intent_seq
    }

    fn receive_to_clipboard(&mut self, client: ClientId, offer: FileOfferId) {
        if self.require_attached(client).is_none() {
            return;
        }
        let Some((origin, _)) = self.incoming_offer(offer) else {
            self.client_status(client, "This offer is no longer available".into());
            return;
        };
        if self.content.receipt_count() >= MAX_RECEIPTS {
            self.client_status(client, format!("The retained-file store is full ({MAX_RECEIPTS} receipts); clear received files before accepting more"));
            return;
        }
        let intent = self.next_intent();
        let epoch = self
            .attached
            .as_ref()
            .map(|attached| attached.clip.epoch())
            .unwrap_or_default();
        self.begin_receive(
            client,
            offer,
            ReceiveDestination::Cache,
            ReceiveTarget::Clipboard {
                intent,
                epoch,
                origin,
            },
        );
    }

    fn begin_receive(
        &mut self,
        client: ClientId,
        offer: FileOfferId,
        destination: ReceiveDestination,
        target: ReceiveTarget,
    ) {
        let Some(files) = self.require_attached(client) else {
            return;
        };
        self.last_error = None;
        self.publish_status();
        self.jobs.spawn(async move {
            let result = match files
                .request(FileCommand::Receive {
                    offer,
                    destination,
                })
                .await
            {
                Ok(FileReply::Receiving(transfer)) => Ok(transfer),
                Ok(_) => Err("Unexpected reply while receiving files".to_string()),
                Err(error) => Err(describe(&error)),
            };
            Done::Receiving {
                client,
                offer,
                target,
                result,
            }
        });
    }

    fn follow_receive(
        &mut self,
        client: ClientId,
        offer: FileOfferId,
        transfer: TransferId,
        target: ReceiveTarget,
    ) {
        let Some(files) = self.require_attached(client) else {
            return;
        };
        self.content.receives().record(offer, transfer);
        self.sync_files();
        match target {
            ReceiveTarget::Clipboard {
                intent,
                epoch,
                origin,
            } => {
                self.jobs.spawn(async move {
                    let result = async {
                        files
                            .wait(transfer)
                            .await
                            .map_err(|error| describe(&error))?;
                        let lease = files
                            .retain_received(transfer)
                            .await
                            .map_err(|error| describe(&error))?;
                        let manifest = lease.manifest().clone();
                        Ok((transfer, lease, origin, manifest))
                    }
                    .await;
                    Done::Received {
                        client,
                        offer,
                        intent,
                        epoch,
                        result,
                    }
                });
            }
            ReceiveTarget::Directory(dest) => {
                self.jobs.spawn(async move {
                    let result = files
                        .wait(transfer)
                        .await
                        .map(|received| received.paths.len())
                        .map_err(|error| describe(&error));
                    Done::Saved {
                        client,
                        dest,
                        result,
                    }
                });
            }
        }
    }

    fn republish_receipt(&mut self, client: ClientId, receipt: String) {
        let (clip, portal) = match &self.attached {
            Some(attached) => (attached.clip.clone(), attached.portal),
            None => {
                self.client_status(client, "Splice engine is offline".into());
                return;
            }
        };
        let Some(transfer) = ids::transfer_from_key(&receipt) else {
            self.client_status(client, "This receipt is not recognized".into());
            return;
        };
        let (Some(lease), Some((offer, _, _))) = (
            self.content.receipt_lease(transfer),
            self.content.receipt_manifest(transfer),
        ) else {
            let error = self.receipt_unavailable(transfer);
            self.client_status(client, error);
            return;
        };
        let intent = self.next_intent();
        let epoch = clip.epoch();
        let latest = self.latest_intent.clone();
        self.last_error = None;
        self.publish_status();
        self.jobs.spawn(async move {
            let result = match clipboard::publish(
                &clip,
                &lease.files().paths,
                epoch,
                portal,
                Some((latest, intent)),
            )
            .await
            {
                Ok(export) => Ok(Publication::new(lease, export, epoch)),
                Err(error) => Err(describe(&error)),
            };
            Done::Published {
                client,
                offer,
                result,
            }
        });
    }

    fn save_to(&mut self, client: ClientId, offer: FileOfferId, dest: PathBuf) {
        if self.require_attached(client).is_none() {
            return;
        }
        if !dest.is_absolute() || !dest.is_dir() {
            self.client_status(client, format!("{} is not a folder", dest.display()));
            return;
        }
        if self.incoming_offer(offer).is_none() {
            self.client_status(client, "This offer is no longer available".into());
            return;
        }
        self.begin_receive(
            client,
            offer,
            ReceiveDestination::Directory(dest.clone()),
            ReceiveTarget::Directory(dest),
        );
    }

    fn receipt_paths(&mut self, client: ClientId, receipt: String) {
        let Some(transfer) = ids::transfer_from_key(&receipt) else {
            self.client_status(client, "This receipt is not recognized".into());
            return;
        };
        let Some(paths) = self.content.receipt_paths(transfer) else {
            let error = self.receipt_unavailable(transfer);
            self.client_status(client, error);
            return;
        };
        let Some(state) = self.clients.get(&client) else {
            return;
        };
        for (chunk, more) in splice_files::ipc::paged(paths, splice_files::ipc::MAX_PACK) {
            state.sender.send(ServiceToHelper::ReceiptPaths {
                receipt: receipt.clone(),
                paths: chunk,
                more,
            });
        }
    }

    fn clear_receipt(&mut self, client: ClientId, receipt: String) {
        let Some(transfer) = ids::transfer_from_key(&receipt) else {
            self.client_status(client, "This receipt is not recognized".into());
            return;
        };
        match self.content.receipt_state(transfer) {
            None => {
                self.client_status(client, "This receipt is no longer retained".into());
                return;
            }
            Some(ReceiptStateDesc::Clearing) => {
                self.client_note(client, "These files are already being cleared".into());
                return;
            }
            Some(_) => {}
        }
        let Some(files) = self.require_attached(client) else {
            return;
        };
        if self
            .publication
            .as_ref()
            .is_some_and(|publication| publication.transfer() == transfer)
        {
            self.client_status(
                client,
                "These files are on the clipboard; copy something else before clearing them".into(),
            );
            return;
        }
        let views = self.content.receipt_views(transfer);
        if let Some(mount) = &self.mount {
            if mount.retire_if_unused(&views).is_err() {
                self.client_status(
                    client,
                    "These files are open in another application; close it before clearing them"
                        .into(),
                );
                return;
            }
        }
        let retired = match self.content.begin_clear(transfer) {
            Ok(retired) => retired,
            Err(error) => {
                self.client_status(client, format!("Cannot clear these files: {error}"));
                return;
            }
        };
        for view in retired {
            self.drop_view_info(view, true);
        }
        self.last_error = None;
        self.publish_status();
        self.sync_receipts();
        self.jobs.spawn(async move {
            let result = files
                .request(FileCommand::ClearReceived { transfer })
                .await
                .map(|_| ())
                .map_err(|error| describe(&error));
            Done::Cleared {
                client,
                transfer,
                result,
            }
        });
    }

    fn restore_lease(&mut self, transfer: TransferId) {
        let Some(attached) = &self.attached else {
            return;
        };
        if !self.restoring.insert(transfer) {
            return;
        }
        let files = attached.files.clone();
        let incarnation = self.incarnation;
        self.jobs.spawn(async move {
            let result = files
                .retain_received(transfer)
                .await
                .map_err(|error| describe(&error));
            Done::Restored {
                incarnation,
                transfer,
                result,
            }
        });
    }

    fn reconcile_receipts(&mut self) {
        let Some(attached) = &self.attached else {
            return;
        };
        if !attached.ready {
            return;
        }
        let mut gone = Vec::new();
        let mut retry = Vec::new();
        {
            let state = attached.files_state.borrow();
            for transfer in self.content.unrestored() {
                match state
                    .transfers
                    .iter()
                    .find(|record| record.id == transfer)
                    .map(|record| record.state)
                {
                    Some(TransferState::Ready) => retry.push(transfer),
                    Some(TransferState::Failed | TransferState::Cancelled) | None => {
                        gone.push(transfer)
                    }
                    Some(_) => {}
                }
            }
        }
        for transfer in gone {
            tracing::info!(transfer = ?transfer, "dropping a retained receipt whose files are no longer stored");
            if let Err(error) = self.forget_receipt(transfer) {
                self.last_error = Some(error);
                self.publish_status();
            }
        }
        for transfer in retry {
            self.restore_lease(transfer);
        }
        self.sync_receipts();
    }

    fn current_receive_id(&self, offer: FileOfferId, states: &[TransferState]) -> Option<TransferId> {
        let attached = self.attached.as_ref()?;
        let state = attached.files_state.borrow();
        current_receive(self.content.receives().current(offer), offer, &state.transfers)
            .filter(|transfer| states.contains(&transfer.state))
            .map(|transfer| transfer.id)
    }

    fn cancel_receive(&mut self, client: ClientId, offer: FileOfferId) {
        let Some(files) = self.require_attached(client) else {
            return;
        };
        let Some(transfer) = self.current_receive_id(
            offer,
            &[
                TransferState::Preparing,
                TransferState::Committed,
                TransferState::Receiving,
                TransferState::Verifying,
            ],
        ) else {
            self.client_status(client, "There is no active receive to cancel".into());
            return;
        };
        self.jobs.spawn(async move {
            let result = files
                .request(FileCommand::Cancel { transfer })
                .await
                .map(|_| ())
                .map_err(|error| describe(&error));
            Done::TransferAction {
                client,
                verb: "cancel",
                result,
            }
        });
    }

    fn retry_receive(&mut self, client: ClientId, offer: FileOfferId) {
        let Some(files) = self.require_attached(client) else {
            return;
        };
        let Some(transfer) =
            self.current_receive_id(offer, &[TransferState::Failed, TransferState::Cancelled])
        else {
            self.client_status(client, "There is no failed receive to retry".into());
            return;
        };
        self.jobs.spawn(async move {
            let result = match files.request(FileCommand::Retry { transfer }).await {
                Ok(FileReply::Receiving(transfer)) => Ok(transfer),
                Ok(_) => Err("Unexpected reply while retrying the receive".to_string()),
                Err(error) => Err(describe(&error)),
            };
            Done::Retried {
                client,
                offer,
                result,
            }
        });
    }

    fn dismiss(&mut self, client: ClientId, offer: FileOfferId) {
        let Some(files) = self.require_attached(client) else {
            return;
        };
        self.jobs.spawn(async move {
            let result = files
                .request(FileCommand::Revoke { offer })
                .await
                .map(|_| ())
                .map_err(|error| describe(&error));
            Done::Dismissed { client, result }
        });
    }

    fn done(&mut self, done: Done) {
        match done {
            Done::Attached { incarnation, portal, leases } => {
                if incarnation != self.incarnation {
                    return;
                }
                if let Some(attached) = &mut self.attached {
                    attached.portal = portal;
                    attached.ready = true;
                }
                for (transfer, result) in leases {
                    self.restoring.remove(&transfer);
                    self.content.install_lease(transfer, result);
                }
                self.reconcile_receipts();
                self.recover_orphan_receipts();
                self.sync_receipts();
            }
            Done::Restored { incarnation, transfer, result } => {
                if incarnation != self.incarnation {
                    return;
                }
                self.restoring.remove(&transfer);
                self.content.install_lease(transfer, result);
                self.sync_receipts();
            }
            Done::Cleared { client, transfer, result } => match result {
                Ok(()) => match self.forget_receipt(transfer) {
                    Ok(()) => self.client_note(client, "Cleared the retained files".into()),
                    Err(error) => self.client_status(client, format!("Cleared the retained files, but the journal could not be updated: {error}")),
                },
                Err(error) => {
                    self.content.end_clear(transfer);
                    for view in self.content.persisted_views(transfer) {
                        self.restore_view(view);
                    }
                    self.restore_lease(transfer);
                    self.client_status(client, format!("Cannot clear these files: {error}; they stay in the shelf"));
                    self.sync_receipts();
                }
            },
            Done::Retried { client, offer, result } => match result {
                Ok(transfer) => {
                    self.retry_receipts.insert(transfer, client);
                    self.content.receives().record(offer, transfer);
                    self.sync_files();
                }
                Err(error) => self.client_status(client, format!("Cannot retry this transfer: {error}")),
            },
            Done::Receiving { client, offer, target, result } => match result {
                Ok(transfer) => self.follow_receive(client, offer, transfer, target),
                Err(error) => {
                    let verb = match target {
                        ReceiveTarget::Clipboard { .. } => "Receiving",
                        ReceiveTarget::Directory(_) => "Saving",
                    };
                    self.client_status(client, format!("{verb} failed: {error}"));
                }
            },
            Done::Prepared { client, offer, result } => {
                if let Some(state) = self.clients.get_mut(&client) {
                    state.preparing.remove(&offer);
                }
                match result {
                    Ok((drag, origin, manifest)) => {
                        let usable = self.clients.get(&client).is_some_and(|state| state.ready) && self.incoming_offer(offer).is_some() && self.mount.is_some();
                        if !usable {
                            if let Some(attached) = &self.attached {
                                let _ = attached.files.send(FileCommand::CancelDrag { drag });
                            }
                            return;
                        }
                        let Some(mount) = &self.mount else { return };
                        let view = mount.create_view(ids::native_manifest(offer, &origin, &manifest));
                        let uris = mount.view_uris(view);
                        self.content.register(view, drag, offer, origin, manifest);
                        self.views.insert(view, ViewInfo { client: Some(client), offer, phase: Phase::Armed, receipt: None, armed_at: Instant::now(), dropped_at: None });
                        if let Some(state) = self.clients.get_mut(&client) {
                            state.armed.insert(offer, view);
                            state.sender.send(ServiceToHelper::ViewReady { view, offer: ids::offer_to_native(offer), uris, portal_key: None });
                        }
                    }
                    Err(error) => self.view_failed(client, offer, format!("Cannot prepare the drag: {error}")),
                }
            }
            Done::Offered { client, result } => match result {
                Ok((count, name)) => self.client_note(client, format!("Offered {count} {} to {name}; pick them up there", if count == 1 { "item" } else { "items" })),
                Err(error) => self.client_status(client, format!("Cannot offer these files: {error}")),
            },
            Done::Received { client, offer, intent, epoch, result } => match result {
                Ok((transfer, lease, origin, manifest)) => {
                    if let Err(error) = self.content.add_receipt(transfer, offer, origin, manifest, lease.clone()) {
                        self.client_status(client, format!("Cannot retain the received files: {error}"));
                        return;
                    }
                    self.sync_receipts();
                    let current_intent = self.receive_intent == Some(intent);
                    let current_epoch = self.attached.as_ref().is_some_and(|attached| attached.clip.epoch() == epoch);
                    if !current_intent || !current_epoch {
                        self.client_note(client, "Clipboard changed while receiving; the received files are in the shelf".into());
                        return;
                    }
                    let Some(attached) = &self.attached else { return };
                    let clip = attached.clip.clone();
                    let portal = attached.portal;
                    let latest = self.latest_intent.clone();
                    self.jobs.spawn(async move {
                        let result = match clipboard::publish(&clip, &lease.files().paths, epoch, portal, Some((latest, intent))).await {
                            Ok(export) => Ok(Publication::new(lease, export, epoch)),
                            Err(error) => Err(describe(&error)),
                        };
                        Done::Published { client, offer, result }
                    });
                }
                Err(error) => self.client_status(client, format!("Receiving failed: {error}")),
            },
            Done::RetainedForReceipt { incarnation, client, offer, origin, recovered, result } => {
                if incarnation != self.incarnation {
                    return;
                }
                match result {
                Ok((transfer, lease)) => match self.content.add_receipt(transfer, offer, origin, lease.manifest().clone(), lease) {
                    Ok(()) => {
                        self.sync_receipts();
                        let message = if recovered {
                            "Recovered received files into the shelf".to_string()
                        } else {
                            "The retried files were received; they are in the shelf".to_string()
                        };
                        match client {
                            Some(client) => self.client_note(client, message),
                            None => self.note_all(message),
                        }
                    }
                    Err(error) => match client {
                        Some(client) => self.client_status(client, format!("Cannot retain the received files: {error}")),
                        None => {
                            self.last_error = Some(format!("Cannot retain the received files: {error}"));
                            self.publish_status();
                        }
                    },
                },
                Err(error) => {
                    if recovered {
                        tracing::info!(%error, "an orphaned receive could not be retained");
                    } else if let Some(client) = client {
                        self.client_status(client, format!("The retried receive could not be retained: {error}"));
                    }
                }
                }
            }
            Done::Published { client, offer, result } => match result {
                Ok(publication) => {
                    let count = publication.paths().len();
                    if let Some(previous) = self.publication.replace(publication) {
                        tokio::spawn(previous.release());
                    }
                    self.sync_receipts();
                    if let Some(state) = self.clients.get(&client) {
                        state.sender.send(ServiceToHelper::Progress { offer: ids::offer_to_native(offer), state: "Received to clipboard".into(), done: count as u64, total: Some(count as u64) });
                    }
                    self.client_note(client, "Files received; paste them in any folder".into());
                }
                Err(error) => self.client_status(client, format!("Cannot publish to the clipboard: {error}; the received files stay in the shelf")),
            },
            Done::Saved { client, dest, result } => match result {
                Ok(count) => self.client_note(client, format!("Saved {count} {} to {}", if count == 1 { "item" } else { "items" }, dest.display())),
                Err(error) => self.client_status(client, format!("Saving failed: {error}")),
            },
            Done::Dismissed { client, result } => {
                if let Err(error) = result {
                    self.client_status(client, format!("Cannot dismiss this offer: {error}"));
                }
            }
            Done::TransferAction { client, verb, result } => {
                if let Err(error) = result {
                    self.client_status(client, format!("Cannot {verb} this transfer: {error}"));
                }
            }
        }
    }

    fn sync_peers(&mut self) {
        let Some(attached) = &self.attached else {
            return;
        };
        let state = attached.ui_state.borrow();
        let peers: Vec<PeerDesc> = state
            .machines
            .iter()
            .filter(|machine| {
                machine.id != state.self_id
                    && machine.enabled
                    && matches!(
                        machine.connection,
                        UiConnection::Direct { .. } | UiConnection::Derp { .. }
                    )
            })
            .map(|machine| PeerDesc {
                id: machine.id.0.clone(),
                name: machine.hostname.clone(),
            })
            .collect();
        drop(state);
        let encoded = serde_json::to_string(&peers).unwrap_or_default();
        if self.peers_snapshot.as_deref() == Some(encoded.as_str()) {
            return;
        }
        self.peers_snapshot = Some(encoded);
        for client in self.clients.values().filter(|client| client.ready) {
            client.sender.send(ServiceToHelper::Peers {
                peers: peers.clone(),
            });
        }
    }

    fn sync_files(&mut self) {
        let Some(attached) = &self.attached else {
            return;
        };
        let files_handle = attached.files.clone();
        let self_id = attached.ui_state.borrow().self_id.clone();
        let state = attached.files_state.borrow().clone();
        let receives = self.content.receives();
        receives.retain(|offer| state.offers.iter().any(|record| record.id == offer));
        self.receives_seen = receives.revision();
        let mut offers = Vec::new();
        let mut available = HashSet::new();
        for record in state
            .offers
            .iter()
            .filter(|record| record.recipient == self_id)
        {
            let current = current_receive(
                self.content.receives().current(record.id),
                record.id,
                &state.transfers,
            );
            let desc_state = match record.state {
                OfferState::Revoked => OfferStateDesc::Revoked,
                OfferState::Expired => OfferStateDesc::Expired,
                OfferState::Available => match current.map(|transfer| transfer.state) {
                    Some(TransferState::Preparing | TransferState::Committed) => {
                        OfferStateDesc::Preparing
                    }
                    Some(TransferState::Receiving | TransferState::Verifying) => {
                        OfferStateDesc::Receiving
                    }
                    Some(TransferState::Ready) => OfferStateDesc::Ready,
                    Some(TransferState::Failed) => OfferStateDesc::Failed,
                    Some(TransferState::Cancelled) | None => OfferStateDesc::Available,
                },
            };
            if record.state == OfferState::Available {
                available.insert(record.id);
                match current {
                    Some(transfer) => {
                        self.progress.insert(
                            record.id,
                            (
                                progress_label(transfer),
                                transfer.bytes,
                                Some(transfer.total_bytes),
                            ),
                        );
                    }
                    None => {
                        self.progress.remove(&record.id);
                    }
                }
            }
            let roots: Vec<String> = record
                .manifest
                .entries
                .iter()
                .filter(|entry| entry.parent.is_none())
                .map(|entry| entry.name.clone())
                .collect();
            offers.push(OfferDesc {
                offer: ids::offer_to_native(record.id),
                origin: splice_files::ipc::summarize_origin(&self.machine_name(&record.owner)),
                count: roots.len() as u32,
                names: splice_files::ipc::summarize_names(&roots),
                total_size: Some(record.manifest.total_bytes),
                state: desc_state,
                expires_at: Some((record.expires_unix_ms / 1000) as i64),
            });
        }
        let fresh: Vec<FileOfferId> = available
            .iter()
            .copied()
            .filter(|id| !self.announced_offers.contains(id))
            .collect();
        self.announced_offers.retain(|id| available.contains(id));
        if !fresh.is_empty() {
            for id in fresh {
                self.announced_offers.insert(id);
            }
            self.open_shelf();
        }
        let encoded = serde_json::to_string(&offers).unwrap_or_default();
        if self.offers_snapshot.as_deref() != Some(encoded.as_str()) {
            self.offers_snapshot = Some(encoded);
            let pages = splice_files::ipc::paged(offers, splice_files::ipc::MAX_PACK);
            for client in self.clients.values().filter(|client| client.ready) {
                for (chunk, more) in &pages {
                    client.sender.send(ServiceToHelper::Offers {
                        offers: chunk.clone(),
                        more: *more,
                    });
                }
            }
        }
        let stale: Vec<ViewId> = self
            .views
            .iter()
            .filter(|(view, info)| {
                info.phase != Phase::Restored
                    && info.receipt.is_none()
                    && !available.contains(&info.offer)
                    && !self.content.is_committed(**view)
            })
            .map(|(view, _)| *view)
            .collect();
        for view in stale {
            self.retire_view(view, true);
        }
        if let Some(error) = state.error.as_ref() {
            tracing::debug!(error, "file service error");
        }
        self.retry_receipts.retain(|transfer, _| {
            state
                .transfers
                .iter()
                .any(|candidate| candidate.id == *transfer)
        });
        let mut completed = Vec::new();
        for transfer in state.transfers.iter().filter(|transfer| {
            transfer.direction == TransferDirection::Receive
                && transfer.state == TransferState::Ready
        }) {
            if let Some(client) = self.retry_receipts.remove(&transfer.id) {
                completed.push((transfer.id, client, transfer.offer, transfer.peer.clone()));
            }
        }
        for (transfer, client, offer, peer) in completed {
            let origin = self.machine_name(&peer);
            if self.content.receipt_state(transfer).is_some() {
                continue;
            }
            if self.content.receipt_count() >= MAX_RECEIPTS {
                self.client_status(client, format!("The retained-file store is full ({MAX_RECEIPTS} receipts); clear received files before accepting more"));
                continue;
            }
            let files = files_handle.clone();
            let incarnation = self.incarnation;
            self.jobs.spawn(async move {
                let result = files
                    .retain_received(transfer)
                    .await
                    .map(|lease| (transfer, lease))
                    .map_err(|error| describe(&error));
                Done::RetainedForReceipt {
                    incarnation,
                    client: Some(client),
                    offer,
                    origin,
                    recovered: false,
                    result,
                }
            });
        }
        self.reconcile_receipts();
    }

    fn recover_orphan_receipts(&mut self) {
        let Some(attached) = &self.attached else {
            return;
        };
        let files_handle = attached.files.clone();
        let state = attached.files_state.borrow().clone();
        for transfer in state.transfers.iter().filter(|transfer| {
            transfer.direction == TransferDirection::Receive
                && transfer.state == TransferState::Ready
        }) {
            if self.content.receipt_state(transfer.id).is_some() {
                continue;
            }
            if self.content.receipt_count() >= MAX_RECEIPTS {
                self.note_all(format!("The retained-file store is full ({MAX_RECEIPTS} receipts); clear received files before accepting more"));
                break;
            }
            let offer = transfer.offer;
            let origin = self.machine_name(&transfer.peer);
            let files = files_handle.clone();
            let transfer_id = transfer.id;
            let incarnation = self.incarnation;
            self.jobs.spawn(async move {
                let result = files
                    .retain_received(transfer_id)
                    .await
                    .map(|lease| (transfer_id, lease))
                    .map_err(|error| describe(&error));
                Done::RetainedForReceipt {
                    incarnation,
                    client: None,
                    offer,
                    origin,
                    recovered: true,
                    result,
                }
            });
        }
    }

    fn sync_receipts(&mut self) {
        let published = self
            .publication
            .as_ref()
            .map(|publication| publication.transfer());
        let version = self.content.version();
        if self.receipts_sent == Some((version, published)) {
            return;
        }
        self.receipts_sent = Some((version, published));
        let receipts: Vec<ReceiptDesc> = self
            .content
            .summaries()
            .into_iter()
            .map(|summary| ReceiptDesc {
                receipt: ids::transfer_key(summary.transfer),
                offer: ids::offer_to_native(summary.offer),
                origin: splice_files::ipc::summarize_origin(&summary.origin),
                count: summary.names.len() as u32,
                names: splice_files::ipc::summarize_names(&summary.names),
                total_size: Some(summary.total_bytes),
                state: summary.state,
                error: summary
                    .error
                    .as_deref()
                    .map(splice_files::ipc::summarize_error),
                published: published == Some(summary.transfer),
            })
            .collect();
        let pages = splice_files::ipc::paged(receipts, splice_files::ipc::MAX_PACK);
        for client in self.clients.values().filter(|client| client.ready) {
            for (chunk, more) in &pages {
                client.sender.send(ServiceToHelper::Receipts {
                    receipts: chunk.clone(),
                    more: *more,
                });
            }
        }
    }

    async fn tick(&mut self) {
        let mut updates = Vec::new();
        for (offer, progress) in &self.progress {
            if self.sent_progress.get(offer) != Some(progress) {
                updates.push((*offer, progress.clone()));
            }
        }
        for (offer, progress) in updates {
            for client in self.clients.values().filter(|client| client.ready) {
                client.sender.send(ServiceToHelper::Progress {
                    offer: ids::offer_to_native(offer),
                    state: progress.0.clone(),
                    done: progress.1,
                    total: progress.2,
                });
            }
            self.sent_progress.insert(offer, progress);
        }
        let epoch = self.attached.as_ref().map(|attached| attached.clip.epoch());
        if let Some(publication) = &self.publication {
            if epoch != Some(publication.epoch()) {
                if let Some(publication) = self.publication.take() {
                    publication.release().await;
                }
            }
        }
        let expired: Vec<ViewId> = self
            .views
            .iter()
            .filter(|(view, info)| {
                info.phase == Phase::Dropped
                    && info
                        .dropped_at
                        .is_some_and(|dropped| dropped.elapsed() >= DROPPED_VIEW_TTL)
                    && self
                        .mount
                        .as_ref()
                        .and_then(|mount| mount.view_state(**view))
                        == Some(ViewState::DroppedAwaitingRead)
            })
            .map(|(view, _)| *view)
            .collect();
        for view in expired {
            self.retire_view(view, true);
        }
        if self.content.receives().revision() != self.receives_seen {
            self.sync_files();
        }
        let now = Instant::now();
        if now >= self.next_reconcile {
            self.next_reconcile = now + RECONCILE_INTERVAL;
            if !self.content.unrestored().is_empty() {
                self.reconcile_receipts();
            }
        }
        self.sync_receipts();
        if let Some(age) = self.helper.running() {
            if age >= shelf::CONNECT_GRACE
                && !self.clients.values().any(|client| client.ready)
                && self.helper_error.is_none()
            {
                self.helper_error =
                    Some("The file shelf is running but has not connected to Splice".into());
                self.publish_status();
            }
        }
    }
}

fn current_receive(
    tracked: Option<TransferId>,
    offer: FileOfferId,
    transfers: &[TransferRecord],
) -> Option<&TransferRecord> {
    let candidates = || {
        transfers
            .iter()
            .filter(|transfer| transfer.offer == offer && transfer.direction == TransferDirection::Receive)
    };
    if let Some(current) = tracked.and_then(|id| candidates().find(|transfer| transfer.id == id)) {
        return Some(current);
    }
    candidates()
        .find(|transfer| !transfer.state.terminal())
        .or_else(|| candidates().find(|transfer| transfer.state == TransferState::Ready))
        .or_else(|| candidates().find(|transfer| transfer.state == TransferState::Failed))
        .or_else(|| candidates().find(|transfer| transfer.state == TransferState::Cancelled))
}

fn progress_label(transfer: &TransferRecord) -> String {
    match transfer.state {
        TransferState::Preparing | TransferState::Committed => "Preparing".to_string(),
        TransferState::Receiving => "Receiving".to_string(),
        TransferState::Verifying => "Verifying".to_string(),
        TransferState::Ready => "Ready".to_string(),
        TransferState::Cancelled => "Cancelled".to_string(),
        TransferState::Failed => format!(
            "Failed: {}",
            transfer.error.as_deref().unwrap_or("unknown error")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::content::Receives;

    fn record(id: u8, offer: u8, direction: TransferDirection, state: TransferState) -> TransferRecord {
        TransferRecord {
            id: TransferId([id; 16]),
            offer: FileOfferId([offer; 16]),
            peer: MachineId("peer".into()),
            direction,
            state,
            bytes: 0,
            total_bytes: 0,
            paths: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn tracked_transfer_wins_regardless_of_id_order() {
        let offer = FileOfferId([1; 16]);
        let transfers = vec![
            record(2, 1, TransferDirection::Receive, TransferState::Receiving),
            record(9, 1, TransferDirection::Receive, TransferState::Failed),
        ];
        let current = current_receive(Some(TransferId([2; 16])), offer, &transfers).unwrap();
        assert_eq!(current.id, TransferId([2; 16]));
        assert_eq!(current.state, TransferState::Receiving);
        let current = current_receive(Some(TransferId([9; 16])), offer, &transfers).unwrap();
        assert_eq!(current.state, TransferState::Failed);
    }

    #[test]
    fn untracked_selection_prefers_active_then_ready_then_failed_then_cancelled() {
        let offer = FileOfferId([1; 16]);
        let mut transfers = vec![
            record(9, 1, TransferDirection::Receive, TransferState::Cancelled),
            record(7, 1, TransferDirection::Receive, TransferState::Failed),
        ];
        assert_eq!(current_receive(None, offer, &transfers).unwrap().state, TransferState::Failed);
        transfers.push(record(1, 1, TransferDirection::Receive, TransferState::Ready));
        assert_eq!(current_receive(None, offer, &transfers).unwrap().state, TransferState::Ready);
        transfers.push(record(3, 1, TransferDirection::Receive, TransferState::Verifying));
        assert_eq!(current_receive(None, offer, &transfers).unwrap().id, TransferId([3; 16]));
        transfers.retain(|transfer| transfer.state == TransferState::Cancelled);
        assert_eq!(current_receive(None, offer, &transfers).unwrap().state, TransferState::Cancelled);
    }

    #[test]
    fn native_commit_after_a_cancelled_button_receive_becomes_the_current_transfer() {
        let offer = FileOfferId([1; 16]);
        let receives = Receives::new();
        let button = TransferId([9; 16]);
        let native = TransferId([2; 16]);
        receives.record(offer, button);
        let seen = receives.revision();
        let transfers = vec![
            record(9, 1, TransferDirection::Receive, TransferState::Cancelled),
            record(2, 1, TransferDirection::Receive, TransferState::Receiving),
        ];
        let current = current_receive(receives.current(offer), offer, &transfers).unwrap();
        assert_eq!(current.id, button);
        assert_eq!(current.state, TransferState::Cancelled);
        receives.record(offer, native);
        assert!(receives.revision() > seen);
        let current = current_receive(receives.current(offer), offer, &transfers).unwrap();
        assert_eq!(current.id, native);
        assert_eq!(current.state, TransferState::Receiving);
        receives.retain(|candidate| candidate == offer);
        assert_eq!(receives.current(offer), Some(native));
        assert_eq!(current_receive(receives.current(offer), offer, &transfers[..1]).unwrap().id, button);
        assert_eq!(receives.current(offer), Some(native));
        receives.retain(|_| false);
        assert_eq!(receives.current(offer), None);
        assert_eq!(current_receive(None, offer, &transfers).unwrap().id, native);
    }

    #[test]
    fn stale_tracking_and_foreign_records_are_ignored() {
        let offer = FileOfferId([1; 16]);
        let transfers = vec![
            record(5, 1, TransferDirection::Send, TransferState::Receiving),
            record(6, 2, TransferDirection::Receive, TransferState::Receiving),
            record(4, 1, TransferDirection::Receive, TransferState::Failed),
        ];
        let current = current_receive(Some(TransferId([8; 16])), offer, &transfers).unwrap();
        assert_eq!(current.id, TransferId([4; 16]));
        assert!(current_receive(None, FileOfferId([3; 16]), &transfers).is_none());
        assert_eq!(progress_label(current), "Failed: unknown error");
    }
}
