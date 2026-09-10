use parking_lot::Mutex;
use splice_core::files::{LocalSelection, ReceivedLease, SelectedRoot, TransferId};
use splice_core::EngineHandle;
use splice_platform::linux::{fileclip, fileportal};
use splice_platform::{ClipFetch, Clipboard, ClipboardOffer, ClipboardClock, ClipboardGuard, ClipboardObserver, Platform};
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const READ_DEADLINE: Duration = Duration::from_secs(5);
const PUBLICATION_ID_BASE: u64 = 1 << 62;

pub struct ClipboardShared {
    clock: Arc<ClipboardClock>,
    pending: Mutex<Option<tokio::task::JoinHandle<()>>>,
    clipboard: Arc<dyn Clipboard>,
    dbus: Option<zbus::Connection>,
    engine: Mutex<Option<EngineHandle>>,
    notice: watch::Sender<Option<String>>,
    publish_lock: tokio::sync::Mutex<()>,
}

pub type EngineFiles = Arc<ClipboardShared>;

impl ClipboardShared {
    pub fn epoch(&self) -> u64 {
        self.clock.current()
    }

    pub fn set_engine(&self, engine: EngineHandle) {
        *self.engine.lock() = Some(engine.clone());
        self.report(engine.invalidate_file_clipboard(self.epoch()), "File clipboard invalidation failed");
    }

    pub fn notices(&self) -> watch::Receiver<Option<String>> {
        self.notice.subscribe()
    }

    fn notify(&self, message: Option<String>) {
        self.notice.send_replace(message);
    }
}

pub async fn install(platform: &mut Platform) -> EngineFiles {
    let dbus = match zbus::Connection::session().await {
        Ok(connection) => Some(connection),
        Err(error) => {
            tracing::warn!(%error, "no D-Bus session bus; portal file selections and exports are unavailable");
            None
        }
    };
    let inner = platform.clipboard.clone();
    let shared = Arc::new(ClipboardShared {
        clock: splice_platform::native_clipboard_clock(),
        pending: Mutex::new(None),
        clipboard: inner,
        dbus,
        engine: Mutex::new(None),
        notice: watch::channel(None).0,
        publish_lock: tokio::sync::Mutex::new(()),
    });
    platform.clipboard = Arc::new(EpochClipboard { shared: shared.clone() });
    shared.clock.observe(Arc::new(Observer {
        shared: Arc::downgrade(&shared),
        runtime: tokio::runtime::Handle::current(),
    }));
    shared
}

struct EpochClipboard {
    shared: Arc<ClipboardShared>,
}

#[async_trait::async_trait]
impl Clipboard for EpochClipboard {
    async fn set_remote_offer(&self, offer: ClipboardOffer, fetch: Arc<dyn ClipFetch>) -> splice_platform::Result<()> {
        let generation = self.shared.clock.invalidate();
        let fetch = Arc::new(GuardedFetch {
            inner: fetch,
            guard: ClipboardGuard::new(self.shared.clock.clone(), generation, None),
        });
        self.shared.clipboard.set_remote_offer(offer, fetch).await
    }

    async fn read_local(&self, mime: &str) -> splice_platform::Result<Vec<u8>> {
        self.shared.clipboard.read_local(mime).await
    }
}

impl ClipboardShared {
    fn report(&self, result: anyhow::Result<()>, what: &str) {
        if let Err(error) = result {
            self.notify(Some(format!("{what}: {error}")));
        }
    }
}

struct Observer {
    shared: std::sync::Weak<ClipboardShared>,
    runtime: tokio::runtime::Handle,
}

impl ClipboardObserver for Observer {
    fn invalidated(&self, generation: u64) {
        let Some(shared) = self.shared.upgrade() else { return };
        if let Some(previous) = shared.pending.lock().take() {
            previous.abort();
        }
        let engine = shared.engine.lock().clone();
        if let Some(engine) = engine {
            shared.report(engine.invalidate_file_clipboard(generation), "File clipboard invalidation failed");
        }
    }

    fn changed(&self, generation: u64, mimes: Vec<String>, inline_text: Option<String>) {
        let Some(shared) = self.shared.upgrade() else { return };
        let engine = shared.engine.lock().clone();
        let Some(engine) = engine else { return };
        if fileclip::probe_mimes(&mimes).is_none() {
            shared.report(engine.clipboard_changed(generation, mimes, inline_text), "Clipboard sync failed");
        } else {
            *shared.pending.lock() = Some(self.runtime.spawn(capture(shared.clone(), engine, generation, mimes, inline_text)));
        }
    }
}

