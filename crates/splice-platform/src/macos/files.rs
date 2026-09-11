use super::shelf;
use crate::file_shelf::{
    FileAdapter, FileEvent, FileShelf, ReceiveIntent, ReceivedSelection, ShelfSnapshot, SourceLease,
    EVENT_QUEUE_CAPACITY,
};
use crate::{PlatformError, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::ClassType;
use objc2_app_kit::{NSPasteboard, NSPasteboardURLReadingFileURLsOnlyKey, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc::Sender, Notify};

const RESERVATION_CAPACITY: usize = EVENT_QUEUE_CAPACITY * 2;

#[derive(Clone, Copy)]
enum EventClass {
    Action = 0,
    Control = 1,
    Reserved = 2,
}

#[derive(Default)]
struct EventState {
    events: VecDeque<(FileEvent, EventClass)>,
    queued: [usize; 3],
    reserved: usize,
    clipboard: Option<FileEvent>,
    dropped: u64,
    closed: bool,
}

pub struct EventOutbox {
    tx: Sender<FileEvent>,
    state: Mutex<EventState>,
    ready: Notify,
}

pub struct EventReservation {
    outbox: Arc<EventOutbox>,
    available: bool,
}

impl EventReservation {
    pub fn send(mut self, event: FileEvent) {
        let mut state = self.outbox.state.lock();
        state.reserved -= 1;
        self.available = false;
        if state.closed || self.outbox.tx.is_closed() { return; }
        state.events.push_back((event, EventClass::Reserved));
        state.queued[EventClass::Reserved as usize] += 1;
        drop(state);
        self.outbox.ready.notify_one();
    }
}

impl Drop for EventReservation {
    fn drop(&mut self) {
        if self.available {
            self.outbox.state.lock().reserved -= 1;
        }
    }
}

impl EventOutbox {
    pub(super) fn new(tx: Sender<FileEvent>, runtime: &tokio::runtime::Handle) -> Arc<Self> {
        let outbox = Arc::new(Self { tx, state: Mutex::new(EventState::default()), ready: Notify::new() });
        let weak = Arc::downgrade(&outbox);
        runtime.spawn(async move {
            while let Some(outbox) = weak.upgrade() {
                tokio::select! {
                    _ = outbox.tx.closed() => { outbox.close(); break; },
                    _ = outbox.ready.notified() => {}
                }
                loop {
                    let permit = match outbox.tx.reserve().await {
                        Ok(permit) => permit,
                        Err(_) => { outbox.close(); return; },
                    };
                    let event = {
                        let mut state = outbox.state.lock();
                        state.clipboard.take().or_else(|| {
                            (state.dropped > 0).then(|| FileEvent::Overflow { dropped: std::mem::take(&mut state.dropped) })
                        }).or_else(|| {
                            let (event, class) = state.events.pop_front()?;
                            state.queued[class as usize] -= 1;
                            Some(event)
                        })
                    };
                    match event {
                        Some(event) => { permit.send(event); }
                        None => break,
                    }
                }
            }
        });
        outbox
    }

    fn close(&self) {
        let (events, clipboard) = {
            let mut state = self.state.lock();
            state.closed = true;
            state.queued = [0; 3];
            (std::mem::take(&mut state.events), state.clipboard.take())
        };
        drop(events);
        drop(clipboard);
    }

    fn emit(&self, event: FileEvent) -> bool {
        if self.tx.is_closed() {
            return false;
        }
        let mut state = self.state.lock();
        if state.closed { return false; }
        if matches!(event, FileEvent::ClipboardInvalidated { .. } | FileEvent::ClipboardChanged { .. }) {
            state.clipboard = Some(event);
        } else {
            let control = matches!(event, FileEvent::Cancel { .. } | FileEvent::ClearReceived { .. } | FileEvent::Dismiss { .. });
            if control && state.events.iter().any(|(pending, _)| same_control(pending, &event)) {
                return true;
            }
            let class = if control { EventClass::Control } else { EventClass::Action };
            if state.queued[class as usize] >= EVENT_QUEUE_CAPACITY {
                state.dropped = state.dropped.saturating_add(1);
                drop(state);
                self.ready.notify_one();
                return false;
            }
            state.events.push_back((event, class));
            state.queued[class as usize] += 1;
        }
        drop(state);
        self.ready.notify_one();
        true
    }

    pub(super) fn reserve(self: &Arc<Self>, count: usize) -> Option<Vec<EventReservation>> {
        let mut state = self.state.lock();
        if state.closed || self.tx.is_closed() || count > RESERVATION_CAPACITY.saturating_sub(state.queued[EventClass::Reserved as usize] + state.reserved) {
            state.dropped = state.dropped.saturating_add(1);
            drop(state);
            self.ready.notify_one();
            return None;
        }
        state.reserved += count;
        Some((0..count).map(|_| EventReservation { outbox: self.clone(), available: true }).collect())
    }
}

fn same_control(a: &FileEvent, b: &FileEvent) -> bool {
    match (a, b) {
        (FileEvent::Cancel { transfer: a }, FileEvent::Cancel { transfer: b })
        | (FileEvent::ClearReceived { transfer: a }, FileEvent::ClearReceived { transfer: b }) => a == b,
        (FileEvent::Dismiss { offer: a }, FileEvent::Dismiss { offer: b }) => a == b,
        _ => false,
    }
}

pub struct ShelfCtx {
    outbox: Arc<EventOutbox>,
    own_change: Arc<Mutex<i64>>,
    pub staging_dir: PathBuf,
    attempt_seq: AtomicU64,
    receive_seq: AtomicU64,
    pending: Mutex<Option<ShelfSnapshot>>,
    sync_scheduled: AtomicBool,
    rejection_scheduled: AtomicBool,
    published: Mutex<Option<(u64, ReceivedSelection)>>,
    runtime: tokio::runtime::Handle,
    pub attempts: Arc<tokio::sync::Semaphore>,
    pub imports: Arc<tokio::sync::Semaphore>,
}

impl ShelfCtx {
    pub fn emit(self: &Arc<Self>, event: FileEvent) -> bool {
        let accepted = self.outbox.emit(event);
        if !accepted && !self.rejection_scheduled.swap(true, Ordering::AcqRel) {
            let ctx = self.clone();
            on_main(move || {
                ctx.rejection_scheduled.store(false, Ordering::Release);
                shelf::with_ui(&ctx, |ui| ui.set_status("File service is busy; try the action again"));
            });
        }
        accepted
    }

    pub fn reserve_events(&self, count: usize) -> Option<Vec<EventReservation>> {
        self.outbox.reserve(count)
    }

    pub fn next_attempt(&self) -> crate::file_shelf::DragAttempt {
        crate::file_shelf::DragAttempt(self.attempt_seq.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub fn receive_intent(&self) -> (ReceiveIntent, u64) {
        let _publication = self.own_change.lock();
        let intent = ReceiveIntent(self.receive_seq.fetch_add(1, Ordering::AcqRel) + 1);
        let generation = objc2::rc::autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount()) as u64;
        (intent, generation)
    }

    pub fn background(&self, work: impl FnOnce() + Send + 'static) {
        self.runtime.spawn_blocking(work);
    }

    pub fn after(&self, delay: Duration, work: impl FnOnce() + Send + 'static) {
        self.runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            work();
        });
    }

    pub fn release_publication_if_replaced(&self, current_generation: u64) {
        let mut published = self.published.lock();
        if published.as_ref().is_some_and(|(generation, _)| *generation != current_generation) {
            *published = None;
        }
    }
}

