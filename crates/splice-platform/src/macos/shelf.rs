use super::files::{self, on_main, FileSelection, ShelfCtx};
use super::promise::{self, PromisedRoot};
use crate::file_shelf::{
    format_bytes, summarize_names, summarize_roots, DragAttempt, DragOutcome, FileEvent, OfferId,
    OfferState, ReceivedSelection, Recipient, ShelfOffer, ShelfSnapshot, ShelfTransfer, SourceGesture,
    SourceLease, TransferDirection, TransferId, TransferState,
};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSBorderType, NSBox, NSBoxType, NSButton, NSColor,
    NSControlSize, NSDragOperation, NSDraggingContext, NSDraggingDestination, NSDraggingInfo,
    NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSFilePromiseReceiver, NSFont,
    NSImage, NSLineBreakMode, NSModalResponseOK, NSOpenPanel, NSPanel, NSPasteboard,
    NSPasteboardTypeFileURL, NSPasteboardWriting, NSPopUpButton, NSProgressIndicator,
    NSProgressIndicatorStyle, NSScreen, NSScrollView, NSTextField, NSTitlePosition, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSCopying, NSDictionary, NSError, NSOperationQueue, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use parking_lot::Mutex;
use splice_proto::MachineId;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WIDTH: f64 = 392.0;
const MARGIN: f64 = 12.0;
const HEADER_H: f64 = 96.0;
const ROW_H: f64 = 78.0;
const EMPTY_H: f64 = 44.0;
const STATUS_H: f64 = 22.0;
const MAX_VISIBLE_ROWS: usize = 5;
const DRAG_THRESHOLD: f64 = 4.0;
const STATUS_HOLD: Duration = Duration::from_secs(8);
const SCREEN_MARGIN: f64 = 16.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Receive,
    Copy,
    Save,
    Reveal,
    Cancel,
    Retry,
    Clear,
    Dismiss,
}