struct GuardedFetch {
    inner: Arc<dyn ClipFetch>,
    guard: ClipboardGuard,
}

#[async_trait::async_trait]
impl ClipFetch for GuardedFetch {
    fn publication_guard(&self) -> Option<ClipboardGuard> {
        Some(self.guard.clone())
    }

    async fn fetch(&self, mime: &str) -> Option<Vec<u8>> {
        if self.guard.is_cancelled() {
            return None;
        }
        let bytes = self.inner.fetch(mime).await;
        if self.guard.is_cancelled() { None } else { bytes }
    }
}

async fn capture(
    shared: Arc<ClipboardShared>,
    engine: EngineHandle,
    generation: u64,
    mimes: Vec<String>,
    inline_text: Option<String>,
) {
    let outcome = match shared.dbus.as_ref() {
        Some(dbus) => match tokio::time::timeout(READ_DEADLINE, fileclip::read_selection(&shared.clipboard, dbus, &mimes)).await {
            Ok(Ok(selection)) => Ok(selection),
            Ok(Err(error)) => Err(format!("cannot read the copied file selection: {error}")),
            Err(_) => Err("reading the copied file selection timed out".to_string()),
        },
        None => Err("file clipboard capture needs a D-Bus session bus".to_string()),
    };
    if shared.epoch() != generation {
        return;
    }
    match outcome {
        Ok(Some(selection)) => {
            let files = selection.paths.len();
            let items = if selection.portal_fds.is_empty() {
                selection.paths.into_iter().map(|path| (path, None)).collect()
            } else if selection.portal_fds.len() == files {
                selection.paths.into_iter().zip(selection.portal_fds.into_iter().map(Some)).collect()
            } else {
                Vec::new()
            };
            match local_selection(items) {
                Ok(selection) => {
                    let captured = engine.capture_file_clipboard(selection, generation).map_err(|error| error.to_string());
                    match captured {
                        Ok(()) => {
                            tracing::info!(files, generation, "captured local file clipboard selection");
                            shared.notify(None);
                        }
                        Err(error) => {
                            shared.notify(Some(format!("File clipboard capture failed: {error}")));
                            shared.report(engine.clipboard_changed(generation, text_free(mimes), None), "Clipboard sync failed");
                        }
                    }
                }
                Err(error) => {
                    shared.notify(Some(format!("File clipboard capture failed: {error}")));
                    shared.report(engine.clipboard_changed(generation, text_free(mimes), None), "Clipboard sync failed");
                }
            }
        }
        Ok(None) => {
            shared.report(engine.clipboard_changed(generation, file_free(mimes), inline_text), "Clipboard sync failed");
        }
        Err(error) => {
            shared.notify(Some(format!("File clipboard capture failed: {error}")));
            shared.report(engine.clipboard_changed(generation, text_free(mimes), None), "Clipboard sync failed");
        }
    }
}

fn is_text(mime: &str) -> bool {
    mime.split(';').next().unwrap_or(mime).trim().eq_ignore_ascii_case("text/plain")
}

pub fn file_free(mimes: Vec<String>) -> Vec<String> {
    mimes.into_iter().filter(|mime| !splice_core::files::is_file_mime(mime)).collect()
}

pub fn text_free(mimes: Vec<String>) -> Vec<String> {
    file_free(mimes).into_iter().filter(|mime| !is_text(mime)).collect()
}

pub fn portal_doc_path(path: &std::path::Path) -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .is_some_and(|dir| path.starts_with(PathBuf::from(dir).join("doc")))
}

pub fn selection_with_descriptors(paths: Vec<PathBuf>, fds: Vec<OwnedFd>) -> Result<Vec<(PathBuf, Option<OwnedFd>)>, String> {
    let mut fds = fds.into_iter();
    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        let fd = if portal_doc_path(&path) {
            Some(fds.next().ok_or_else(|| format!("{} is a portal document without a descriptor", path.display()))?)
        } else {
            None
        };
        items.push((path, fd));
    }
    if fds.next().is_some() {
        return Err("the drop carried more descriptors than portal documents".into());
    }
    Ok(items)
}