pub fn on_main(work: impl FnOnce() + Send + 'static) {
    dispatch2::DispatchQueue::main().exec_async(work);
}

pub struct MacFileShelf {
    ctx: Arc<ShelfCtx>,
}

pub fn create(data_dir: &Path, own_change: Arc<Mutex<i64>>) -> (FileAdapter, Arc<ShelfCtx>) {
    let (tx, events) = tokio::sync::mpsc::channel(EVENT_QUEUE_CAPACITY);
    let ctx = Arc::new(ShelfCtx {
        outbox: EventOutbox::new(tx, &tokio::runtime::Handle::current()),
        own_change,
        staging_dir: data_dir.join("promised-imports"),
        attempt_seq: AtomicU64::new(0),
        receive_seq: AtomicU64::new(0),
        pending: Mutex::new(None),
        sync_scheduled: AtomicBool::new(false),
        rejection_scheduled: AtomicBool::new(false),
        published: Mutex::new(None),
        runtime: tokio::runtime::Handle::current(),
        attempts: Arc::new(tokio::sync::Semaphore::new(32)),
        imports: Arc::new(tokio::sync::Semaphore::new(8)),
    });
    let shelf = Arc::new(MacFileShelf { ctx: ctx.clone() });
    let boot = ctx.clone();
    on_main(move || shelf::with_ui(&boot, |_| {}));
    (FileAdapter { shelf, events }, ctx)
}