impl Action {
    fn title(self) -> &'static str {
        match self {
            Action::Receive => "Receive",
            Action::Copy => "Copy",
            Action::Save => "Save…",
            Action::Reveal => "Reveal",
            Action::Cancel => "Cancel",
            Action::Retry => "Retry",
            Action::Clear => "Clear",
            Action::Dismiss => "Dismiss",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Action::Receive => "Download the files, then put them on the clipboard for a normal Paste",
            Action::Copy => "Put these received files on the clipboard",
            Action::Save => "Choose a folder and download the files into it",
            Action::Reveal => "Show the files in Finder",
            Action::Cancel => "Stop this transfer",
            Action::Retry => "Start this transfer again from the beginning",
            Action::Clear => "Clear received copy and revoke earlier dragged file URLs; apps still reading those files may fail",
            Action::Dismiss => "Remove this offer from the shelf",
        }
    }

    const ALL: [Action; 8] = [
        Action::Receive,
        Action::Copy,
        Action::Save,
        Action::Reveal,
        Action::Cancel,
        Action::Retry,
        Action::Clear,
        Action::Dismiss,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragMode {
    LocalUrls(ReceivedSelection),
    Promise(Vec<(u32, String)>),
    Blocked(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    Offer(OfferId),
    Transfer(TransferId),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RowModel {
    pub key: RowKey,
    pub offer: Option<OfferId>,
    pub transfer: Option<TransferId>,
    pub title: String,
    pub subtitle: String,
    pub progress: Option<f64>,
    pub actions: Vec<Action>,
    pub drag: DragMode,
    pub paths: Vec<PathBuf>,
    pub incoming: bool,
    pub indented: bool,
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn expiry_text(expires_unix_ms: u64, now_unix_ms: u64) -> String {
    let remaining = expires_unix_ms.saturating_sub(now_unix_ms);
    let minutes = remaining / 60_000;
    if minutes >= 120 {
        format!("expires in {} h", minutes / 60)
    } else if minutes >= 1 {
        format!("expires in {minutes} min")
    } else {
        "expires now".to_string()
    }
}

fn progress_of(transfer: &ShelfTransfer) -> Option<f64> {
    if transfer.total_bytes == 0 {
        return Some(0.0);
    }
    Some((transfer.bytes as f64 / transfer.total_bytes as f64).clamp(0.0, 1.0))
}

fn promised_roots(offer: &ShelfOffer) -> Vec<(u32, String)> {
    offer.roots().map(|r| (r.id, r.name.clone())).collect()
}

fn item_count(n: usize) -> String {
    if n == 1 {
        "1 item".to_string()
    } else {
        format!("{n} items")
    }
}

fn offer_row(snapshot: &ShelfSnapshot, offer: &ShelfOffer, now_unix_ms: u64) -> RowModel {
    let incoming = offer.owner != snapshot.self_id;
    let peer = if incoming { snapshot.name_of(&offer.owner) } else { snapshot.name_of(&offer.recipient) };
    let size = format_bytes(offer.total_bytes);
    let count = item_count(offer.roots().count());
    let (subtitle, actions, drag) = match (incoming, offer.state) {
        (true, OfferState::Available) => (
            format!("From {peer} · {count} · {size} · {}", expiry_text(offer.expires_unix_ms, now_unix_ms)),
            vec![Action::Receive, Action::Save, Action::Dismiss],
            if offer.has_directory_root() {
                DragMode::Blocked("Folders arrive with Receive or Save first, then drag them from the receipt")
            } else {
                DragMode::Promise(promised_roots(offer))
            },
        ),
        (true, OfferState::Revoked) => (
            format!("Withdrawn by {peer}"),
            vec![Action::Dismiss],
            DragMode::Blocked("This offer was withdrawn"),
        ),
        (true, OfferState::Expired) => (
            format!("Expired · from {peer}"),
            vec![Action::Dismiss],
            DragMode::Blocked("This offer expired"),
        ),
        (false, OfferState::Available) => (
            format!("Offered to {peer} · {count} · {size} · waiting for them to receive"),
            vec![Action::Dismiss],
            DragMode::Blocked("Files you offered are dragged on the other computer"),
        ),
        (false, OfferState::Revoked) => ("Withdrawn".to_string(), vec![Action::Dismiss], DragMode::Blocked("This offer was withdrawn")),
        (false, OfferState::Expired) => ("Expired".to_string(), vec![Action::Dismiss], DragMode::Blocked("This offer expired")),
    };
    RowModel {
        key: RowKey::Offer(offer.id),
        offer: Some(offer.id),
        transfer: None,
        title: summarize_roots(offer),
        subtitle,
        progress: None,
        actions,
        drag,
        paths: Vec::new(),
        incoming,
        indented: false,
    }
}

fn transfer_title(offer: Option<&ShelfOffer>, transfer: &ShelfTransfer) -> (String, Option<String>) {
    if let Some(offer) = offer {
        let all: Vec<&crate::file_shelf::ShelfEntry> = offer.roots().collect();
        if !transfer.roots.is_empty() {
            let covered: Vec<&str> = all
                .iter()
                .filter(|r| transfer.roots.contains(&r.id))
                .map(|r| r.name.as_str())
                .collect();
            let coverage = (covered.len() < all.len()).then(|| format!("{} of {}", covered.len(), item_count(all.len())));
            return (summarize_names(covered.into_iter()), coverage);
        }
        return (summarize_roots(offer), None);
    }
    let names: Vec<String> = transfer
        .paths
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    if names.is_empty() {
        ("Received files".to_string(), None)
    } else {
        (summarize_names(names.iter().map(String::as_str)), None)
    }
}

fn transfer_row(snapshot: &ShelfSnapshot, offer: Option<&ShelfOffer>, transfer: &ShelfTransfer) -> RowModel {
    let incoming = transfer.direction == TransferDirection::Receive;
    let peer = snapshot.name_of(&transfer.peer);
    let (title, coverage) = transfer_title(offer, transfer);
    let suffix = coverage.map(|c| format!(" · {c}")).unwrap_or_default();
    let size = format_bytes(transfer.total_bytes);
    let error = || transfer.error.clone().unwrap_or_else(|| "unknown error".into());
    let (subtitle, progress, actions, drag) = match (incoming, transfer.state) {
        (true, state) if state.active() => (
            format!("Receiving from {peer} · {} of {size}{suffix}", format_bytes(transfer.bytes)),
            progress_of(transfer),
            vec![Action::Cancel],
            DragMode::Blocked("Still receiving; drag once the files are ready"),
        ),
        (true, TransferState::Ready) => {
            let mut actions = vec![Action::Copy];
            if !transfer.paths.is_empty() {
                actions.push(Action::Reveal);
            }
            actions.push(Action::Clear);
            (
                format!("Received from {peer} · {size}{suffix}"),
                None,
                actions,
                match &transfer.received {
                    Some(selection) if !selection.paths.is_empty() => DragMode::LocalUrls(selection.clone()),
                    _ => DragMode::Blocked("These files are not pinned for dragging yet"),
                },
            )
        }
        (true, TransferState::Failed) => {
            let mut actions = vec![Action::Retry];
            if !transfer.paths.is_empty() {
                actions.push(Action::Reveal);
            }
            actions.push(Action::Clear);
            (format!("Failed: {}{suffix}", error()), None, actions, DragMode::Blocked("This transfer failed"))
        }
        (true, _) => (
            format!("Cancelled · from {peer}{suffix}"),
            None,
            vec![Action::Retry, Action::Clear],
            DragMode::Blocked("This transfer was cancelled"),
        ),
        (false, state) if state.active() => (
            format!("Sending to {peer} · {} of {size}{suffix}", format_bytes(transfer.bytes)),
            progress_of(transfer),
            vec![Action::Cancel],
            DragMode::Blocked("This item is being sent; drag it on the other computer"),
        ),
        (false, TransferState::Ready) => (
            format!("Sent to {peer} · {size}{suffix}"),
            None,
            Vec::new(),
            DragMode::Blocked("Files you offered are dragged on the other computer"),
        ),
        (false, TransferState::Failed) => (
            format!("Sending failed: {}{suffix}", error()),
            None,
            Vec::new(),
            DragMode::Blocked("Files you offered are dragged on the other computer"),
        ),
        (false, _) => (
            format!("Sending cancelled · to {peer}{suffix}"),
            None,
            Vec::new(),
            DragMode::Blocked("Files you offered are dragged on the other computer"),
        ),
    };
    RowModel {
        key: RowKey::Transfer(transfer.id),
        offer: offer.map(|o| o.id),
        transfer: Some(transfer.id),
        title,
        subtitle,
        progress,
        actions,
        drag,
        paths: transfer.paths.clone(),
        incoming,
        indented: offer.is_some(),
    }
}

pub fn build_rows(snapshot: &ShelfSnapshot, now_unix_ms: u64) -> Vec<RowModel> {
    let mut rows = Vec::new();
    for offer in &snapshot.offers {
        rows.push(offer_row(snapshot, offer, now_unix_ms));
        for transfer in snapshot.transfers.iter().filter(|t| t.offer == offer.id) {
            rows.push(transfer_row(snapshot, Some(offer), transfer));
        }
    }
    for transfer in &snapshot.transfers {
        if snapshot.offers.iter().any(|o| o.id == transfer.offer) {
            continue;
        }
        if transfer.direction == TransferDirection::Send && transfer.state != TransferState::Failed && !transfer.state.active() {
            continue;
        }
        rows.push(transfer_row(snapshot, None, transfer));
    }
    rows
}

pub fn drop_permitted(enabled: bool, recipient: Option<&Recipient>, source_mask: NSDragOperation, has_files: bool) -> bool {
    enabled && recipient.is_some() && has_files && source_mask.contains(NSDragOperation::Copy)
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn label(mtm: MainThreadMarker, frame: NSRect, size: f64, bold: bool, secondary: bool) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::new(), mtm);
    field.setFrame(frame);
    let font = if bold { NSFont::boldSystemFontOfSize(size) } else { NSFont::systemFontOfSize(size) };
    let color = if secondary { NSColor::secondaryLabelColor() } else { NSColor::labelColor() };
    field.setFont(Some(&font));
    field.setTextColor(Some(&color));
    field.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    field.setUsesSingleLineMode(true);
    field
}

fn backdrop(mtm: MainThreadMarker, frame: NSRect, alpha: f64, radius: f64) -> Retained<NSBox> {
    let view = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    view.setBoxType(NSBoxType::Custom);
    view.setTitlePosition(NSTitlePosition::NoTitle);
    view.setBorderWidth(0.0);
    view.setCornerRadius(radius);
    view.setContentViewMargins(NSSize::new(0.0, 0.0));
    view.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(0.11, 0.12, 0.15, alpha));
    view
}

fn small_button(mtm: MainThreadMarker, title: &str, target: &AnyObject, action: objc2::runtime::Sel) -> Retained<NSButton> {
    let button = unsafe { NSButton::buttonWithTitle_target_action(&ns(title), Some(target), Some(action), mtm) };
    button.setControlSize(NSControlSize::Small);
    button.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    button.sizeToFit();
    button
}

pub struct RowIvars {
    ctx: Arc<ShelfCtx>,
    queue: Retained<NSOperationQueue>,
    model: RefCell<RowModel>,
    title: Retained<NSTextField>,
    subtitle: Retained<NSTextField>,
    progress: Retained<NSProgressIndicator>,
    buttons: RefCell<Vec<(Action, Retained<NSButton>)>>,
    press: Cell<Option<NSPoint>>,
    attempt: Cell<Option<(DragAttempt, bool)>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpliceShelfRow"]
    #[ivars = RowIvars]
    pub struct RowView;

    unsafe impl NSObjectProtocol for RowView {}

    impl RowView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.ivars().press.set(Some(event.locationInWindow()));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().press.set(None);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some(press) = self.ivars().press.get() else { return };
            let here = event.locationInWindow();
            if (here.x - press.x).abs() < DRAG_THRESHOLD && (here.y - press.y).abs() < DRAG_THRESHOLD {
                return;
            }
            self.ivars().press.set(None);
            self.begin_drag(event);
        }

        #[unsafe(method(receive:))]
        fn receive(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            let model = iv.model.borrow();
            let (intent, clipboard_generation) = iv.ctx.receive_intent();
            match (model.transfer, model.offer) {
                (Some(transfer), _) => {
                    iv.ctx.emit(FileEvent::Republish { transfer, clipboard_generation, intent });
                }
                (None, Some(offer)) => {
                    iv.ctx.emit(FileEvent::Receive { offer, clipboard_generation, intent });
                }
                (None, None) => {}
            }
        }

        #[unsafe(method(save:))]
        fn save(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            let Some(offer) = iv.model.borrow().offer else { return };
            let ctx = iv.ctx.clone();
            choose_directory(self.mtm(), "Save Here", "Choose where Splice should save these files.", move |selection| {
                let Some(directory) = selection.paths.into_iter().next() else { return };
                ctx.emit(FileEvent::Save { offer, directory, lease: selection.lease });
            });
        }

        #[unsafe(method(reveal:))]
        fn reveal(&self, _sender: Option<&AnyObject>) {
            files::reveal_in_finder(&self.ivars().model.borrow().paths);
        }

        #[unsafe(method(cancel:))]
        fn cancel(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            if let Some(transfer) = iv.model.borrow().transfer {
                iv.ctx.emit(FileEvent::Cancel { transfer });
            }
        }

        #[unsafe(method(retry:))]
        fn retry(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            if let Some(transfer) = iv.model.borrow().transfer {
                iv.ctx.emit(FileEvent::Retry { transfer });
            }
        }

        #[unsafe(method(clear:))]
        fn clear(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            let transfer = iv.model.borrow().transfer;
            if let Some(transfer) = transfer {
                if iv.ctx.emit(FileEvent::ClearReceived { transfer }) {
                    promise::revoke_exports(transfer);
                    iv.model.borrow_mut().drag = DragMode::Blocked("This received copy is being cleared");
                }
            }
        }

        #[unsafe(method(dismiss:))]
        fn dismiss(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            if let Some(offer) = iv.model.borrow().offer {
                iv.ctx.emit(FileEvent::Dismiss { offer });
            }
        }
    }

    unsafe impl NSDraggingSource for RowView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(ignoreModifierKeysForDraggingSession:))]
        fn ignore_modifiers(&self, _session: &NSDraggingSession) -> bool {
            true
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            let iv = self.ivars();
            let Some((attempt, _promised)) = iv.attempt.take() else { return };
            let outcome = if operation.contains(NSDragOperation::Copy) { DragOutcome::Copied } else { DragOutcome::Cancelled };
            promise::session_ended(attempt, outcome);
            let ctx = iv.ctx.clone();
            with_ui(&ctx, |ui| ui.drag_finished());
        }
    }
);