pub fn local_selection(items: Vec<(PathBuf, Option<OwnedFd>)>) -> Result<LocalSelection, String> {
    if items.is_empty() {
        return Err("the selection contains no local files".into());
    }
    if items.len() > splice_proto::files::MAX_ROOTS {
        return Err(format!("the selection has more than {} items", splice_proto::files::MAX_ROOTS));
    }
    let mut roots = Vec::with_capacity(items.len());
    for (path, fd) in items {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| splice_proto::files::valid_name(name))
            .ok_or_else(|| format!("{} has an unsupported name", path.display()))?
            .to_owned();
        match fd {
            Some(fd) => {
                let file = std::fs::File::from(fd);
                let kind = file.metadata().map_err(|error| format!("{name}: {error}"))?.file_type();
                if !kind.is_file() && !kind.is_dir() {
                    return Err(format!("{name} is not a regular file or directory"));
                }
                roots.push(SelectedRoot::Open { name, file: Arc::new(file) });
            }
            None => {
                if !path.is_absolute() {
                    return Err(format!("{} is not an absolute path", path.display()));
                }
                let kind = std::fs::symlink_metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?.file_type();
                if !kind.is_file() && !kind.is_dir() {
                    return Err(format!("{} is not a regular file or directory", path.display()));
                }
                roots.push(SelectedRoot::Path(path));
            }
        }
    }
    Ok(LocalSelection { roots, access: None })
}

struct Representations {
    entries: Vec<(String, Vec<u8>)>,
}

#[async_trait::async_trait]
impl ClipFetch for Representations {
    async fn fetch(&self, mime: &str) -> Option<Vec<u8>> {
        self.entries.iter().find(|(candidate, _)| candidate == mime).map(|(_, bytes)| bytes.clone())
    }
}

pub struct Publication {
    lease: ReceivedLease,
    export: Option<fileportal::PortalExport>,
    epoch: u64,
}