impl FileShelf for MacFileShelf {
    fn sync(&self, snapshot: ShelfSnapshot) {
        *self.ctx.pending.lock() = Some(snapshot);
        if self.ctx.sync_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let ctx = self.ctx.clone();
        on_main(move || {
            ctx.sync_scheduled.store(false, Ordering::Release);
            let Some(snapshot) = ctx.pending.lock().take() else { return };
            shelf::with_ui(&ctx, |ui| ui.apply(snapshot));
        });
    }

    fn set_visible(&self, visible: bool) {
        let ctx = self.ctx.clone();
        on_main(move || shelf::with_ui(&ctx, |ui| ui.set_visible_by_user(visible)));
    }

    fn set_edge_targets(&self, targets: Vec<crate::file_shelf::EdgeTarget>) {
        let ctx = self.ctx.clone();
        on_main(move || shelf::with_ui(&ctx, |ui| ui.set_edge_targets(targets)));
    }

    fn publish_clipboard_files(
        &self,
        selection: ReceivedSelection,
        generation: u64,
        intent: ReceiveIntent,
    ) -> Result<u64> {
        if selection.paths.is_empty() {
            return Err(PlatformError::Unavailable("no received files to publish".into()));
        }
        let urls: Vec<Retained<NSURL>> = selection.paths.iter().map(|p| file_url(p)).collect();
        let mut own_change = self.ctx.own_change.lock();
        let mut published = self.ctx.published.lock();
        if self.ctx.receive_seq.load(Ordering::Acquire) != intent.0 {
            return Err(PlatformError::Unavailable(
                "a newer Receive was chosen; this receipt stays in the shelf".into(),
            ));
        }
        objc2::rc::autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            let current = pb.changeCount() as u64;
            if current != generation {
                return Err(PlatformError::Unavailable(format!(
                    "clipboard changed while receiving (generation {generation} is now {current}); the files stay in the shelf"
                )));
            }
            let writers: Vec<&ProtocolObject<dyn NSPasteboardWriting>> =
                urls.iter().map(|u| ProtocolObject::from_ref(&**u)).collect();
            *published = None;
            pb.clearContents();
            let accepted = pb.writeObjects(&NSArray::from_slice(&writers));
            let count = pb.changeCount() as i64;
            *own_change = count;
            self.ctx.emit(FileEvent::ClipboardInvalidated { generation: count as u64 });
            if !accepted {
                return Err(PlatformError::Unavailable("NSPasteboard refused the file URLs".into()));
            }
            *published = Some((count as u64, selection));
            Ok(count as u64)
        })
    }

    fn reveal(&self, paths: Vec<PathBuf>) {
        on_main(move || reveal_in_finder(&paths));
    }
}

pub fn reveal_in_finder(paths: &[PathBuf]) {
    let urls: Vec<Retained<NSURL>> = paths.iter().map(|p| file_url(p)).collect();
    if urls.is_empty() {
        return;
    }
    objc2_app_kit::NSWorkspace::sharedWorkspace()
        .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&urls));
}

pub fn file_url(path: &Path) -> Retained<NSURL> {
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), is_dir)
}

pub struct FileSelection {
    pub paths: Vec<PathBuf>,
    pub lease: Arc<SourceLease>,
}

pub fn read_file_urls(pb: &NSPasteboard) -> Option<FileSelection> {
    let classes = NSArray::from_slice(&[NSURL::class()]);
    let options = NSDictionary::from_slices(
        &[unsafe { NSPasteboardURLReadingFileURLsOnlyKey }],
        &[&*NSNumber::new_bool(true) as &AnyObject],
    );
    let objects = unsafe { pb.readObjectsForClasses_options(&classes, Some(&options)) }?;
    let urls: Vec<Retained<NSURL>> = (0..objects.count())
        .filter_map(|i| {
            let object: Retained<AnyObject> = objects.objectAtIndex(i);
            object.downcast::<NSURL>().ok().filter(|u| u.isFileURL())
        })
        .collect();
    selection_from_urls(urls)
}