impl RowView {
    fn new(mtm: MainThreadMarker, ctx: Arc<ShelfCtx>, queue: Retained<NSOperationQueue>, model: RowModel) -> Retained<Self> {
        let frame = rect(0.0, 0.0, WIDTH - 2.0 * MARGIN, ROW_H);
        let title = label(mtm, rect(10.0, 8.0, frame.size.width - 20.0, 17.0), 12.5, true, false);
        let subtitle = label(mtm, rect(10.0, 27.0, frame.size.width - 20.0, 15.0), 11.0, false, true);
        let progress = NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), rect(10.0, 44.0, frame.size.width - 20.0, 6.0));
        progress.setStyle(NSProgressIndicatorStyle::Bar);
        progress.setIndeterminate(false);
        progress.setMinValue(0.0);
        progress.setMaxValue(1.0);
        progress.setControlSize(NSControlSize::Small);
        progress.setHidden(true);
        let this = Self::alloc(mtm).set_ivars(RowIvars {
            ctx,
            queue,
            model: RefCell::new(model.clone()),
            title: title.clone(),
            subtitle: subtitle.clone(),
            progress: progress.clone(),
            buttons: RefCell::new(Vec::new()),
            press: Cell::new(None),
            attempt: Cell::new(None),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let background = backdrop(mtm, rect(0.0, 0.0, frame.size.width, ROW_H - 6.0), 0.55, 8.0);
        this.addSubview(&background);
        this.addSubview(&title);
        this.addSubview(&subtitle);
        this.addSubview(&progress);
        let target: &AnyObject = &this;
        let mut buttons = Vec::new();
        for action in Action::ALL {
            let selector = match action {
                Action::Receive | Action::Copy => sel!(receive:),
                Action::Save => sel!(save:),
                Action::Reveal => sel!(reveal:),
                Action::Cancel => sel!(cancel:),
                Action::Retry => sel!(retry:),
                Action::Clear => sel!(clear:),
                Action::Dismiss => sel!(dismiss:),
            };
            let button = small_button(mtm, action.title(), target, selector);
            button.setToolTip(Some(&ns(action.tooltip())));
            button.setHidden(true);
            this.addSubview(&button);
            buttons.push((action, button));
        }
        *this.ivars().buttons.borrow_mut() = buttons;
        this.apply(model);
        this
    }

    fn apply(&self, model: RowModel) {
        let iv = self.ivars();
        let inset = if model.indented { 18.0 } else { 0.0 };
        let width = self.frame().size.width;
        iv.title.setFrame(rect(10.0 + inset, 8.0, width - 20.0 - inset, 17.0));
        iv.subtitle.setFrame(rect(10.0 + inset, 27.0, width - 20.0 - inset, 15.0));
        iv.title.setStringValue(&ns(&model.title));
        iv.subtitle.setStringValue(&ns(&model.subtitle));
        iv.subtitle.setToolTip(Some(&ns(&model.subtitle)));
        match model.progress {
            Some(p) => {
                iv.progress.setDoubleValue(p);
                iv.progress.setHidden(false);
            }
            None => iv.progress.setHidden(true),
        }
        let mut x = 10.0 + inset;
        let y = if model.progress.is_some() { 52.0 } else { 46.0 };
        for (action, button) in iv.buttons.borrow().iter() {
            let visible = model.actions.contains(action);
            button.setHidden(!visible);
            if visible {
                let size = button.frame().size;
                button.setFrame(rect(x, y, size.width, size.height));
                x += size.width + 4.0;
            }
        }
        let hint = match &model.drag {
            DragMode::LocalUrls(_) => "Drag into a folder or app to copy the received files",
            DragMode::Promise(_) => "Drag into a folder or app; the files download when the drop is accepted",
            DragMode::Blocked(reason) => reason,
        };
        self.setToolTip(Some(&ns(hint)));
        *iv.model.borrow_mut() = model;
    }

    fn begin_drag(&self, event: &NSEvent) {
        let iv = self.ivars();
        if iv.attempt.get().is_some() {
            return;
        }
        let model = iv.model.borrow().clone();
        let location = self.convertPoint_fromView(event.locationInWindow(), None);
        let frame = rect(location.x - 16.0, location.y - 16.0, 32.0, 32.0);
        let mut items: Vec<Retained<NSDraggingItem>> = Vec::new();
        let attempt = iv.ctx.next_attempt();
        let promised = match &model.drag {
            DragMode::LocalUrls(selection) => {
                if !promise::hold_local(attempt, selection.clone()) {
                    with_ui(&iv.ctx, |ui| ui.set_status("Clear an earlier exported receipt before dragging another"));
                    return;
                }
                for (i, path) in selection.paths.iter().enumerate() {
                    let url = files::file_url(path);
                    let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
                    let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
                    let icon = NSWorkspace::sharedWorkspace().iconForFile(&ns(&path.to_string_lossy()));
                    let contents: &AnyObject = &icon;
                    unsafe { item.setDraggingFrame_contents(offset(frame, i), Some(contents)) };
                    items.push(item);
                }
                false
            }
            DragMode::Promise(roots) => {
                let Some(offer) = model.offer else { return };
                let promised: Vec<PromisedRoot> = roots.iter().map(|(id, name)| PromisedRoot { id: *id, name: name.clone() }).collect();
                let Some(providers) = promise::providers_for(&iv.ctx, attempt, offer, &promised, &iv.queue) else {
                    with_ui(&iv.ctx, |ui| ui.set_status("Finish an existing file drag before starting another"));
                    return;
                };
                let icon = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns("doc"), None);
                for (i, provider) in providers.iter().enumerate() {
                    let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&**provider);
                    let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
                    let contents: Option<&AnyObject> = icon.as_deref().map(|i| -> &AnyObject { i });
                    unsafe { item.setDraggingFrame_contents(offset(frame, i), contents) };
                    items.push(item);
                }
                true
            }
            DragMode::Blocked(reason) => {
                let ctx = iv.ctx.clone();
                let reason = *reason;
                with_ui(&ctx, |ui| ui.set_status(reason));
                return;
            }
        };
        if items.is_empty() {
            promise::session_ended(attempt, DragOutcome::Cancelled);
            return;
        }
        iv.attempt.set(Some((attempt, promised)));
        let ctx = iv.ctx.clone();
        with_ui(&ctx, |ui| ui.drag_started());
        let source: &ProtocolObject<dyn NSDraggingSource> = ProtocolObject::from_ref(self);
        let session = self.beginDraggingSessionWithItems_event_source(&NSArray::from_retained_slice(&items), event, source);
        session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    }
}