impl Publication {
    pub fn new(lease: ReceivedLease, export: Option<fileportal::PortalExport>, epoch: u64) -> Publication {
        Publication { lease, export, epoch }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn transfer(&self) -> TransferId {
        self.lease.files().transfer
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.lease.files().paths
    }

    pub async fn release(self) {
        if let Some(export) = self.export {
            export.stop().await;
        }
        drop(self.lease);
    }
}

struct PendingPublication(Option<ClipboardGuard>);

impl Drop for PendingPublication {
    fn drop(&mut self) {
        if let Some(guard) = &self.0 {
            guard.cancel();
        }
    }
}

pub async fn publish(shared: &ClipboardShared, paths: &[PathBuf], epoch: u64, portal: bool, intent: Option<(Arc<AtomicU64>, u64)>) -> anyhow::Result<Option<fileportal::PortalExport>> {
    let _serialized = shared.publish_lock.lock().await;
    let guard = ClipboardGuard::new(shared.clock.clone(), epoch, intent);
    guard.check()?;
    let mut pending = PendingPublication(Some(guard.clone()));
    anyhow::ensure!(!paths.is_empty(), "the receipt published no files");
    let mut entries = fileclip::completed_offer(paths);
    let export = match (portal, shared.dbus.as_ref()) {
        (true, Some(dbus)) => {
            let export = export_paths(dbus, paths).await.map_err(|error| anyhow::anyhow!("cannot export the received files through the FileTransfer portal: {error}"))?;
            entries.push((splice_platform::files::MIME_PORTAL_FILETRANSFER.to_owned(), export.key().as_bytes().to_vec()));
            Some(export)
        }
        (true, None) => anyhow::bail!("portal publication needs a D-Bus session bus"),
        (false, _) => None,
    };
    guard.check()?;
    let mimes = entries.iter().map(|(mime, _)| mime.clone()).collect();
    let fetch = Arc::new(GuardedFetch { inner: Arc::new(Representations { entries }), guard: guard.clone() });
    let id = PUBLICATION_ID_BASE | (epoch & (PUBLICATION_ID_BASE - 1));
    shared
        .clipboard
        .set_remote_offer(ClipboardOffer { id, mimes, inline_text: None }, fetch)
        .await
        .map_err(|error| anyhow::anyhow!("cannot publish received files to the clipboard: {error}"))?;
    guard.check()?;
    anyhow::ensure!(guard.is_published(), "the native clipboard backend did not publish the offer");
    pending.0.take();
    Ok(export)
}

async fn export_paths(dbus: &zbus::Connection, paths: &[PathBuf]) -> anyhow::Result<fileportal::PortalExport> {
    let mut fds = Vec::with_capacity(paths.len());
    for path in paths {
        fds.push(OwnedFd::from(std::fs::File::open(path)?));
    }
    Ok(fileportal::PortalExport::begin(dbus, fds).await?)
}

pub async fn portal_available(shared: &ClipboardShared) -> bool {
    match shared.dbus.as_ref() {
        Some(dbus) => fileportal::probe(dbus).await,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn file_and_path_text_representations_are_suppressed() {
        let mimes: Vec<String> = ["text/uri-list", "text/plain;charset=utf-8", "x-special/gnome-copied-files", "application/x-kde-cutselection", "text/html", "application/vnd.portal.filetransfer"]
            .into_iter().map(String::from).collect();
        assert_eq!(file_free(mimes.clone()), vec!["text/plain;charset=utf-8".to_string(), "text/html".to_string()]);
        assert_eq!(text_free(mimes), vec!["text/html".to_string()]);
    }

    #[test]
    fn local_selection_validates_paths_and_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.txt");
        std::fs::write(&file, b"x").unwrap();
        let selection = local_selection(vec![(file.clone(), None), (dir.path().to_path_buf(), None)]).unwrap();
        assert_eq!(selection.roots.len(), 2);
        assert!(matches!(&selection.roots[0], SelectedRoot::Path(path) if path == &file));
        assert!(local_selection(vec![(PathBuf::from("relative/x"), None)]).is_err());
        assert!(local_selection(vec![(dir.path().join("missing"), None)]).is_err());
        assert!(local_selection(Vec::new()).is_err());
        let fd = OwnedFd::from(std::fs::File::open(&file).unwrap());
        let opened = local_selection(vec![(PathBuf::from("/run/user/1000/doc/abc/report.txt"), Some(fd))]).unwrap();
        assert!(matches!(&opened.roots[0], SelectedRoot::Open { name, .. } if name == "report.txt"));
        let fd = OwnedFd::from(std::fs::File::open(&file).unwrap());
        assert!(local_selection(vec![(PathBuf::from("/run/user/1000/doc/abc/.."), Some(fd))]).is_err());
    }

    #[test]
    fn source_drop_descriptors_bind_only_to_portal_documents() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("run");
        std::fs::create_dir_all(runtime.join("doc")).unwrap();
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, b"x").unwrap();
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        let portal = runtime.join("doc").join("k").join("shared.txt");
        let fd = OwnedFd::from(std::fs::File::open(&file).unwrap());
        let items = selection_with_descriptors(vec![file.clone(), portal.clone()], vec![fd]).unwrap();
        assert!(items[0].1.is_none());
        assert!(items[1].1.is_some());
        assert!(selection_with_descriptors(vec![portal.clone()], Vec::new()).is_err());
        let fd = OwnedFd::from(std::fs::File::open(&file).unwrap());
        assert!(selection_with_descriptors(vec![file], vec![fd]).is_err());
        match previous {
            Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[tokio::test]
    async fn representations_serve_only_advertised_mimes() {
        let fetch = Representations { entries: fileclip::completed_offer(&[PathBuf::from("/cache/a b.txt")]) };
        let uri = fetch.fetch("text/uri-list").await.unwrap();
        assert_eq!(uri, b"file:///cache/a%20b.txt\r\n");
        assert_eq!(fetch.fetch("application/x-kde-cutselection").await.unwrap(), b"0");
        assert!(fetch.fetch("text/plain").await.is_none());
    }

    struct MockClipboard {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        block: std::sync::atomic::AtomicBool,
        calls: std::sync::atomic::AtomicUsize,
        acknowledge: std::sync::atomic::AtomicBool,
        block_after_ack: std::sync::atomic::AtomicBool,
        acknowledged: tokio::sync::Notify,
        error: std::sync::atomic::AtomicBool,
        queued: Mutex<Option<Arc<dyn ClipFetch>>>,
        offers: Mutex<Vec<ClipboardOffer>>,
    }

    impl MockClipboard {
        fn new(block: bool) -> Arc<MockClipboard> {
            Arc::new(MockClipboard {
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                block: std::sync::atomic::AtomicBool::new(block),
                calls: std::sync::atomic::AtomicUsize::new(0),
                acknowledge: std::sync::atomic::AtomicBool::new(true),
                block_after_ack: std::sync::atomic::AtomicBool::new(false),
                acknowledged: tokio::sync::Notify::new(),
                error: std::sync::atomic::AtomicBool::new(false),
                queued: Mutex::new(None),
                offers: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl Clipboard for MockClipboard {
        async fn set_remote_offer(&self, offer: ClipboardOffer, fetch: Arc<dyn ClipFetch>) -> splice_platform::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.queued.lock() = Some(fetch.clone());
            self.entered.notify_one();
            if self.block.load(Ordering::SeqCst) {
                self.release.notified().await;
            }
            if self.error.load(Ordering::Acquire) {
                return Err(splice_platform::PlatformError::Unavailable("injected native failure".into()));
            }
            if !self.acknowledge.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(guard) = fetch.publication_guard() {
                guard.check()?;
                guard.published();
            }
            self.offers.lock().push(offer);
            self.acknowledged.notify_one();
            if self.block_after_ack.load(Ordering::Acquire) {
                self.release.notified().await;
            }
            Ok(())
        }

        async fn read_local(&self, _mime: &str) -> splice_platform::Result<Vec<u8>> {
            Err(splice_platform::PlatformError::Other(anyhow::anyhow!("unused in tests")))
        }
    }

    fn shared_with(clipboard: Arc<MockClipboard>, epoch: u64) -> Arc<ClipboardShared> {
        Arc::new(ClipboardShared {
            clock: {
                let clock = Arc::new(ClipboardClock::default());
                for _ in 0..epoch { clock.invalidate(); }
                clock
            },
            pending: Mutex::new(None),
            clipboard,
            dbus: None,
            engine: Mutex::new(None),
            notice: watch::channel(None).0,
            publish_lock: tokio::sync::Mutex::new(()),
        })
    }

    #[tokio::test]
    async fn stale_publication_is_rejected_before_the_native_call() {
        let mock = MockClipboard::new(false);
        let shared = shared_with(mock.clone(), 5);
        let error = publish(&shared, &[PathBuf::from("/cache/a.txt")], 4, false, None).await.map(|_| ()).expect_err("stale epoch must fail");
        assert!(error.to_string().contains("clipboard changed"), "{error}");
        assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
        let error = publish(&shared, &[], 5, false, None).await.map(|_| ()).expect_err("empty receipt must fail");
        assert!(error.to_string().contains("no files"), "{error}");
        assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn superseded_intent_is_rejected_before_the_native_call() {
        let mock = MockClipboard::new(false);
        let shared = shared_with(mock.clone(), 5);
        let latest = Arc::new(AtomicU64::new(2));
        let error = publish(&shared, &[PathBuf::from("/cache/a.txt")], 5, false, Some((latest, 1))).await.map(|_| ()).expect_err("superseded intent must fail");
        assert!(error.to_string().contains("superseded"), "{error}");
        assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn current_publication_reaches_the_native_backend() {
        let mock = MockClipboard::new(false);
        let shared = shared_with(mock.clone(), 5);
        let export = publish(&shared, &[PathBuf::from("/cache/a.txt")], 5, false, None).await.unwrap();
        assert!(export.is_none());
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        let offers = mock.offers.lock();
        assert!(offers[0].mimes.iter().any(|mime| mime == "text/uri-list"));
        assert!(offers[0].mimes.iter().any(|mime| mime == "x-special/gnome-copied-files"));
        assert!(!offers[0].mimes.iter().any(|mime| mime == "application/vnd.portal.filetransfer"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publications_are_serialized_and_latecomers_recheck_the_epoch() {
        let mock = MockClipboard::new(true);
        let shared = shared_with(mock.clone(), 5);
        let first_shared = shared.clone();
        let first = tokio::spawn(async move { publish(&first_shared, &[PathBuf::from("/cache/a.txt")], 5, false, None).await });
        mock.entered.notified().await;
        let second_shared = shared.clone();
        let second = tokio::spawn(async move { publish(&second_shared, &[PathBuf::from("/cache/b.txt")], 5, false, None).await });

        shared.clock.invalidate();
        mock.release.notify_one();
        let first_result = first.await.unwrap();
        assert!(first_result.is_err(), "the queued native write must reject a changed clipboard");
        assert!(mock.offers.lock().is_empty());
        drop(first_result);
        let error = second.await.unwrap().map(|_| ()).expect_err("the late publication must fail");
        assert!(error.to_string().contains("clipboard changed"), "{error}");
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn queued_publication_rejects_a_newer_receive_intent() {
        let mock = MockClipboard::new(true);
        let shared = shared_with(mock.clone(), 5);
        let latest = Arc::new(AtomicU64::new(1));
        let task = {
            let shared = shared.clone();
            let latest = latest.clone();
            tokio::spawn(async move { publish(&shared, &[PathBuf::from("/cache/a.txt")], 5, false, Some((latest, 1))).await })
        };
        mock.entered.notified().await;
        latest.store(2, Ordering::Release);
        mock.release.notify_one();
        assert!(task.await.unwrap().is_err());
        assert!(mock.offers.lock().is_empty());
    }

    #[tokio::test]
    async fn cancelled_publication_cannot_be_replayed_from_the_native_queue() {
        let mock = MockClipboard::new(true);
        let shared = shared_with(mock.clone(), 5);
        let task = tokio::spawn(async move { publish(&shared, &[PathBuf::from("/cache/a.txt")], 5, false, None).await });
        mock.entered.notified().await;
        let guard = mock.queued.lock().as_ref().unwrap().publication_guard().unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(guard.check().is_err());
        assert!(mock.offers.lock().is_empty());
    }

    #[tokio::test]
    async fn failed_or_unacknowledged_native_publication_cannot_be_replayed() {
        for failure in [true, false] {
            let mock = MockClipboard::new(false);
            mock.error.store(failure, Ordering::Release);
            mock.acknowledge.store(failure, Ordering::Release);
            let shared = shared_with(mock.clone(), 5);
            assert!(publish(&shared, &[PathBuf::from("/cache/a.txt")], 5, false, None).await.is_err());
            let guard = mock.queued.lock().as_ref().unwrap().publication_guard().unwrap();
            assert!(guard.check().is_err());
            assert!(mock.offers.lock().is_empty());
        }
    }

    #[tokio::test]
    async fn abort_after_native_ack_disables_the_selected_file_provider() {
        let mock = MockClipboard::new(false);
        mock.block_after_ack.store(true, Ordering::Release);
        let shared = shared_with(mock.clone(), 5);
        let task = tokio::spawn(async move { publish(&shared, &[PathBuf::from("/cache/a.txt")], 5, false, None).await });
        mock.acknowledged.notified().await;
        let fetch = mock.queued.lock().clone().unwrap();
        let guard = fetch.publication_guard().unwrap();
        assert!(guard.is_published());
        assert!(fetch.fetch("text/uri-list").await.is_some());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(guard.is_cancelled());
        assert!(fetch.fetch("text/uri-list").await.is_none());
        assert!(fetch.fetch("x-special/gnome-copied-files").await.is_none());
    }

    #[tokio::test]
    async fn retired_ordinary_provider_serves_queued_sends_until_cancelled() {
        let clock = Arc::new(ClipboardClock::default());
        let guard = ClipboardGuard::new(clock.clone(), clock.current(), None);
        let fetch = GuardedFetch {
            inner: Arc::new(Representations { entries: vec![("text/plain".into(), b"retained".to_vec())] }),
            guard: guard.clone(),
        };
        guard.published();
        clock.invalidate();
        assert!(guard.check().is_err());
        assert_eq!(fetch.fetch("text/plain").await.unwrap(), b"retained");
        guard.cancel();
        assert!(fetch.fetch("text/plain").await.is_none());
    }

    #[tokio::test]
    async fn in_flight_fetch_rechecks_cancellation_without_rejecting_old_generations() {
        struct DelayedFetch {
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
        }

        #[async_trait::async_trait]
        impl ClipFetch for DelayedFetch {
            async fn fetch(&self, _mime: &str) -> Option<Vec<u8>> {
                self.entered.notify_one();
                self.release.notified().await;
                Some(b"held bytes".to_vec())
            }
        }

        for cancel in [false, true] {
            let clock = Arc::new(ClipboardClock::default());
            let guard = ClipboardGuard::new(clock.clone(), clock.current(), None);
            let inner = Arc::new(DelayedFetch { entered: tokio::sync::Notify::new(), release: tokio::sync::Notify::new() });
            let fetch = GuardedFetch { inner: inner.clone(), guard: guard.clone() };
            let task = tokio::spawn(async move { fetch.fetch("application/vnd.portal.filetransfer").await });
            inner.entered.notified().await;
            clock.invalidate();
            if cancel { guard.cancel(); }
            inner.release.notify_one();
            assert_eq!(task.await.unwrap(), if cancel { None } else { Some(b"held bytes".to_vec()) });
        }
    }

}
