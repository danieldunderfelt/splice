use super::files::{EventReservation, ShelfCtx};
use crate::file_shelf::{Access, DragAttempt, DragOutcome, FileEvent, OfferId, PromiseCancellation, PromiseWriter, ReceivedSelection, TransferId};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_app_kit::{NSFilePromiseProvider, NSFilePromiseProviderDelegate};
use objc2_foundation::{NSError, NSOperationQueue, NSString, NSURL};
use objc2_uniform_type_identifiers::UTType;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

const GENERIC_FILE_TYPE: &str = "public.data";
const ERROR_DOMAIN: &str = "io.splice.files";
const CHUNK: usize = 1024 * 1024;

struct AttemptLease {
    attempt: DragAttempt,
    retirement: Option<EventReservation>,
    _admission: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for AttemptLease {
    fn drop(&mut self) {
        if let Some(retirement) = self.retirement.take() {
            retirement.send(FileEvent::DragRetired { attempt: self.attempt });
        }
    }
}

pub struct DelegateIvars {
    ctx: Arc<ShelfCtx>,
    attempt: DragAttempt,
    root: u32,
    name: String,
    queue: Retained<NSOperationQueue>,
    _lease: Arc<AttemptLease>,
    finished: Mutex<Option<EventReservation>>,
    requested: AtomicBool,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SpliceFilePromiseDelegate"]
    #[ivars = DelegateIvars]
    pub struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, _provider: &NSFilePromiseProvider, _file_type: &NSString) -> Retained<NSString> {
            NSString::from_str(&self.ivars().name)
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &block2::DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let _delegate = unsafe { Retained::retain(self as *const Self as *mut Self) };
            let iv = self.ivars();
            let first_request = !iv.requested.swap(true, Ordering::AcqRel);
            let scoped = unsafe { url.startAccessingSecurityScopedResource() };
            let (outcome, access) = if !first_request {
                (Err("This promised root has already been requested".into()), None)
            } else {
                match url.path().filter(|_| url.isFileURL()) {
                    Some(path) => fulfil(&iv.ctx, iv.attempt, iv.root, PathBuf::from(path.to_string())),
                    None => (Err("AppKit supplied a promise destination that is not a file URL".into()), None),
                }
            };
            match &outcome {
                Ok(_) => completion.call((std::ptr::null_mut(),)),
                Err(message) => {
                    let error = unsafe {
                        NSError::errorWithDomain_code_userInfo(
                            &NSString::from_str(ERROR_DOMAIN),
                            1,
                            Some(&objc2_foundation::NSDictionary::from_slices(
                                &[&*NSString::from_str("NSLocalizedDescription")],
                                &[&*NSString::from_str(message) as &objc2::runtime::AnyObject],
                            )),
                        )
                    };
                    completion.call((Retained::as_ptr(&error) as *mut NSError,));
                }
            }
            if scoped {
                unsafe { url.stopAccessingSecurityScopedResource() };
            }
            drop(access);
            if first_request {
                if let Some(finished) = iv.finished.lock().take() {
                    finished.send(FileEvent::PromiseFinished { attempt: iv.attempt, root: iv.root, result: outcome });
                }
            }
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl PromiseDelegate {
    fn new(ivars: DelegateIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

fn fulfil(
    ctx: &Arc<ShelfCtx>,
    attempt: DragAttempt,
    root: u32,
    destination: PathBuf,
) -> (Result<PathBuf, String>, Option<Access>) {
    let (writer, reply) = PromiseWriter::channel();
    let cancellation = writer.cancellation();
    if !ctx.emit(FileEvent::PromiseRequested {
        attempt,
        root,
        destination: destination.clone(),
        writer,
    }) {
        return (Err("Splice file service is not accepting work".into()), None);
    }
    let source = match reply.recv() {
        Ok(Ok(source)) => source,
        Ok(Err(message)) => return (Err(message), None),
        Err(_) => return (Err("Splice file service stopped before the transfer completed".into()), None),
    };
    let result = publish(&source.path, &destination, attempt, root, &cancellation);
    (result, source.access)
}

pub fn staging_path(destination: &Path, attempt: DragAttempt, root: u32) -> Option<PathBuf> {
    let parent = destination.parent()?;
    destination.file_name()?;
    static NAMESPACE: OnceLock<u128> = OnceLock::new();
    let namespace = NAMESPACE.get_or_init(rand::random);
    Some(parent.join(format!(".splice-{namespace:032x}-{}-{root}.part", attempt.0)))
}

pub fn publish(
    source: &Path,
    destination: &Path,
    attempt: DragAttempt,
    root: u32,
    cancellation: &PromiseCancellation,
) -> Result<PathBuf, String> {
    let mut input = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(source)
        .map_err(|e| format!("verified file {} is unreadable: {e}", source.display()))?;
    let metadata = input.metadata()
        .map_err(|e| format!("verified file {} is unreadable: {e}", source.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{} is not a regular file; folders are received first, then dragged as local files",
            source.display()
        ));
    }
    let staging = staging_path(destination, attempt, root)
        .ok_or_else(|| format!("{} is not a usable promise destination", destination.display()))?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .map_err(|e| format!("creating staging file {} failed: {e}", staging.display()))?;
    let outcome = stream(&mut input, &mut output, metadata.len(), cancellation).and_then(|()| {
        output.set_permissions(std::fs::Permissions::from_mode(metadata.mode() & 0o777))
            .map_err(|error| format!("preserving promised file permissions failed: {error}"))?;
        let modified = metadata.modified().map_err(|error| format!("reading the promised file timestamp failed: {error}"))?;
        output.set_times(std::fs::FileTimes::new().set_modified(modified))
            .map_err(|error| format!("preserving the promised file timestamp failed: {error}"))?;
        output.sync_all().map_err(|error| format!("syncing promised file metadata failed: {error}"))
    });
    drop(output);
    if let Err(message) = outcome {
        let _ = std::fs::remove_file(&staging);
        return Err(message);
    }
    publish_staging(&staging, destination, cancellation)
}

fn publish_staging(staging: &Path, destination: &Path, cancellation: &PromiseCancellation) -> Result<PathBuf, String> {
    if let Err(message) = cancellation.begin_publication() {
        let _ = std::fs::remove_file(staging);
        return Err(message);
    }
    if let Err(e) = rename_no_replace(staging, destination) {
        let _ = std::fs::remove_file(staging);
        return Err(format!("publishing {} failed: {e}", destination.display()));
    }
    Ok(destination.to_path_buf())
}

fn stream(input: &mut std::fs::File, output: &mut std::fs::File, expected: u64, cancellation: &PromiseCancellation) -> Result<(), String> {
    let mut buffer = vec![0u8; CHUNK];
    let mut copied = 0u64;
    loop {
        if cancellation.is_cancelled() {
            return Err("promised file transfer was cancelled".into());
        }
        let read = input.read(&mut buffer).map_err(|e| format!("reading the received file failed: {e}"))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|e| format!("writing the promised file failed: {e}"))?;
        copied += read as u64;
    }
    if copied != expected {
        return Err(format!("received file changed while copying: wrote {copied} of {expected} bytes"));
    }
    Ok(())
}

pub fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub fn file_type_for(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| UTType::typeWithFilenameExtension(&NSString::from_str(e)))
        .map(|t| t.identifier().to_string())
        .unwrap_or_else(|| GENERIC_FILE_TYPE.to_string())
}

enum Attempt {
    Promise {
        _providers: Vec<Retained<NSFilePromiseProvider>>,
        ended: EventReservation,
    },
    Local {
        selection: ReceivedSelection,
    },
}

thread_local! {
    static ATTEMPTS: RefCell<HashMap<DragAttempt, Attempt>> = RefCell::new(HashMap::new());
    static EXPORTS: RefCell<HashMap<TransferId, ReceivedSelection>> = RefCell::new(HashMap::new());
}

pub struct PromisedRoot {
    pub id: u32,
    pub name: String,
}

pub fn providers_for(
    ctx: &Arc<ShelfCtx>,
    attempt: DragAttempt,
    offer: OfferId,
    roots: &[PromisedRoot],
    queue: &Retained<NSOperationQueue>,
) -> Option<Vec<Retained<NSFilePromiseProvider>>> {
    if roots.is_empty() {
        return None;
    }
    let admission = ctx.attempts.clone().try_acquire_owned().ok()?;
    let mut events = ctx.reserve_events(roots.len().checked_add(3)?)?;
    let started = events.pop()?;
    let ended = events.pop()?;
    let retirement = events.pop()?;
    let lease = Arc::new(AttemptLease { attempt, retirement: Some(retirement), _admission: admission });
    let mut providers = Vec::with_capacity(roots.len());
    for (root, finished) in roots.iter().zip(events) {
        let delegate = PromiseDelegate::new(DelegateIvars {
            ctx: ctx.clone(),
            attempt,
            root: root.id,
            name: root.name.clone(),
            queue: queue.clone(),
            _lease: lease.clone(),
            finished: Mutex::new(Some(finished)),
            requested: AtomicBool::new(false),
        });
        let provider = NSFilePromiseProvider::initWithFileType_delegate(
            NSFilePromiseProvider::alloc(),
            &NSString::from_str(&file_type_for(&root.name)),
            ProtocolObject::from_ref(&*delegate),
        );
        unsafe { provider.setUserInfo(Some(&delegate)) };
        providers.push(provider);
    }
    ATTEMPTS.with(|attempts| {
        attempts.borrow_mut().insert(attempt, Attempt::Promise { _providers: providers.clone(), ended });
    });
    started.send(FileEvent::DragStarted { attempt, offer, roots: roots.iter().map(|root| root.id).collect() });
    Some(providers)
}

pub fn hold_local(attempt: DragAttempt, selection: ReceivedSelection) -> bool {
    let admitted = EXPORTS.with(|exports| {
        let exports = exports.borrow();
        exports.contains_key(&selection.transfer) || exports.len() < 32
    });
    if admitted {
        ATTEMPTS.with(|attempts| {
            attempts.borrow_mut().insert(attempt, Attempt::Local { selection });
        });
    }
    admitted
}

pub fn revoke_exports(transfer: TransferId) {
    EXPORTS.with(|exports| { exports.borrow_mut().remove(&transfer); });
}

pub fn session_ended(attempt: DragAttempt, outcome: DragOutcome) {
    let held = ATTEMPTS.with(|attempts| attempts.borrow_mut().remove(&attempt));
    match held {
        Some(Attempt::Promise { ended, _providers }) => {
            ended.send(FileEvent::DragEnded { attempt, outcome });
            drop(_providers);
        }
        Some(Attempt::Local { selection }) if outcome == DragOutcome::Copied => {
            EXPORTS.with(|exports| { exports.borrow_mut().insert(selection.transfer, selection); });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("splice-promise-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn payload() -> Vec<u8> {
        (0..(CHUNK * 2 + 777)).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn publish_copies_exactly_and_leaves_no_staging() {
        let dir = workspace("ok");
        let src = dir.join("src.bin");
        std::fs::write(&src, payload()).unwrap();
        let dest = dir.join("dest.bin");
        let (writer, _rx) = PromiseWriter::channel();
        assert_eq!(publish(&src, &dest, DragAttempt(1), 0, &writer.cancellation()).unwrap(), dest);
        assert_eq!(std::fs::read(&src).unwrap(), std::fs::read(&dest).unwrap());
        assert!(!staging_path(&dest, DragAttempt(1), 0).unwrap().exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn publish_preserves_executable_bits_and_modification_time() {
        let dir = workspace("metadata");
        let source = dir.join("script");
        std::fs::write(&source, b"exit 0\n").unwrap();
        let file = std::fs::File::open(&source).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o751)).unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_123);
        file.set_times(std::fs::FileTimes::new().set_modified(modified)).unwrap();
        let destination = dir.join("received-script");
        publish(&source, &destination, DragAttempt(99), 0, &PromiseCancellation::default()).unwrap();
        let metadata = std::fs::metadata(&destination).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o751);
        assert_eq!(metadata.modified().unwrap(), modified);
        assert_eq!(std::fs::read(&destination).unwrap(), b"exit 0\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn maximum_length_destination_name_fits_the_staging_scheme() {
        let dir = workspace("long-name");
        let source = dir.join("source");
        std::fs::write(&source, b"content").unwrap();
        let destination = dir.join(format!("{}.txt", "x".repeat(251)));
        publish(&source, &destination, DragAttempt(100), 0, &PromiseCancellation::default()).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"content");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publish_never_replaces_an_existing_destination() {
        let dir = workspace("collision");
        let src = dir.join("src.bin");
        std::fs::write(&src, payload()).unwrap();
        let dest = dir.join("dest.bin");
        std::fs::write(&dest, b"keep me").unwrap();
        let (writer, _rx) = PromiseWriter::channel();
        let err = publish(&src, &dest, DragAttempt(2), 0, &writer.cancellation()).unwrap_err();
        assert!(err.contains("publishing"), "{err}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
        assert!(!staging_path(&dest, DragAttempt(2), 0).unwrap().exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn publish_rejects_directories_and_missing_sources_without_touching_destination() {
        let dir = workspace("error");
        let dest = dir.join("dest.bin");
        let (writer, _rx) = PromiseWriter::channel();
        let err = publish(&dir, &dest, DragAttempt(3), 0, &writer.cancellation()).unwrap_err();
        assert!(err.contains("not a regular file"));
        let err = publish(&dir.join("missing"), &dest, DragAttempt(3), 1, &writer.cancellation()).unwrap_err();
        assert!(err.contains("unreadable"));
        assert!(!dest.exists());
        assert!(std::fs::read_dir(&dir).unwrap().count() == 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn publish_rejects_a_named_pipe_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let dir = workspace("fifo");
        let source = dir.join("source");
        let name = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let destination = dir.join("destination");
        let (done, result) = std::sync::mpsc::channel();
        let read_source = source.clone();
        let target = destination.clone();
        let worker = std::thread::spawn(move || {
            let _ = done.send(publish(&read_source, &target, DragAttempt(101), 0, &PromiseCancellation::default()));
        });
        let outcome = result.recv_timeout(std::time::Duration::from_secs(5));
        if outcome.is_err() {
            let _unblock = std::fs::OpenOptions::new().read(true).write(true).open(&source);
            worker.join().unwrap();
            std::fs::remove_dir_all(dir).unwrap();
            panic!("a non-regular source blocked the promise writer");
        }
        worker.join().unwrap();
        assert!(outcome.unwrap().unwrap_err().contains("not a regular file"));
        assert!(!destination.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelled_writer_stops_the_copy_and_removes_staging() {
        let dir = workspace("cancel");
        let src = dir.join("src.bin");
        std::fs::write(&src, payload()).unwrap();
        let dest = dir.join("dest.bin");
        let (writer, _rx) = PromiseWriter::channel();
        writer.cancel();
        let err = publish(&src, &dest, DragAttempt(4), 0, &writer.cancellation()).unwrap_err();
        assert!(err.contains("cancelled"));
        assert!(!dest.exists());
        assert!(!staging_path(&dest, DragAttempt(4), 0).unwrap().exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_staging_file_blocks_rather_than_truncates() {
        let dir = workspace("stale");
        let src = dir.join("src.bin");
        std::fs::write(&src, payload()).unwrap();
        let dest = dir.join("dest.bin");
        let staging = staging_path(&dest, DragAttempt(5), 0).unwrap();
        std::fs::write(&staging, b"someone else's partial").unwrap();
        let (writer, _rx) = PromiseWriter::channel();
        let err = publish(&src, &dest, DragAttempt(5), 0, &writer.cancellation()).unwrap_err();
        assert!(err.contains("staging"), "{err}");
        assert_eq!(std::fs::read(&staging).unwrap(), b"someone else's partial");
        assert!(!dest.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_types_come_from_extensions() {
        assert_eq!(file_type_for("notes.txt"), "public.plain-text");
        assert_eq!(file_type_for("photo.png"), "public.png");
        assert_eq!(file_type_for("archive"), GENERIC_FILE_TYPE);
    }

    #[test]
    fn cancellation_after_fsync_removes_staging_without_publication() {
        let dir = workspace("after-sync");
        let staging = dir.join("staging");
        let destination = dir.join("destination");
        let mut output = std::fs::File::create(&staging).unwrap();
        output.write_all(b"verified").unwrap();
        output.sync_all().unwrap();
        drop(output);
        let (writer, _reply) = PromiseWriter::channel();
        writer.cancel();
        let result = publish_staging(&staging, &destination, &writer.cancellation());
        assert!(result.unwrap_err().contains("cancelled"));
        assert!(!staging.exists());
        assert!(!destination.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancellation_after_publication_is_too_late() {
        let dir = workspace("after-publication");
        let staging = dir.join("staging");
        let destination = dir.join("destination");
        std::fs::write(&staging, b"verified").unwrap();
        let (writer, _reply) = PromiseWriter::channel();
        publish_staging(&staging, &destination, &writer.cancellation()).unwrap();
        writer.cancel();
        assert!(!writer.is_cancelled());
        assert_eq!(std::fs::read(&destination).unwrap(), b"verified");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn attempt_retirement_waits_for_every_owner_and_releases_admission() {
        let (tx, mut events) = tokio::sync::mpsc::channel(1);
        let outbox = super::super::files::EventOutbox::new(tx, &tokio::runtime::Handle::current());
        let admission = Arc::new(tokio::sync::Semaphore::new(1));
        let lease = Arc::new(AttemptLease {
            attempt: DragAttempt(44),
            retirement: outbox.reserve(1).unwrap().pop(),
            _admission: admission.clone().try_acquire_owned().unwrap(),
        });
        let provider = lease.clone();
        let callback = lease.clone();
        drop(lease);
        drop(provider);
        assert!(events.try_recv().is_err());
        assert!(admission.clone().try_acquire_owned().is_err());
        drop(callback);
        assert!(matches!(events.recv().await, Some(FileEvent::DragRetired { attempt: DragAttempt(44) })));
        assert!(admission.try_acquire_owned().is_ok());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn clear_revokes_idle_exports_but_preserves_active_drag_and_other_receipts() {
        let transfer = TransferId([8; 16]);
        let pin: Access = Arc::new(());
        let selection = ReceivedSelection::new(transfer, Vec::new(), Some(pin.clone()));
        assert!(hold_local(DragAttempt(80), selection.clone()));
        session_ended(DragAttempt(80), DragOutcome::Copied);
        assert!(hold_local(DragAttempt(81), selection.clone()));
        assert!(hold_local(DragAttempt(82), ReceivedSelection::new(TransferId([9; 16]), Vec::new(), None)));
        session_ended(DragAttempt(82), DragOutcome::Copied);
        drop(selection);
        assert_eq!(Arc::strong_count(&pin), 3);
        revoke_exports(transfer);
        assert_eq!(Arc::strong_count(&pin), 2);
        assert_eq!(EXPORTS.with(|exports| exports.borrow().len()), 1);
        session_ended(DragAttempt(81), DragOutcome::Cancelled);
        assert_eq!(Arc::strong_count(&pin), 1);
        revoke_exports(TransferId([9; 16]));
    }

}