fn offset(frame: NSRect, index: usize) -> NSRect {
    let shift = (index.min(4) as f64) * 6.0;
    rect(frame.origin.x + shift, frame.origin.y + shift, frame.size.width, frame.size.height)
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpliceShelfList"]
    pub struct ListView;

    unsafe impl NSObjectProtocol for ListView {}

    impl ListView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl ListView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct DropIvars {
    ctx: Arc<ShelfCtx>,
    enabled: Cell<bool>,
    recipient: RefCell<Option<Recipient>>,
    hint: RefCell<Option<Retained<NSTextField>>>,
    import_seq: AtomicU64,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpliceShelfDropView"]
    #[ivars = DropIvars]
    pub struct DropView;

    unsafe impl NSObjectProtocol for DropView {}

    impl DropView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }

    unsafe impl NSDraggingDestination for DropView {
        #[unsafe(method(draggingEntered:))]
        fn entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            let iv = self.ivars();
            let pb = sender.draggingPasteboard();
            let permitted = drop_permitted(
                iv.enabled.get(),
                iv.recipient.borrow().as_ref(),
                sender.draggingSourceOperationMask(),
                pasteboard_has_files(&pb),
            );
            if permitted {
                let name = iv.recipient.borrow().as_ref().map(|r| r.name.clone()).unwrap_or_default();
                self.set_hint(&format!("Release to offer these files to {name}"));
                NSDragOperation::Copy
            } else {
                self.set_hint(self.refusal_reason(&pb, sender.draggingSourceOperationMask()));
                NSDragOperation::None
            }
        }

        #[unsafe(method(draggingUpdated:))]
        fn updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            let iv = self.ivars();
            let pb = sender.draggingPasteboard();
            if drop_permitted(iv.enabled.get(), iv.recipient.borrow().as_ref(), sender.draggingSourceOperationMask(), pasteboard_has_files(&pb)) {
                NSDragOperation::Copy
            } else {
                NSDragOperation::None
            }
        }

        #[unsafe(method(draggingExited:))]
        fn exited(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            self.restore_hint();
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            let iv = self.ivars();
            let pb = sender.draggingPasteboard();
            drop_permitted(iv.enabled.get(), iv.recipient.borrow().as_ref(), sender.draggingSourceOperationMask(), pasteboard_has_files(&pb))
        }

        #[unsafe(method(performDragOperation:))]
        fn perform(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            self.perform_drop(sender)
        }

        #[unsafe(method(concludeDragOperation:))]
        fn conclude(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            self.restore_hint();
        }
    }
);

impl DropView {
    fn new(mtm: MainThreadMarker, ctx: Arc<ShelfCtx>, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DropIvars {
            ctx,
            enabled: Cell::new(false),
            recipient: RefCell::new(None),
            hint: RefCell::new(None),
            import_seq: AtomicU64::new(0),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let mut types: Vec<Retained<NSString>> = NSFilePromiseReceiver::readableDraggedTypes().to_vec();
        types.push(unsafe { NSPasteboardTypeFileURL }.copy());
        this.registerForDraggedTypes(&NSArray::from_retained_slice(&types));
        this
    }

    fn perform_drop(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
        let iv = self.ivars();
        let Some(recipient) = iv.recipient.borrow().clone() else { return false };
        if !iv.enabled.get() || !sender.draggingSourceOperationMask().contains(NSDragOperation::Copy) {
            return false;
        }
        let pb = sender.draggingPasteboard();
        let selection = files::read_file_urls(&pb);
        let receivers = read_receivers(&pb);
        if receivers.is_empty() {
            let Some(selection) = selection else { return false };
            return iv.ctx.emit(FileEvent::SourceSelected {
                paths: selection.paths,
                recipient: recipient.id,
                gesture: SourceGesture::NativeDrop,
                lease: selection.lease,
            });
        }
        self.import_promised(receivers, selection, recipient)
    }

    fn set_hint(&self, text: &str) {
        if let Some(hint) = self.ivars().hint.borrow().as_ref() {
            hint.setStringValue(&ns(text));
        }
    }

    fn restore_hint(&self) {
        let iv = self.ivars();
        self.set_hint(&idle_hint(iv.enabled.get(), iv.recipient.borrow().as_ref()));
    }

    fn refusal_reason(&self, pb: &NSPasteboard, mask: NSDragOperation) -> &'static str {
        let iv = self.ivars();
        if !iv.enabled.get() {
            "File sharing is turned off"
        } else if iv.recipient.borrow().is_none() {
            "Choose a recipient before dropping files"
        } else if !pasteboard_has_files(pb) {
            "Only files and folders can be offered"
        } else if !mask.contains(NSDragOperation::Copy) {
            "Splice copies files; a move or cut is refused"
        } else {
            "This drop is not accepted"
        }
    }

    fn import_promised(&self, receivers: Vec<Retained<NSFilePromiseReceiver>>, selection: Option<FileSelection>, recipient: Recipient) -> bool {
        let iv = self.ivars();
        let import = iv.import_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let dir = iv.ctx.staging_dir.join(format!("{}-{import}", now_unix_ms()));
        if receivers.len() > 64 {
            self.set_hint("Too many promised items in this drop");
            return false;
        }
        let Ok(admission) = iv.ctx.imports.clone().try_acquire_owned() else {
            self.set_hint("Finish an existing promised import before adding another");
            return false;
        };
        let Some(mut events) = iv.ctx.reserve_events(1) else {
            self.set_hint("File service is busy; try the drop again");
            return false;
        };
        let ctx = iv.ctx.clone();
        let cleanup_ctx = ctx.clone();
        let cleanup_dir = dir.clone();
        let owned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup_owned = owned.clone();
        let cleanup = SourceLease::new(move || {
            if cleanup_owned.load(Ordering::Acquire) {
                cleanup_ctx.background(move || { let _ = std::fs::remove_dir_all(cleanup_dir); });
            }
        });
        let mut state = PendingImport::new(selection, cleanup);
        state.delivery = events.pop();
        state._admission = Some(admission);
        let abandoned_ctx = ctx.clone();
        state.on_abandon = Some(Box::new(move || {
            on_main(move || {
                IMPORTS.with(|imports| { imports.borrow_mut().remove(&import); });
                with_ui(&abandoned_ctx, |ui| ui.set_status("The promised import was abandoned before all files arrived"));
            });
        }));
        let pending = Arc::new(Mutex::new(state));
        IMPORTS.with(|imports| { imports.borrow_mut().insert(import, receivers); });
        let weak = Arc::downgrade(&pending);
        ctx.after(IMPORT_RETIREMENT, move || {
            if let Some(pending) = weak.upgrade() {
                pending.lock().abandon();
            }
        });
        let work_ctx = ctx.clone();
        ctx.background(move || {
            let result = std::fs::create_dir_all(dir.parent().expect("import directory has a parent"))
                .and_then(|_| std::fs::create_dir(&dir));
            if result.is_ok() {
                owned.store(true, Ordering::Release);
            }
            on_main(move || {
                if let Err(error) = result {
                    let mut state = pending.lock();
                    state.errors.push(format!("Cannot stage promised files: {error}"));
                    state.expected = Some(0);
                    finish_import_if_complete(&mut state, &work_ctx, &recipient, import);
                    return;
                }
                let receivers = IMPORTS.with(|imports| imports.borrow().get(&import).cloned());
                let Some(receivers) = receivers else { return };
                if pending.lock().finished { return; }
                let dest = NSURL::fileURLWithPath_isDirectory(&ns(&dir.to_string_lossy()), true);
                let block = {
                    let pending = pending.clone();
                    let ctx = work_ctx.clone();
                    let recipient = recipient.clone();
                    RcBlock::new(move |url: NonNull<NSURL>, error: *mut NSError| {
                        let mut state = pending.lock();
                        if state.finished { return; }
                        if error.is_null() {
                            match unsafe { url.as_ref() }.path() {
                                Some(path) => state.paths.push(PathBuf::from(path.to_string())),
                                None => state.errors.push("promised file arrived without a path".into()),
                            }
                        } else {
                            state.errors.push(unsafe { &*error }.localizedDescription().to_string());
                        }
                        state.done += 1;
                        finish_import_if_complete(&mut state, &ctx, &recipient, import);
                    })
                };
                let queue = NSOperationQueue::new();
                queue.setMaxConcurrentOperationCount(4);
                for receiver in &receivers {
                    unsafe {
                        receiver.receivePromisedFilesAtDestination_options_operationQueue_reader(&dest, &NSDictionary::new(), &queue, &block);
                    }
                }
                let expected: usize = receivers.iter().map(|r| r.fileNames().count()).sum();
                let mut state = pending.lock();
                state.expected = Some(expected);
                finish_import_if_complete(&mut state, &work_ctx, &recipient, import);
            });
        });
        self.set_hint("Importing promised files…");
        true
    }
}