pub fn selection_from_urls(urls: Vec<Retained<NSURL>>) -> Option<FileSelection> {
    let paths: Vec<PathBuf> = urls
        .iter()
        .filter_map(|u| u.path().map(|p| PathBuf::from(p.to_string())))
        .filter(|p| p.is_absolute())
        .collect();
    if paths.is_empty() || paths.len() != urls.len() {
        return None;
    }
    let scoped: Vec<Retained<NSURL>> = urls
        .into_iter()
        .filter(|u| unsafe { u.startAccessingSecurityScopedResource() })
        .collect();
    let lease = if scoped.is_empty() {
        SourceLease::none()
    } else {
        SourceLease::new(move || {
            for url in &scoped {
                unsafe { url.stopAccessingSecurityScopedResource() };
            }
        })
    };
    Some(FileSelection { paths, lease })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_shelf::{DragAttempt, DragOutcome, TransferId};

    fn action() -> FileEvent {
        FileEvent::Retry { transfer: TransferId([1; 16]) }
    }

    #[tokio::test]
    async fn saturation_preserves_reserved_lifecycle_controls_and_latest_invalidation() {
        let (tx, mut events) = tokio::sync::mpsc::channel(1);
        tx.try_send(action()).unwrap();
        let outbox = EventOutbox::new(tx, &tokio::runtime::Handle::current());
        let mut terminal = outbox.reserve(3).unwrap();
        for _ in 0..EVENT_QUEUE_CAPACITY {
            assert!(outbox.emit(action()));
        }
        assert!(!outbox.emit(action()));
        assert!(outbox.emit(FileEvent::Cancel { transfer: TransferId([2; 16]) }));
        assert!(outbox.emit(FileEvent::ClearReceived { transfer: TransferId([3; 16]) }));
        assert!(outbox.emit(FileEvent::ClipboardInvalidated { generation: 10 }));
        assert!(outbox.emit(FileEvent::ClipboardInvalidated { generation: 11 }));
        terminal.pop().unwrap().send(FileEvent::PromiseFinished { attempt: DragAttempt(1), root: 5, result: Err("cancelled".into()) });
        terminal.pop().unwrap().send(FileEvent::DragEnded { attempt: DragAttempt(1), outcome: DragOutcome::Cancelled });
        terminal.pop().unwrap().send(FileEvent::DragRetired { attempt: DragAttempt(1) });
        events.recv().await.unwrap();
        assert!(matches!(events.recv().await, Some(FileEvent::ClipboardInvalidated { generation: 11 })));
        assert!(matches!(events.recv().await, Some(FileEvent::Overflow { dropped: 1 })));
        let mut observed = Vec::new();
        for _ in 0..EVENT_QUEUE_CAPACITY + 5 {
            observed.push(tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().unwrap());
        }
        assert!(matches!(observed[EVENT_QUEUE_CAPACITY], FileEvent::Cancel { .. }));
        assert!(matches!(observed[EVENT_QUEUE_CAPACITY + 1], FileEvent::ClearReceived { .. }));
        assert!(matches!(observed[EVENT_QUEUE_CAPACITY + 2], FileEvent::PromiseFinished { root: 5, .. }));
        assert!(matches!(observed[EVENT_QUEUE_CAPACITY + 3], FileEvent::DragEnded { .. }));
        assert!(matches!(observed[EVENT_QUEUE_CAPACITY + 4], FileEvent::DragRetired { .. }));
    }

    #[tokio::test]
    async fn a_previous_maximum_drag_does_not_block_its_replacement() {
        let (tx, _events) = tokio::sync::mpsc::channel(1);
        let outbox = EventOutbox::new(tx, &tokio::runtime::Handle::current());
        let count = splice_proto::files::MAX_ROOTS + 3;
        let previous = outbox.reserve(count).unwrap();
        let next = outbox.reserve(count).unwrap();
        drop(previous);
        drop(next);
        assert_eq!(outbox.state.lock().reserved, 0);
    }

    #[tokio::test]
    async fn latest_clipboard_content_replaces_its_pending_invalidation() {
        let (tx, mut events) = tokio::sync::mpsc::channel(1);
        tx.try_send(action()).unwrap();
        let outbox = EventOutbox::new(tx, &tokio::runtime::Handle::current());
        assert!(outbox.emit(FileEvent::ClipboardInvalidated { generation: 42 }));
        assert!(outbox.emit(FileEvent::ClipboardChanged {
            generation: 42, mimes: vec!["text/plain;charset=utf-8".into()], inline_text: Some("new copy".into()),
        }));
        events.recv().await.unwrap();
        assert!(matches!(events.recv().await, Some(FileEvent::ClipboardChanged { generation: 42, inline_text: Some(text), .. }) if text == "new copy"));
    }

    #[tokio::test]
    async fn rejected_capture_still_delivers_invalidation_and_releases_source_scope() {
        let (tx, mut events) = tokio::sync::mpsc::channel(1);
        tx.try_send(action()).unwrap();
        let outbox = EventOutbox::new(tx, &tokio::runtime::Handle::current());
        let reservations = outbox.reserve(RESERVATION_CAPACITY).unwrap();
        for _ in 0..EVENT_QUEUE_CAPACITY { assert!(outbox.emit(action())); }
        let released = Arc::new(AtomicBool::new(false));
        let release = released.clone();
        assert!(outbox.emit(FileEvent::ClipboardInvalidated { generation: 99 }));
        assert!(!outbox.emit(FileEvent::ClipboardFiles {
            generation: 99,
            paths: vec![PathBuf::from("/scope/file")],
            lease: SourceLease::new(move || { release.store(true, Ordering::Release); }),
        }));
        assert!(released.load(Ordering::Acquire));
        assert!(outbox.reserve(1).is_none());
        events.recv().await.unwrap();
        assert!(matches!(events.recv().await, Some(FileEvent::ClipboardInvalidated { generation: 99 })));
        assert!(matches!(events.recv().await, Some(FileEvent::Overflow { dropped: 2 })));
        drop(reservations);
        assert!(outbox.reserve(RESERVATION_CAPACITY).is_some());
    }
    #[tokio::test]
    async fn receiver_shutdown_drops_queued_reply_owners_and_late_reserved_events() {
        let (tx, mut events) = tokio::sync::mpsc::channel(1);
        tx.try_send(action()).unwrap();
        let outbox = EventOutbox::new(tx, &tokio::runtime::Handle::current());
        let (writer, reply) = crate::file_shelf::PromiseWriter::channel();
        let cancellation = writer.cancellation();
        assert!(outbox.emit(FileEvent::PromiseRequested {
            attempt: DragAttempt(1), root: 0, destination: PathBuf::from("/destination"), writer,
        }));
        let late = outbox.reserve(1).unwrap().pop().unwrap();
        events.close();
        let result = tokio::task::spawn_blocking(move || reply.recv_timeout(Duration::from_secs(2))).await.unwrap();
        assert!(result.unwrap().unwrap_err().contains("dropped"));
        assert!(!cancellation.is_cancelled());
        let (writer, reply) = crate::file_shelf::PromiseWriter::channel();
        late.send(FileEvent::PromiseRequested {
            attempt: DragAttempt(1), root: 1, destination: PathBuf::from("/destination"), writer,
        });
        assert!(reply.try_recv().unwrap().is_err());
        assert!(outbox.state.lock().events.is_empty());
        assert_eq!(outbox.state.lock().reserved, 0);
    }

    #[tokio::test]
    async fn terminal_reservations_do_not_consume_action_or_control_admission() {
        let (tx, _events) = tokio::sync::mpsc::channel(1);
        let outbox = EventOutbox::new(tx, &tokio::runtime::Handle::current());
        let held = outbox.reserve(RESERVATION_CAPACITY).unwrap();
        for _ in 0..EVENT_QUEUE_CAPACITY { assert!(outbox.emit(action())); }
        assert!(!outbox.emit(action()));
        for _ in 0..EVENT_QUEUE_CAPACITY * 2 {
            assert!(outbox.emit(FileEvent::Cancel { transfer: TransferId([2; 16]) }));
        }
        assert_eq!(outbox.state.lock().queued[EventClass::Control as usize], 1);
        assert_eq!(outbox.state.lock().reserved, RESERVATION_CAPACITY);
        drop(held);
    }

}