const IMPORT_RETIREMENT: Duration = Duration::from_secs(30 * 60);

struct PendingImport {
    expected: Option<usize>,
    done: usize,
    paths: Vec<PathBuf>,
    errors: Vec<String>,
    lease: Option<Arc<SourceLease>>,
    cleanup: Arc<SourceLease>,
    delivery: Option<super::files::EventReservation>,
    _admission: Option<tokio::sync::OwnedSemaphorePermit>,
    on_abandon: Option<Box<dyn FnOnce() + Send>>,
    finished: bool,
}

impl PendingImport {
    fn new(selection: Option<FileSelection>, cleanup: Arc<SourceLease>) -> Self {
        let (paths, lease) = match selection {
            Some(s) => (s.paths, Some(s.lease)),
            None => (Vec::new(), None),
        };
        Self { expected: None, done: 0, paths, errors: Vec::new(), lease, cleanup, delivery: None, _admission: None, on_abandon: None, finished: false }
    }

    fn abandon(&mut self) {
        if self.finished { return; }
        self.finished = true;
        self.delivery.take();
        if let Some(abandon) = self.on_abandon.take() { abandon(); }
    }

    fn complete(&self) -> bool {
        !self.finished && self.expected.is_some_and(|expected| self.done >= expected)
    }
}

impl Drop for PendingImport {
    fn drop(&mut self) {
        self.abandon();
    }
}

fn finish_import_if_complete(state: &mut PendingImport, ctx: &Arc<ShelfCtx>, recipient: &Recipient, import: u64) {
    if !state.complete() {
        return;
    }
    state.finished = true;
    state.on_abandon.take();
    on_main(move || { IMPORTS.with(|imports| { imports.borrow_mut().remove(&import); }); });
    let paths = std::mem::take(&mut state.paths);
    let errors = std::mem::take(&mut state.errors);
    let source_lease = state.lease.take().unwrap_or_else(SourceLease::none);
    if !errors.is_empty() || state.expected == Some(0) {
        state.delivery.take();
        let message = if errors.is_empty() {
            "The dragged items promised no files".to_string()
        } else {
            format!("Could not import promised files: {}", errors.join("; "))
        };
        let ctx = ctx.clone();
        on_main(move || with_ui(&ctx, |ui| ui.set_status(&message)));
        return;
    }
    if let Some(delivery) = state.delivery.take() {
        delivery.send(FileEvent::SourceSelected {
            paths,
            recipient: recipient.id.clone(),
            gesture: SourceGesture::NativeDrop,
            lease: SourceLease::chain(source_lease, state.cleanup.clone()),
        });
    }
}

thread_local! {
    static IMPORTS: RefCell<HashMap<u64, Vec<Retained<NSFilePromiseReceiver>>>> = RefCell::new(HashMap::new());
    static UI: RefCell<Option<ShelfUi>> = const { RefCell::new(None) };
}

fn read_receivers(pb: &NSPasteboard) -> Vec<Retained<NSFilePromiseReceiver>> {
    let classes = NSArray::from_slice(&[NSFilePromiseReceiver::class()]);
    let Some(objects) = (unsafe { pb.readObjectsForClasses_options(&classes, None) }) else { return Vec::new() };
    (0..objects.count())
        .filter_map(|i| {
            let object: Retained<AnyObject> = objects.objectAtIndex(i);
            object.downcast::<NSFilePromiseReceiver>().ok()
        })
        .collect()
}

fn pasteboard_has_files(pb: &NSPasteboard) -> bool {
    let Some(types) = pb.types() else { return false };
    let promise_types = NSFilePromiseReceiver::readableDraggedTypes();
    types.iter().any(|t| {
        let t = t.to_string();
        t == unsafe { NSPasteboardTypeFileURL }.to_string() || promise_types.iter().any(|p| p.to_string() == t)
    })
}

fn idle_hint(enabled: bool, recipient: Option<&Recipient>) -> String {
    if !enabled {
        "File sharing is turned off".to_string()
    } else {
        match recipient {
            Some(r) => format!("Drop files or folders here to offer them to {}", r.name),
            None => "Choose a recipient, then drop files here".to_string(),
        }
    }
}

pub struct HeaderIvars {
    ctx: Arc<ShelfCtx>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpliceShelfHeader"]
    #[ivars = HeaderIvars]
    pub struct HeaderController;

    unsafe impl NSObjectProtocol for HeaderController {}

    impl HeaderController {
        #[unsafe(method(addFiles:))]
        fn add_files(&self, _sender: Option<&AnyObject>) {
            let ctx = self.ivars().ctx.clone();
            let mtm = self.mtm();
            let mut recipient = None;
            with_ui(&ctx, |ui| recipient = ui.selected_recipient());
            let Some(recipient) = recipient else {
                with_ui(&ctx, |ui| ui.set_status("Choose a recipient before adding files"));
                return;
            };
            choose_sources(mtm, move |selection| {
                ctx.emit(FileEvent::SourceSelected {
                    paths: selection.paths,
                    recipient: recipient.id.clone(),
                    gesture: SourceGesture::OpenPanel,
                    lease: selection.lease,
                });
            });
        }

        #[unsafe(method(recipientChanged:))]
        fn recipient_changed(&self, _sender: Option<&AnyObject>) {
            let ctx = self.ivars().ctx.clone();
            with_ui(&ctx, |ui| ui.recipient_changed());
        }

        #[unsafe(method(closePanel:))]
        fn close_panel(&self, _sender: Option<&AnyObject>) {
            let ctx = self.ivars().ctx.clone();
            with_ui(&ctx, |ui| ui.set_visible_by_user(false));
        }
    }
);

impl HeaderController {
    fn new(mtm: MainThreadMarker, ctx: Arc<ShelfCtx>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HeaderIvars { ctx });
        unsafe { msg_send![super(this), init] }
    }
}

#[allow(deprecated)]
fn bring_forward(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

fn choose_directory(mtm: MainThreadMarker, prompt: &str, message: &str, done: impl Fn(FileSelection) + 'static) {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setCanCreateDirectories(true);
    panel.setAllowsMultipleSelection(false);
    panel.setPrompt(Some(&ns(prompt)));
    panel.setMessage(Some(&ns(message)));
    let handler = {
        let panel = panel.clone();
        RcBlock::new(move |response| {
            if response != NSModalResponseOK {
                return;
            }
            let urls: Vec<Retained<NSURL>> = panel.URL().into_iter().collect();
            if let Some(selection) = files::selection_from_urls(urls) {
                done(selection);
            }
        })
    };
    bring_forward(mtm);
    panel.beginWithCompletionHandler(&handler);
}

fn choose_sources(mtm: MainThreadMarker, done: impl Fn(FileSelection) + 'static) {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(true);
    panel.setCanCreateDirectories(false);
    panel.setAllowsMultipleSelection(true);
    panel.setPrompt(Some(&ns("Offer")));
    panel.setMessage(Some(&ns("Choose files or folders to offer to the other computer.")));
    let handler = {
        let panel = panel.clone();
        RcBlock::new(move |response| {
            if response != NSModalResponseOK {
                return;
            }
            let urls: Vec<Retained<NSURL>> = panel.URLs().to_vec();
            if let Some(selection) = files::selection_from_urls(urls) {
                done(selection);
            }
        })
    };
    bring_forward(mtm);
    panel.beginWithCompletionHandler(&handler);
}

pub struct ShelfUi {
    mtm: MainThreadMarker,
    ctx: Arc<ShelfCtx>,
    panel: Retained<NSPanel>,
    root: Retained<DropView>,
    _header: Retained<HeaderController>,
    popup: Retained<NSPopUpButton>,
    add_button: Retained<NSButton>,
    hint: Retained<NSTextField>,
    empty: Retained<NSTextField>,
    status: Retained<NSTextField>,
    scroll: Retained<NSScrollView>,
    list: Retained<ListView>,
    promise_queue: Retained<NSOperationQueue>,
    rows: Vec<(RowKey, Retained<RowView>)>,
    recipients: Vec<Recipient>,
    selected: Option<MachineId>,
    seen_offers: HashSet<OfferId>,
    status_until: Option<Instant>,
    snapshot_error: Option<String>,
    user_hidden: bool,
    drags_active: usize,
    deferred: Option<Vec<RowModel>>,
}

pub fn with_ui(ctx: &Arc<ShelfCtx>, f: impl FnOnce(&mut ShelfUi)) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!("file shelf accessed off the main thread");
        return;
    };
    UI.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            tracing::error!("file shelf re-entered while busy; dropping update");
            return;
        };
        let ui = slot.get_or_insert_with(|| ShelfUi::new(mtm, ctx.clone()));
        f(ui);
    });
}

impl ShelfUi {
    fn new(mtm: MainThreadMarker, ctx: Arc<ShelfCtx>) -> Self {
        let height = HEADER_H + EMPTY_H + STATUS_H + MARGIN;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, WIDTH, height),
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setHidesOnDeactivate(false);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setFloatingPanel(true);
        panel.setMovableByWindowBackground(true);
        panel.setLevel(objc2_app_kit::NSFloatingWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        let promise_queue = NSOperationQueue::new();
        promise_queue.setName(Some(&ns("splice-file-promises")));
        promise_queue.setMaxConcurrentOperationCount(4);
        let root = DropView::new(mtm, ctx.clone(), rect(0.0, 0.0, WIDTH, height));
        let background = backdrop(mtm, rect(0.0, 0.0, WIDTH, height), 0.96, 12.0);
        background.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable);
        root.addSubview(&background);
        let header = HeaderController::new(mtm, ctx.clone());
        let header_target: &AnyObject = &header;
        let title = label(mtm, rect(MARGIN, 10.0, 200.0, 18.0), 13.0, true, false);
        title.setStringValue(&ns("Splice files"));
        root.addSubview(&title);
        let close = small_button(mtm, "Hide", header_target, sel!(closePanel:));
        let close_size = close.frame().size;
        close.setFrame(rect(WIDTH - MARGIN - close_size.width, 8.0, close_size.width, close_size.height));
        root.addSubview(&close);
        let send_to = label(mtm, rect(MARGIN, 40.0, 56.0, 18.0), 12.0, false, true);
        send_to.setStringValue(&ns("Send to"));
        root.addSubview(&send_to);
        let popup = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), rect(MARGIN + 56.0, 36.0, 190.0, 24.0), false);
        popup.setControlSize(NSControlSize::Small);
        popup.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        unsafe {
            popup.setTarget(Some(header_target));
            popup.setAction(Some(sel!(recipientChanged:)));
        }
        root.addSubview(&popup);
        let add_button = small_button(mtm, "Add files…", header_target, sel!(addFiles:));
        let add_size = add_button.frame().size;
        add_button.setFrame(rect(WIDTH - MARGIN - add_size.width, 36.0, add_size.width, add_size.height));
        add_button.setToolTip(Some(&ns("Choose files or folders to offer to the selected computer")));
        root.addSubview(&add_button);
        let hint = label(mtm, rect(MARGIN, 66.0, WIDTH - 2.0 * MARGIN, 16.0), 11.0, false, true);
        hint.setStringValue(&ns(&idle_hint(false, None)));
        root.addSubview(&hint);
        *root.ivars().hint.borrow_mut() = Some(hint.clone());
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(MARGIN, HEADER_H, WIDTH - 2.0 * MARGIN, EMPTY_H));
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);
        scroll.setBorderType(NSBorderType::NoBorder);
        let list = ListView::new(mtm, rect(0.0, 0.0, WIDTH - 2.0 * MARGIN, 0.0));
        scroll.setDocumentView(Some(&list));
        root.addSubview(&scroll);
        let empty = label(mtm, rect(MARGIN, HEADER_H + 12.0, WIDTH - 2.0 * MARGIN, 16.0), 11.5, false, true);
        empty.setStringValue(&ns("Nothing offered yet"));
        root.addSubview(&empty);
        let status = label(mtm, rect(MARGIN, HEADER_H + EMPTY_H + 2.0, WIDTH - 2.0 * MARGIN, 16.0), 11.0, false, true);
        root.addSubview(&status);
        panel.setContentView(Some(&root));
        Self {
            mtm,
            ctx,
            panel,
            root,
            _header: header,
            popup,
            add_button,
            hint,
            empty,
            status,
            scroll,
            list,
            promise_queue,
            rows: Vec::new(),
            recipients: Vec::new(),
            selected: None,
            seen_offers: HashSet::new(),
            status_until: None,
            snapshot_error: None,
            user_hidden: false,
            drags_active: 0,
            deferred: None,
        }
    }

    pub fn selected_recipient(&self) -> Option<Recipient> {
        let selected = self.selected.as_ref()?;
        self.recipients.iter().find(|r| &r.id == selected).cloned()
    }

    pub fn recipient_changed(&mut self) {
        let index = self.popup.indexOfSelectedItem();
        self.selected = usize::try_from(index).ok().and_then(|i| i.checked_sub(1)).and_then(|i| self.recipients.get(i)).map(|r| r.id.clone());
        self.sync_recipient();
    }

    fn sync_recipient(&mut self) {
        let recipient = self.selected_recipient();
        let enabled = self.root.ivars().enabled.get();
        *self.root.ivars().recipient.borrow_mut() = recipient.clone();
        self.hint.setStringValue(&ns(&idle_hint(enabled, recipient.as_ref())));
        self.add_button.setEnabled(enabled && recipient.is_some());
    }

    fn set_recipients(&mut self, recipients: Vec<Recipient>) {
        if recipients != self.recipients {
            self.popup.removeAllItems();
            self.popup.addItemWithTitle(&ns(if recipients.is_empty() { "No computers available" } else { "Choose a computer" }));
            for recipient in &recipients {
                let name = if recipients.iter().filter(|other| other.name == recipient.name).count() > 1 {
                    format!("{} ({})", recipient.name, recipient.id.0)
                } else { recipient.name.clone() };
                self.popup.addItemWithTitle(&ns(&name));
            }
            self.popup.setEnabled(!recipients.is_empty());
            self.recipients = recipients;
        }
        let still_present = self.selected.as_ref().is_some_and(|s| self.recipients.iter().any(|r| &r.id == s));
        if !still_present {
            self.selected = None;
        }
        if let Some(index) = self.selected.as_ref().and_then(|s| self.recipients.iter().position(|r| &r.id == s)) {
            self.popup.selectItemAtIndex(index as isize + 1);
        } else {
            self.popup.selectItemAtIndex(0);
        }
        self.sync_recipient();
    }

    pub fn set_status(&mut self, text: &str) {
        self.status.setStringValue(&ns(text));
        self.status.setToolTip(Some(&ns(text)));
        self.status_until = Some(Instant::now() + STATUS_HOLD);
        if !self.user_hidden {
            self.show();
        }
    }

    fn refresh_status(&mut self) {
        let holding = self.status_until.is_some_and(|until| Instant::now() < until);
        if holding {
            return;
        }
        self.status_until = None;
        let text = self.snapshot_error.clone().unwrap_or_default();
        self.status.setStringValue(&ns(&text));
        self.status.setToolTip(Some(&ns(&text)));
    }

    pub fn set_visible_by_user(&mut self, visible: bool) {
        self.user_hidden = !visible;
        if visible {
            self.show();
        } else {
            self.panel.orderOut(None);
        }
    }

    fn show(&mut self) {
        self.place();
        self.panel.orderFrontRegardless();
    }

    fn place(&self) {
        let Some(screen) = NSScreen::mainScreen(self.mtm) else { return };
        let visible = screen.visibleFrame();
        let size = self.panel.frame().size;
        let current = self.panel.frame();
        let inside = current.origin.x >= visible.origin.x
            && current.origin.y >= visible.origin.y
            && current.origin.x + size.width <= visible.origin.x + visible.size.width
            && current.origin.y + size.height <= visible.origin.y + visible.size.height;
        if inside && self.panel.isVisible() {
            return;
        }
        let origin = NSPoint::new(
            visible.origin.x + visible.size.width - size.width - SCREEN_MARGIN,
            visible.origin.y + SCREEN_MARGIN,
        );
        self.panel.setFrameOrigin(origin);
    }

    pub fn drag_started(&mut self) {
        self.drags_active += 1;
    }

    pub fn drag_finished(&mut self) {
        self.drags_active = self.drags_active.saturating_sub(1);
        if self.drags_active == 0 {
            if let Some(rows) = self.deferred.take() {
                self.set_rows(rows);
            }
        }
    }

    pub fn apply(&mut self, snapshot: ShelfSnapshot) {
        let rows = build_rows(&snapshot, now_unix_ms());
        self.root.ivars().enabled.set(snapshot.enabled);
        self.set_recipients(snapshot.recipients.clone());
        self.snapshot_error = snapshot.error.clone();
        let mut fresh = false;
        for offer in &snapshot.offers {
            if offer.owner != snapshot.self_id && offer.state == OfferState::Available && self.seen_offers.insert(offer.id) {
                fresh = true;
            }
        }
        self.seen_offers.retain(|id| snapshot.offers.iter().any(|o| &o.id == id));
        if self.drags_active > 0 {
            self.update_rows_in_place(&rows);
            self.deferred = Some(rows);
        } else {
            self.set_rows(rows);
        }
        self.refresh_status();
        if fresh {
            self.user_hidden = false;
        }
        if !self.user_hidden && (fresh || !self.panel.isVisible() && !snapshot.offers.is_empty()) {
            self.show();
        }
    }

    fn update_rows_in_place(&self, models: &[RowModel]) {
        for (key, view) in &self.rows {
            if let Some(model) = models.iter().find(|m| m.key == *key) {
                view.apply(model.clone());
            }
        }
    }

    fn set_rows(&mut self, models: Vec<RowModel>) {
        let mut existing: HashMap<RowKey, Retained<RowView>> = self.rows.drain(..).collect();
        let mut rows = Vec::with_capacity(models.len());
        for (index, model) in models.into_iter().enumerate() {
            let view = match existing.remove(&model.key) {
                Some(view) => {
                    view.apply(model.clone());
                    view
                }
                None => {
                    let view = RowView::new(self.mtm, self.ctx.clone(), self.promise_queue.clone(), model.clone());
                    self.list.addSubview(&view);
                    view
                }
            };
            view.setFrame(rect(0.0, index as f64 * ROW_H, WIDTH - 2.0 * MARGIN, ROW_H));
            rows.push((model.key, view));
        }
        for (_, stale) in existing {
            stale.removeFromSuperview();
        }
        self.rows = rows;
        let count = self.rows.len();
        let list_h = if count == 0 { EMPTY_H } else { (count.min(MAX_VISIBLE_ROWS) as f64) * ROW_H };
        self.list.setFrame(rect(0.0, 0.0, WIDTH - 2.0 * MARGIN, (count as f64) * ROW_H));
        self.scroll.setFrame(rect(MARGIN, HEADER_H, WIDTH - 2.0 * MARGIN, list_h));
        self.empty.setHidden(count != 0);
        self.status.setFrame(rect(MARGIN, HEADER_H + list_h + 2.0, WIDTH - 2.0 * MARGIN, 16.0));
        let height = HEADER_H + list_h + STATUS_H + MARGIN;
        let frame = self.panel.frame();
        let top = frame.origin.y + frame.size.height;
        self.panel.setFrame_display(rect(frame.origin.x, top - height, WIDTH, height), true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_shelf::{EntryKind, MachineName, ShelfEntry};

    fn machine(s: &str) -> MachineId {
        MachineId(s.into())
    }

    fn offer(id: u8, owner: &str, recipient: &str, roots: &[(&str, EntryKind)], state: OfferState) -> ShelfOffer {
        ShelfOffer {
            id: OfferId([id; 16]),
            owner: machine(owner),
            recipient: machine(recipient),
            entries: roots
                .iter()
                .enumerate()
                .map(|(i, (name, kind))| ShelfEntry { id: i as u32, parent: None, name: (*name).into(), kind: kind.clone() })
                .collect(),
            total_bytes: roots.iter().map(|(_, k)| if let EntryKind::File { size } = k { *size } else { 0 }).sum(),
            expires_unix_ms: 3 * 60 * 60 * 1000,
            state,
        }
    }

    fn transfer(id: u8, offer: u8, state: TransferState, direction: TransferDirection, roots: &[u32], paths: &[&str], pinned: bool) -> ShelfTransfer {
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        ShelfTransfer {
            id: TransferId([id; 16]),
            offer: OfferId([offer; 16]),
            peer: machine("linux"),
            direction,
            state,
            roots: roots.to_vec(),
            bytes: 500,
            total_bytes: 1000,
            received: pinned.then(|| ReceivedSelection::new(TransferId([id; 16]), paths.clone(), Some(Arc::new(())))),
            paths,
            error: (state == TransferState::Failed).then(|| "disk full".to_string()),
        }
    }

    fn snapshot(offers: Vec<ShelfOffer>, transfers: Vec<ShelfTransfer>) -> ShelfSnapshot {
        ShelfSnapshot {
            self_id: machine("mac"),
            enabled: true,
            recipients: vec![Recipient { id: machine("linux"), name: "gamedev".into() }],
            names: vec![
                MachineName { id: machine("linux"), name: "gamedev".into() },
                MachineName { id: machine("mac"), name: "studio".into() },
            ],
            offers,
            transfers,
            error: None,
        }
    }

    fn two_files() -> ShelfOffer {
        offer(1, "linux", "mac", &[("a.txt", EntryKind::File { size: 500 }), ("b.txt", EntryKind::File { size: 500 })], OfferState::Available)
    }

    #[test]
    fn incoming_file_offer_uses_promise_drag_and_explicit_actions() {
        let rows = build_rows(&snapshot(vec![two_files()], vec![]), 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].drag, DragMode::Promise(vec![(0, "a.txt".into()), (1, "b.txt".into())]));
        assert_eq!(rows[0].actions, vec![Action::Receive, Action::Save, Action::Dismiss]);
        assert!(rows[0].subtitle.contains("From gamedev"));
        assert!(rows[0].subtitle.contains("expires in 3 h"));
        assert!(rows[0].incoming && !rows[0].indented);
    }

    #[test]
    fn directory_offers_must_be_received_before_dragging() {
        let snap = snapshot(vec![offer(1, "linux", "mac", &[("photos", EntryKind::Directory)], OfferState::Available)], vec![]);
        let rows = build_rows(&snap, 0);
        assert!(matches!(rows[0].drag, DragMode::Blocked(_)));
        assert!(rows[0].actions.contains(&Action::Receive));
    }

    #[test]
    fn per_root_receipts_are_independent_rows_with_coverage() {
        let snap = snapshot(
            vec![two_files()],
            vec![
                transfer(8, 1, TransferState::Ready, TransferDirection::Receive, &[0], &["/cache/a.txt"], true),
                transfer(9, 1, TransferState::Receiving, TransferDirection::Receive, &[1], &[], false),
            ],
        );
        let rows = build_rows(&snap, 0);
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].drag, DragMode::Promise(_)));
        assert_eq!(rows[1].key, RowKey::Transfer(TransferId([8; 16])));
        assert_eq!(rows[1].title, "a.txt");
        assert!(rows[1].subtitle.contains("1 of 2 items"), "{}", rows[1].subtitle);
        assert!(matches!(&rows[1].drag, DragMode::LocalUrls(s) if s.paths == vec![PathBuf::from("/cache/a.txt")]));
        assert_eq!(rows[1].actions, vec![Action::Copy, Action::Reveal, Action::Clear]);
        assert_eq!(rows[2].key, RowKey::Transfer(TransferId([9; 16])));
        assert_eq!(rows[2].title, "b.txt");
        assert_eq!(rows[2].actions, vec![Action::Cancel]);
        assert_eq!(rows[2].progress, Some(0.5));
        assert!(rows[1].indented && rows[2].indented);
    }

    #[test]
    fn unpinned_ready_receipt_cannot_be_dragged() {
        let snap = snapshot(
            vec![two_files()],
            vec![transfer(8, 1, TransferState::Ready, TransferDirection::Receive, &[], &["/cache/a.txt", "/cache/b.txt"], false)],
        );
        let rows = build_rows(&snap, 0);
        assert!(matches!(rows[1].drag, DragMode::Blocked(_)));
        assert_eq!(rows[1].title, "a.txt and 1 more");
        assert!(!rows[1].subtitle.contains(" of "));
    }

    #[test]
    fn failed_save_keeps_partial_paths_revealable_and_newer_failure_is_visible() {
        let snap = snapshot(
            vec![two_files()],
            vec![
                transfer(8, 1, TransferState::Ready, TransferDirection::Receive, &[], &["/cache/a.txt", "/cache/b.txt"], true),
                transfer(9, 1, TransferState::Failed, TransferDirection::Receive, &[], &["/Users/me/Desktop/a.txt"], false),
            ],
        );
        let rows = build_rows(&snap, 0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].actions, vec![Action::Retry, Action::Reveal, Action::Clear]);
        assert!(rows[2].subtitle.contains("disk full"));
        assert_eq!(rows[2].paths, vec![PathBuf::from("/Users/me/Desktop/a.txt")]);
    }

    #[test]
    fn ready_folder_receipt_drags_local_urls() {
        let snap = snapshot(
            vec![offer(1, "linux", "mac", &[("photos", EntryKind::Directory)], OfferState::Available)],
            vec![transfer(9, 1, TransferState::Ready, TransferDirection::Receive, &[], &["/tmp/cache/photos"], true)],
        );
        let rows = build_rows(&snap, 0);
        assert!(matches!(&rows[1].drag, DragMode::LocalUrls(s) if s.paths == vec![PathBuf::from("/tmp/cache/photos")]));
    }

    #[test]
    fn outgoing_offers_and_sends_never_drag_locally() {
        let snap = snapshot(
            vec![offer(1, "mac", "linux", &[("a.txt", EntryKind::File { size: 10 })], OfferState::Available)],
            vec![transfer(9, 1, TransferState::Receiving, TransferDirection::Send, &[], &[], false)],
        );
        let rows = build_rows(&snap, 0);
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0].drag, DragMode::Blocked(_)));
        assert_eq!(rows[0].actions, vec![Action::Dismiss]);
        assert!(!rows[1].incoming);
        assert_eq!(rows[1].actions, vec![Action::Cancel]);
        assert!(rows[1].subtitle.starts_with("Sending to gamedev"));
    }

    #[test]
    fn revoked_offers_only_dismiss() {
        let snap = snapshot(vec![offer(1, "linux", "mac", &[("a.txt", EntryKind::File { size: 10 })], OfferState::Revoked)], vec![]);
        let rows = build_rows(&snap, 0);
        assert_eq!(rows[0].actions, vec![Action::Dismiss]);
        assert!(matches!(rows[0].drag, DragMode::Blocked(_)));
    }

    #[test]
    fn orphan_receipts_remain_and_finished_orphan_sends_are_hidden() {
        let snap = snapshot(
            vec![],
            vec![
                transfer(9, 1, TransferState::Ready, TransferDirection::Receive, &[], &["/tmp/cache/a.txt", "/tmp/cache/b.txt"], true),
                transfer(10, 2, TransferState::Ready, TransferDirection::Send, &[], &[], false),
                transfer(11, 3, TransferState::Failed, TransferDirection::Send, &[], &[], false),
            ],
        );
        let rows = build_rows(&snap, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, RowKey::Transfer(TransferId([9; 16])));
        assert_eq!(rows[0].title, "a.txt and 1 more");
        assert_eq!(rows[0].actions, vec![Action::Copy, Action::Reveal, Action::Clear]);
        assert!(!rows[0].indented);
        assert_eq!(rows[1].key, RowKey::Transfer(TransferId([11; 16])));
    }

    #[test]
    fn drops_require_recipient_policy_files_and_copy() {
        let recipient = Recipient { id: machine("linux"), name: "gamedev".into() };
        assert!(drop_permitted(true, Some(&recipient), NSDragOperation::Copy | NSDragOperation::Move, true));
        assert!(!drop_permitted(true, Some(&recipient), NSDragOperation::Move, true));
        assert!(!drop_permitted(true, Some(&recipient), NSDragOperation::Copy, false));
        assert!(!drop_permitted(true, None, NSDragOperation::Copy, true));
        assert!(!drop_permitted(false, Some(&recipient), NSDragOperation::Copy, true));
    }

    #[test]
    fn pending_import_completes_once_after_inventory_is_known() {
        let mut state = PendingImport::new(None, SourceLease::none());
        state.done = 2;
        assert!(!state.complete());
        state.expected = Some(3);
        assert!(!state.complete());
        state.done = 3;
        assert!(state.complete());
        state.finished = true;
        state.done = 4;
        assert!(!state.complete());
    }

    #[test]
    fn expiry_text_scales() {
        assert_eq!(expiry_text(10 * 60 * 60 * 1000, 0), "expires in 10 h");
        assert_eq!(expiry_text(90 * 60 * 1000, 0), "expires in 90 min");
        assert_eq!(expiry_text(5, 10), "expires now");
    }
    #[test]
    fn import_abandonment_reports_once_and_cleanup_waits_for_pending_writers() {
        let cleaned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup_flag = cleaned.clone();
        let staging = std::env::temp_dir().join(format!("splice-import-abandon-{}", std::process::id()));
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("pending"), b"payload").unwrap();
        let owned = staging.clone();
        let cleanup = SourceLease::new(move || {
            std::fs::remove_dir_all(owned).unwrap();
            cleanup_flag.store(true, Ordering::Release);
        });
        let abandoned = Arc::new(AtomicU64::new(0));
        let flag = abandoned.clone();
        let mut state = PendingImport::new(None, cleanup);
        state.on_abandon = Some(Box::new(move || { flag.fetch_add(1, Ordering::AcqRel); }));
        let pending = Arc::new(Mutex::new(state));
        let writer = pending.clone();
        pending.lock().abandon();
        pending.lock().abandon();
        assert_eq!(abandoned.load(Ordering::Acquire), 1);
        drop(pending);
        assert!(!cleaned.load(Ordering::Acquire));
        assert!(staging.join("pending").exists());
        drop(writer);
        assert!(cleaned.load(Ordering::Acquire));
        assert!(!staging.exists());
        assert_eq!(abandoned.load(Ordering::Acquire), 1);
    }

    #[test]
    fn dropped_import_callback_reports_abandonment_and_success_retains_cleanup() {
        let abandoned = Arc::new(AtomicU64::new(0));
        let flag = abandoned.clone();
        let mut state = PendingImport::new(None, SourceLease::none());
        state.on_abandon = Some(Box::new(move || { flag.fetch_add(1, Ordering::AcqRel); }));
        drop(state);
        assert_eq!(abandoned.load(Ordering::Acquire), 1);
        let cleaned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cleaned.clone();
        let mut state = PendingImport::new(None, SourceLease::new(move || flag.store(true, Ordering::Release)));
        let admission = Arc::new(tokio::sync::Semaphore::new(1));
        state._admission = Some(admission.clone().try_acquire_owned().unwrap());
        let parent = state.cleanup.clone();
        state.finished = true;
        drop(state);
        assert!(admission.try_acquire_owned().is_ok());
        assert!(!cleaned.load(Ordering::Acquire));
        drop(parent);
        assert!(cleaned.load(Ordering::Acquire));
    }

}
