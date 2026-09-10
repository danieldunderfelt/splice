use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::unix::io::OwnedFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use gtk4::gio::prelude::FileExt;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use splice_platform::files::{
    completed_representations, encode_uri_list, FileOfferId, ViewId, MIME_PORTAL_FILETRANSFER,
    MIME_URI_LIST,
};

use crate::ipc::{
    self, HelperToService, OfferDesc, OfferStateDesc, PeerDesc, ReceiptDesc, ReceiptStateDesc,
    ServiceToHelper,
};

pub const HELPER_ARMED_LIMIT: usize = 6;
pub const IDLE_RELEASE: Duration = Duration::from_secs(90);
const LOST: &str = "Lost the connection to Splice; close this window and open the file shelf again";

#[derive(Clone, Debug)]
pub struct ViewOutcome {
    pub view: ViewId,
    pub uris: Vec<PathBuf>,
    pub portal_key: Option<String>,
}

pub type SourceDropHook = Rc<dyn Fn(String, Vec<PathBuf>, Vec<OwnedFd>) -> Result<(), String>>;

#[derive(Clone)]
pub struct ShelfHooks {
    pub take_view: Rc<dyn Fn(FileOfferId) -> Option<ViewOutcome>>,
    pub arm_view: Rc<dyn Fn(FileOfferId)>,
    pub take_receipt_view: Rc<dyn Fn(String) -> Option<ViewOutcome>>,
    pub arm_receipt_view: Rc<dyn Fn(String)>,
    pub drag_started: Rc<dyn Fn(ViewId)>,
    pub drop_performed: Rc<dyn Fn(ViewId)>,
    pub drag_cancelled: Rc<dyn Fn(ViewId)>,
    pub drag_finished: Rc<dyn Fn(ViewId)>,
    pub source_drop: SourceDropHook,
    pub receive: Rc<dyn Fn(FileOfferId)>,
    pub save_to: Rc<dyn Fn(FileOfferId, PathBuf)>,
    pub dismiss: Rc<dyn Fn(FileOfferId)>,
    pub cancel_receive: Option<Rc<dyn Fn(FileOfferId)>>,
    pub retry_receive: Option<Rc<dyn Fn(FileOfferId)>>,
    pub republish: Rc<dyn Fn(String)>,
    pub clear_receipt: Rc<dyn Fn(String)>,
    pub reveal: Rc<dyn Fn(String)>,
}

struct OfferRow {
    widget: gtk4::Widget,
    state_label: gtk4::Label,
    signature: OfferSignature,
}

struct ReceiptRow {
    widget: gtk4::Widget,
    state_label: gtk4::Label,
    signature: ReceiptSignature,
}

#[derive(Clone, PartialEq, Eq)]
struct OfferSignature {
    state: OfferStateDesc,
    origin: String,
    names: Vec<String>,
    total_size: Option<u64>,
    expires_at: Option<i64>,
    cancelable: bool,
    retryable: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct ReceiptSignature {
    origin: String,
    names: Vec<String>,
    total_size: Option<u64>,
    state: ReceiptStateDesc,
    error: Option<String>,
}

pub struct Shelf {
    pub window: gtk4::Window,
    list: gtk4::ListBox,
    receipt_list: gtk4::ListBox,
    status: gtk4::Label,
    offers: Rc<RefCell<Vec<OfferDesc>>>,
    receipts: Rc<RefCell<Vec<ReceiptDesc>>>,
    progress: Rc<RefCell<HashMap<FileOfferId, String>>>,
    view_errors: Rc<RefCell<HashMap<FileOfferId, String>>>,
    offer_rows: Rc<RefCell<HashMap<FileOfferId, OfferRow>>>,
    receipt_rows: Rc<RefCell<HashMap<String, ReceiptRow>>>,
    peers: Rc<RefCell<Vec<PeerDesc>>>,
    peer_names: gtk4::StringList,
    peer_select: gtk4::DropDown,
    hooks: ShelfHooks,
}

fn draggable(state: &OfferStateDesc) -> bool {
    matches!(state, OfferStateDesc::Available)
}

fn portal_doc_path(path: &Path) -> bool {
    let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") else {
        return false;
    };
    !runtime.is_empty() && path.starts_with(Path::new(&runtime).join("doc"))
}

fn retain_portal_fds(paths: &[PathBuf]) -> Result<Vec<OwnedFd>> {
    let mut fds = Vec::new();
    for path in paths {
        if !portal_doc_path(path) {
            continue;
        }
        if fds.len() >= ipc::MAX_FDS {
            return Err(anyhow!(
                "selection needs more than {} portal descriptors",
                ipc::MAX_FDS
            ));
        }
        let file = std::fs::File::open(path)
            .with_context(|| format!("retain portal-granted {}", path.display()))?;
        fds.push(OwnedFd::from(file));
    }
    Ok(fds)
}

fn row_index(list: &gtk4::ListBox, widget: &gtk4::Widget) -> Option<i32> {
    let row = widget.parent()?.downcast::<gtk4::ListBoxRow>().ok()?;
    let owner = row.parent()?;
    if owner != list.clone().upcast::<gtk4::Widget>() {
        return None;
    }
    Some(row.index())
}

fn ensure_position(list: &gtk4::ListBox, widget: &gtk4::Widget, index: i32) {
    if row_index(list, widget) != Some(index) {
        list.remove(widget);
        list.insert(widget, index);
    }
}

impl Shelf {
    pub fn build(title: &str, hooks: ShelfHooks) -> Rc<Shelf> {
        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.add_css_class("boxed-list");
        let receipt_list = gtk4::ListBox::new();
        receipt_list.set_selection_mode(gtk4::SelectionMode::None);
        receipt_list.add_css_class("boxed-list");
        let status = gtk4::Label::new(None);
        status.set_xalign(0.0);
        status.set_wrap(true);
        let drop_hint = gtk4::Label::new(Some(
            "Drop files here to offer them to the selected machine",
        ));
        drop_hint.add_css_class("dim-label");
        let incoming_hint = gtk4::Label::new(Some("Incoming offers"));
        incoming_hint.set_xalign(0.0);
        incoming_hint.add_css_class("dim-label");
        let receipt_hint = gtk4::Label::new(Some("Received files"));
        receipt_hint.set_xalign(0.0);
        receipt_hint.add_css_class("dim-label");
        let peer_names = gtk4::StringList::new(&["Choose a computer"]);
        let peer_select = gtk4::DropDown::new(Some(peer_names.clone()), gtk4::Expression::NONE);
        peer_select.set_hexpand(true);
        let peer_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        peer_row.append(&gtk4::Label::new(Some("Send to:")));
        peer_row.append(&peer_select);
        let scroll = gtk4::ScrolledWindow::builder().vexpand(true).build();
        let rows_column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        rows_column.append(&incoming_hint);
        rows_column.append(&list);
        rows_column.append(&receipt_hint);
        rows_column.append(&receipt_list);
        scroll.set_child(Some(&rows_column));
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        column.set_margin_top(10);
        column.set_margin_bottom(10);
        column.set_margin_start(10);
        column.set_margin_end(10);
        column.append(&peer_row);
        column.append(&drop_hint);
        column.append(&scroll);
        column.append(&status);
        let window = gtk4::Window::builder()
            .title(title)
            .default_width(420)
            .default_height(560)
            .child(&column)
            .build();
        let shelf = Rc::new(Shelf {
            window: window.clone(),
            list,
            receipt_list,
            status,
            offers: Rc::new(RefCell::new(Vec::new())),
            receipts: Rc::new(RefCell::new(Vec::new())),
            progress: Rc::new(RefCell::new(HashMap::new())),
            view_errors: Rc::new(RefCell::new(HashMap::new())),
            offer_rows: Rc::new(RefCell::new(HashMap::new())),
            receipt_rows: Rc::new(RefCell::new(HashMap::new())),
            peers: Rc::new(RefCell::new(Vec::new())),
            peer_names,
            peer_select,
            hooks,
        });
        let target = gtk4::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let target_shelf = Rc::clone(&shelf);
        target.connect_drop(move |_, value, _, _| {
            let Ok(files) = value.get::<gdk::FileList>() else {
                return false;
            };
            let paths: Vec<PathBuf> = files.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }
            let Some(recipient) = target_shelf.selected_recipient() else {
                target_shelf.set_status("Choose a destination machine before dropping files");
                return false;
            };
            let fds = match retain_portal_fds(&paths) {
                Ok(fds) => fds,
                Err(err) => {
                    target_shelf.set_status(&format!("Cannot offer this selection: {err}"));
                    return false;
                }
            };
            match (target_shelf.hooks.source_drop)(recipient, paths, fds) {
                Ok(()) => true,
                Err(error) => {
                    target_shelf.set_status(&format!("Cannot offer this selection: {error}"));
                    false
                }
            }
        });
        window.add_controller(target);
        shelf
    }

    pub fn selected_recipient(&self) -> Option<String> {
        let index = self.peer_select.selected().checked_sub(1)? as usize;
        self.peers.borrow().get(index).map(|p| p.id.clone())
    }

    pub fn set_peers(&self, peers: Vec<PeerDesc>) {
        let previous = self.selected_recipient();
        let selected = previous
            .as_ref()
            .and_then(|id| peers.iter().position(|peer| &peer.id == id));
        let labels: Vec<String> = std::iter::once("Choose a computer".to_owned())
            .chain(peers.iter().map(|peer| {
                if peers.iter().filter(|other| other.name == peer.name).count() > 1 {
                    format!("{} ({})", peer.name, peer.id)
                } else {
                    peer.name.clone()
                }
            }))
            .collect();
        let names: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.peer_names.splice(0, self.peer_names.n_items(), &names);
        *self.peers.borrow_mut() = peers;
        self.peer_select
            .set_selected(selected.map_or(0, |index| index as u32 + 1));
        if previous.is_some() && selected.is_none() {
            self.set_status("The selected machine went away; choose a destination again");
        }
    }

    pub fn set_offers(&self, offers: Vec<OfferDesc>) {
        {
            let mut rows = self.offer_rows.borrow_mut();
            let wanted: HashSet<FileOfferId> = offers.iter().map(|offer| offer.offer).collect();
            let vanished: Vec<FileOfferId> = rows
                .keys()
                .filter(|id| !wanted.contains(*id))
                .copied()
                .collect();
            for id in vanished {
                if let Some(row) = rows.remove(&id) {
                    self.list.remove(&row.widget);
                }
            }
            for (index, offer) in offers.iter().enumerate() {
                let signature = OfferSignature {
                    state: offer.state.clone(),
                    origin: offer.origin.clone(),
                    names: offer.names.clone(),
                    total_size: offer.total_size,
                    expires_at: offer.expires_at,
                    cancelable: self.hooks.cancel_receive.is_some(),
                    retryable: self.hooks.retry_receive.is_some(),
                };
                let rebuild = rows
                    .get(&offer.offer)
                    .is_some_and(|row| row.signature != signature);
                if rebuild {
                    if let Some(row) = rows.remove(&offer.offer) {
                        self.list.remove(&row.widget);
                    }
                }
                if let std::collections::hash_map::Entry::Vacant(entry) = rows.entry(offer.offer) {
                    let row = self.build_offer_row(offer, signature);
                    self.list.insert(&row.widget, index as i32);
                    entry.insert(row);
                }
            }
            for (index, offer) in offers.iter().enumerate() {
                if let Some(row) = rows.get(&offer.offer) {
                    ensure_position(&self.list, &row.widget, index as i32);
                }
            }
        }
        *self.offers.borrow_mut() = offers;
        self.refresh_offer_labels();
    }

    pub fn set_receipts(&self, receipts: Vec<ReceiptDesc>) {
        {
            let mut rows = self.receipt_rows.borrow_mut();
            let wanted: HashSet<String> = receipts
                .iter()
                .map(|receipt| receipt.receipt.clone())
                .collect();
            let vanished: Vec<String> = rows
                .keys()
                .filter(|key| !wanted.contains(*key))
                .cloned()
                .collect();
            for key in vanished {
                if let Some(row) = rows.remove(&key) {
                    self.receipt_list.remove(&row.widget);
                }
            }
            for (index, receipt) in receipts.iter().enumerate() {
                let signature = ReceiptSignature {
                    origin: receipt.origin.clone(),
                    names: receipt.names.clone(),
                    total_size: receipt.total_size,
                    state: receipt.state,
                    error: receipt.error.clone(),
                };
                let rebuild = rows
                    .get(&receipt.receipt)
                    .is_some_and(|row| row.signature != signature);
                if rebuild {
                    if let Some(row) = rows.remove(&receipt.receipt) {
                        self.receipt_list.remove(&row.widget);
                    }
                }
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    rows.entry(receipt.receipt.clone())
                {
                    let row = self.build_receipt_row(receipt, signature);
                    self.receipt_list.insert(&row.widget, index as i32);
                    entry.insert(row);
                }
            }
            for (index, receipt) in receipts.iter().enumerate() {
                if let Some(row) = rows.get(&receipt.receipt) {
                    ensure_position(&self.receipt_list, &row.widget, index as i32);
                }
            }
        }
        *self.receipts.borrow_mut() = receipts;
        self.refresh_receipt_labels();
    }

    fn refresh_offer_labels(&self) {
        let progress = self.progress.borrow();
        let offers = self.offers.borrow();
        let rows = self.offer_rows.borrow();
        for offer in offers.iter() {
            if let Some(row) = rows.get(&offer.offer) {
                let text = match progress.get(&offer.offer) {
                    Some(text) => text.clone(),
                    None => describe_state(&offer.state, &offer.origin, offer.expires_at),
                };
                row.state_label.set_text(&text);
            }
        }
    }

    fn refresh_receipt_labels(&self) {
        let receipts = self.receipts.borrow();
        let rows = self.receipt_rows.borrow();
        for receipt in receipts.iter() {
            if let Some(row) = rows.get(&receipt.receipt) {
                row.state_label.set_text(&describe_receipt(receipt));
            }
        }
    }

    pub fn set_view_error(&self, offer: FileOfferId, error: String) {
        self.view_errors.borrow_mut().insert(offer, error);
    }

    pub fn clear_view_error(&self, offer: FileOfferId) {
        self.view_errors.borrow_mut().remove(&offer);
    }

    pub fn set_status(&self, message: &str) {
        self.status.set_text(message);
    }

    pub fn reveal(&self, paths: &[PathBuf]) {
        reveal_paths(paths, &self.window, &self.status);
    }

    pub fn set_progress(&self, offer: FileOfferId, text: String) {
        self.progress.borrow_mut().insert(offer, text.clone());
        let rows = self.offer_rows.borrow();
        if let Some(row) = rows.get(&offer) {
            row.state_label.set_text(&text);
        }
    }

    fn build_offer_row(&self, offer: &OfferDesc, signature: OfferSignature) -> OfferRow {
        let names = row_names(&offer.names, offer.count);
        let size = offer
            .total_size
            .map(format_size)
            .unwrap_or_else(|| "size pending".into());
        let title = gtk4::Label::new(Some(&format!("{names}  ({size})")));
        title.set_xalign(0.0);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let state_label = gtk4::Label::new(Some(&describe_state(
            &offer.state,
            &offer.origin,
            offer.expires_at,
        )));
        state_label.set_xalign(0.0);
        state_label.add_css_class("dim-label");
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let offer_id = offer.offer;
        match &offer.state {
            OfferStateDesc::Available => {
                let receive = gtk4::Button::with_label("Receive to clipboard");
                let hooks = self.hooks.clone();
                receive.connect_clicked(move |_| (hooks.receive)(offer_id));
                let save = gtk4::Button::with_label("Save to…");
                let hooks = self.hooks.clone();
                let window = self.window.clone();
                save.connect_clicked(move |_| {
                    let hooks = hooks.clone();
                    let dialog = gtk4::FileDialog::new();
                    dialog.select_folder(
                        Some(&window),
                        gtk4::gio::Cancellable::NONE,
                        move |result| {
                            if let Ok(file) = result {
                                if let Some(path) = file.path() {
                                    (hooks.save_to)(offer_id, path);
                                }
                            }
                        },
                    );
                });
                let dismiss = gtk4::Button::with_label("Dismiss");
                let hooks = self.hooks.clone();
                dismiss.connect_clicked(move |_| (hooks.dismiss)(offer_id));
                buttons.append(&receive);
                buttons.append(&save);
                buttons.append(&dismiss);
            }
            OfferStateDesc::Preparing | OfferStateDesc::Receiving => {
                if let Some(cancel) = &self.hooks.cancel_receive {
                    let button = gtk4::Button::with_label("Cancel");
                    let cancel = cancel.clone();
                    button.connect_clicked(move |_| (cancel)(offer_id));
                    buttons.append(&button);
                }
            }
            OfferStateDesc::Failed => {
                if let Some(retry) = &self.hooks.retry_receive {
                    let button = gtk4::Button::with_label("Retry");
                    let retry = retry.clone();
                    button.connect_clicked(move |_| (retry)(offer_id));
                    buttons.append(&button);
                }
                let dismiss = gtk4::Button::with_label("Dismiss");
                let hooks = self.hooks.clone();
                dismiss.connect_clicked(move |_| (hooks.dismiss)(offer_id));
                buttons.append(&dismiss);
            }
            OfferStateDesc::Ready => {}
            OfferStateDesc::Revoked | OfferStateDesc::Expired => {
                let dismiss = gtk4::Button::with_label("Dismiss");
                let hooks = self.hooks.clone();
                dismiss.connect_clicked(move |_| (hooks.dismiss)(offer_id));
                buttons.append(&dismiss);
            }
        }
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        column.set_margin_top(6);
        column.set_margin_bottom(6);
        column.append(&title);
        column.append(&state_label);
        column.append(&buttons);
        if draggable(&offer.state) {
            let hooks = self.hooks.clone();
            let take = Rc::new(move || (hooks.take_view)(offer_id));
            let hooks = self.hooks.clone();
            let rearm = Rc::new(move || (hooks.arm_view)(offer_id));
            let view_errors = Rc::clone(&self.view_errors);
            let error = Rc::new(move || view_errors.borrow().get(&offer_id).cloned());
            self.attach_drag(&column, take, rearm, error);
        }
        OfferRow {
            widget: column.upcast(),
            state_label,
            signature,
        }
    }

    fn build_receipt_row(&self, receipt: &ReceiptDesc, signature: ReceiptSignature) -> ReceiptRow {
        let names = row_names(&receipt.names, receipt.count);
        let size = receipt
            .total_size
            .map(format_size)
            .unwrap_or_else(|| "size pending".into());
        let title = gtk4::Label::new(Some(&format!("{names}  ({size})")));
        title.set_xalign(0.0);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let state_label = gtk4::Label::new(Some(&describe_receipt(receipt)));
        state_label.set_xalign(0.0);
        state_label.set_wrap(true);
        state_label.add_css_class("dim-label");
        let key = receipt.receipt.clone();
        let retained = receipt.state == ReceiptStateDesc::Retained;
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let copy = gtk4::Button::with_label("Copy to clipboard");
        copy.set_sensitive(retained);
        let hooks = self.hooks.clone();
        let copy_key = key.clone();
        copy.connect_clicked(move |_| (hooks.republish)(copy_key.clone()));
        let reveal = gtk4::Button::with_label("Reveal");
        reveal.set_sensitive(retained);
        let hooks = self.hooks.clone();
        let reveal_key = key.clone();
        reveal.connect_clicked(move |_| (hooks.reveal)(reveal_key.clone()));
        let clear = gtk4::Button::with_label("Clear");
        clear.set_tooltip_text(Some(
            "Removes the retained copy. Saved links to these files will stop working.",
        ));
        clear.set_sensitive(receipt.state != ReceiptStateDesc::Clearing);
        let hooks = self.hooks.clone();
        let clear_key = key.clone();
        clear.connect_clicked(move |_| (hooks.clear_receipt)(clear_key.clone()));
        buttons.append(&copy);
        buttons.append(&reveal);
        buttons.append(&clear);
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        column.set_margin_top(6);
        column.set_margin_bottom(6);
        column.append(&title);
        column.append(&state_label);
        column.append(&buttons);
        if retained {
            let hooks = self.hooks.clone();
            let take_key = key.clone();
            let take = Rc::new(move || (hooks.take_receipt_view)(take_key.clone()));
            let hooks = self.hooks.clone();
            let rearm_key = key.clone();
            let rearm = Rc::new(move || (hooks.arm_receipt_view)(rearm_key.clone()));
            let error = Rc::new(|| -> Option<String> { None });
            self.attach_drag(&column, take, rearm, error);
        }
        ReceiptRow {
            widget: column.upcast(),
            state_label,
            signature,
        }
    }

    fn attach_drag(
        &self,
        widget: &gtk4::Box,
        take: Rc<dyn Fn() -> Option<ViewOutcome>>,
        rearm: Rc<dyn Fn()>,
        error: Rc<dyn Fn() -> Option<String>>,
    ) {
        let hover = gtk4::EventControllerMotion::new();
        let arm = Rc::clone(&rearm);
        hover.connect_enter(move |_, _, _| arm());
        widget.add_controller(hover);
        widget.add_controller(self.drag_source(take, rearm, error));
    }

    fn drag_source(
        &self,
        take: Rc<dyn Fn() -> Option<ViewOutcome>>,
        rearm: Rc<dyn Fn()>,
        error: Rc<dyn Fn() -> Option<String>>,
    ) -> gtk4::DragSource {
        let source = gtk4::DragSource::new();
        source.set_actions(gdk::DragAction::COPY);
        let status = self.status.clone();
        let pending: Rc<RefCell<Option<ViewOutcome>>> = Rc::new(RefCell::new(None));
        let pending_prepare = Rc::clone(&pending);
        let rearm_prepare = Rc::clone(&rearm);
        source.connect_prepare(move |_, _, _| {
            if let Some(outcome) = pending_prepare.borrow().as_ref() {
                return Some(view_provider(outcome));
            }
            let outcome = match take() {
                Some(outcome) => outcome,
                None => {
                    match error() {
                        Some(error) => status.set_text(&error),
                        None => {
                            rearm_prepare();
                            status
                                .set_text("Preparing the files; start the drag again in a moment");
                        }
                    }
                    return None;
                }
            };
            let provider = view_provider(&outcome);
            *pending_prepare.borrow_mut() = Some(outcome);
            Some(provider)
        });
        let hooks = self.hooks.clone();
        source.connect_drag_begin(move |_, drag| {
            let Some(outcome) = pending.borrow_mut().take() else {
                return;
            };
            let view = outcome.view;
            (hooks.drag_started)(view);
            let performed = hooks.drop_performed.clone();
            drag.connect_drop_performed(move |_| performed(view));
            let cancelled = hooks.drag_cancelled.clone();
            drag.connect_cancel(move |_, _| cancelled(view));
            let finished = hooks.drag_finished.clone();
            drag.connect_dnd_finished(move |_| finished(view));
            rearm();
        });
        source
    }
}

fn describe_receipt(receipt: &ReceiptDesc) -> String {
    let mut text = match receipt.state {
        ReceiptStateDesc::Retained => format!("From {} — retained locally", receipt.origin),
        ReceiptStateDesc::Unavailable => format!(
            "From {} — unavailable: {}",
            receipt.origin,
            receipt.error.as_deref().unwrap_or("waiting for Splice")
        ),
        ReceiptStateDesc::Clearing => format!("From {} — clearing…", receipt.origin),
    };
    if receipt.published {
        text.push_str(" — on the clipboard");
    }
    text
}

fn row_names(names: &[String], count: u32) -> String {
    if names.is_empty() {
        return format!("{count} items");
    }
    let label = names
        .iter()
        .map(|name| name.replace(['\n', '\r', '\t'], " "))
        .collect::<Vec<_>>()
        .join(", ");
    if count as usize <= names.len() {
        label
    } else {
        format!("{label}, +{} more", count as usize - names.len())
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn reveal_paths(paths: &[PathBuf], window: &gtk4::Window, status: &gtk4::Label) {
    let Some(path) = paths.first() else {
        status.set_text("Nothing to reveal");
        return;
    };
    let file = gtk4::gio::File::for_path(path);
    let launcher = gtk4::FileLauncher::new(Some(&file));
    let status = status.clone();
    if path.is_dir() {
        launcher.launch(Some(window), gtk4::gio::Cancellable::NONE, move |result| {
            if let Err(error) = result {
                status.set_text(&format!("Cannot reveal the files: {error}"));
            }
        });
    } else {
        launcher.open_containing_folder(
            Some(window),
            gtk4::gio::Cancellable::NONE,
            move |result| {
                if let Err(error) = result {
                    status.set_text(&format!("Cannot reveal the files: {error}"));
                }
            },
        );
    }
}

fn describe_state(state: &OfferStateDesc, origin: &str, expires_at: Option<i64>) -> String {
    let base = match state {
        OfferStateDesc::Available => format!("From {origin} — drag to copy"),
        OfferStateDesc::Revoked => "Revoked by source".into(),
        OfferStateDesc::Expired => "Expired".into(),
        OfferStateDesc::Preparing => "Preparing…".into(),
        OfferStateDesc::Receiving => "Receiving…".into(),
        OfferStateDesc::Ready => "Received — see retained files below".into(),
        OfferStateDesc::Failed => "Failed".into(),
    };
    match expires_at {
        Some(ts) if *state == OfferStateDesc::Available => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let seconds = ts.saturating_sub(now);
            if seconds <= 0 {
                format!("{base} · expiring")
            } else if seconds < 60 {
                format!("{base} · expires in under a minute")
            } else if seconds < 3600 {
                format!("{base} · expires in {} min", (seconds + 59) / 60)
            } else {
                format!("{base} · expires in {} h", (seconds + 3599) / 3600)
            }
        }
        _ => base,
    }
}

fn view_provider(outcome: &ViewOutcome) -> gdk::ContentProvider {
    let mut providers = vec![gdk::ContentProvider::for_bytes(
        MIME_URI_LIST,
        &glib::Bytes::from(encode_uri_list(&outcome.uris).as_bytes()),
    )];
    if let Some(key) = &outcome.portal_key {
        providers.push(gdk::ContentProvider::for_bytes(
            MIME_PORTAL_FILETRANSFER,
            &glib::Bytes::from(key.as_bytes()),
        ));
    }
    gdk::ContentProvider::new_union(&providers)
}

pub fn publish_completed(uris: &[PathBuf], portal_key: Option<&str>) -> Result<()> {
    let mut providers: Vec<gdk::ContentProvider> = completed_representations(uris)
        .iter()
        .map(|(mime, bytes)| gdk::ContentProvider::for_bytes(mime, &glib::Bytes::from(bytes)))
        .collect();
    if let Some(key) = portal_key {
        providers.push(gdk::ContentProvider::for_bytes(
            MIME_PORTAL_FILETRANSFER,
            &glib::Bytes::from(key.as_bytes()),
        ));
    }
    let union = gdk::ContentProvider::new_union(&providers);
    let display = gdk::Display::default().ok_or_else(|| anyhow!("no default display"))?;
    display
        .clipboard()
        .set_content(Some(&union))
        .map_err(|e| anyhow!("publish clipboard: {e}"))
}

pub struct HelperUi {
    pub shelf: Rc<Shelf>,
    pub clipboard_published: Rc<Cell<bool>>,
}

pub fn present_window(ui: &HelperUi) {
    ui.shelf.window.present();
}

pub fn run_window(ui: &HelperUi) {
    ui.shelf.window.present();
    let main_loop = glib::MainLoop::new(None, false);
    {
        let main_loop = main_loop.clone();
        let published = Rc::clone(&ui.clipboard_published);
        let window = ui.shelf.window.clone();
        ui.shelf.window.connect_close_request(move |_| {
            if published.get() {
                window.set_visible(false);
                glib::Propagation::Stop
            } else {
                main_loop.quit();
                glib::Propagation::Proceed
            }
        });
    }
    main_loop.run();
}

enum Incoming {
    Message(ServiceToHelper),
    Lost(String),
}

struct Armory<K> {
    armed: HashMap<K, ViewOutcome>,
    index: HashMap<ViewId, K>,
    pending: HashSet<K>,
    used: HashMap<K, Instant>,
}

impl<K: std::hash::Hash + Eq + Clone> Armory<K> {
    fn new() -> Self {
        Self {
            armed: HashMap::new(),
            index: HashMap::new(),
            pending: HashSet::new(),
            used: HashMap::new(),
        }
    }

    fn take(&mut self, key: &K) -> Option<ViewOutcome> {
        let outcome = self.armed.remove(key)?;
        self.index.remove(&outcome.view);
        self.used.remove(key);
        Some(outcome)
    }

    fn want(&mut self, key: K) -> bool {
        self.used.insert(key.clone(), Instant::now());
        !self.armed.contains_key(&key) && self.pending.insert(key)
    }

    fn ready(&mut self, key: K, outcome: ViewOutcome) {
        self.pending.remove(&key);
        self.used.entry(key.clone()).or_insert_with(Instant::now);
        if let Some(previous) = self.armed.insert(key.clone(), outcome.clone()) {
            self.index.remove(&previous.view);
        }
        self.index.insert(outcome.view, key);
    }

    fn failed(&mut self, key: &K) {
        self.pending.remove(key);
        self.take(key);
    }

    fn retire(&mut self, view: ViewId) {
        if let Some(key) = self.index.remove(&view) {
            if self
                .armed
                .get(&key)
                .is_some_and(|outcome| outcome.view == view)
            {
                self.armed.remove(&key);
                self.used.remove(&key);
            }
        }
    }

    fn retain(&mut self, wanted: &HashSet<K>) {
        let stale: Vec<K> = self
            .armed
            .keys()
            .filter(|key| !wanted.contains(*key))
            .cloned()
            .collect();
        for key in stale {
            self.take(&key);
        }
        self.pending.retain(|key| wanted.contains(key));
        self.used.retain(|key, _| wanted.contains(key));
    }

    fn count(&self) -> usize {
        self.armed.len() + self.pending.len()
    }

    fn oldest(&self) -> Option<(Instant, K)> {
        self.armed
            .keys()
            .map(|key| {
                (
                    self.used.get(key).copied().unwrap_or_else(Instant::now),
                    key.clone(),
                )
            })
            .min_by_key(|(used, _)| *used)
    }

    fn idle(&self, max_age: Duration) -> Vec<K> {
        self.armed
            .keys()
            .filter(|key| {
                self.used
                    .get(*key)
                    .is_none_or(|used| used.elapsed() >= max_age)
            })
            .cloned()
            .collect()
    }

    fn clear(&mut self) {
        self.armed.clear();
        self.index.clear();
        self.pending.clear();
        self.used.clear();
    }
}

fn make_room(
    offers: &mut Armory<FileOfferId>,
    receipts: &mut Armory<String>,
    send: &dyn Fn(HelperToService, Vec<OwnedFd>) -> bool,
) {
    while offers.count() + receipts.count() > HELPER_ARMED_LIMIT {
        let view = match (offers.oldest(), receipts.oldest()) {
            (Some((offer_used, offer)), Some((receipt_used, _))) if offer_used <= receipt_used => {
                offers.take(&offer)
            }
            (_, Some((_, receipt))) => receipts.take(&receipt),
            (Some((_, offer)), None) => offers.take(&offer),
            (None, None) => None,
        };
        match view {
            Some(outcome) => {
                send(
                    HelperToService::ReleaseView { view: outcome.view },
                    Vec::new(),
                );
            }
            None => break,
        }
    }
}

pub fn run() -> Result<()> {
    gtk4::init().map_err(|e| anyhow!("gtk init: {e}"))?;
    let socket = ipc::socket_path()?;
    let mut conn = ipc::Connection::connect(&socket)
        .with_context(|| format!("connect {}", socket.display()))?;
    conn.send(
        &HelperToService::Hello {
            version: ipc::PROTOCOL_VERSION,
        },
        &[],
    )?;
    let (sender, mut receiver) = conn.split()?;
    let connection = Rc::new(conn);
    let sender = Arc::new(Mutex::new(sender));
    let (tx, rx) = std::sync::mpsc::sync_channel::<Incoming>(256);
    std::thread::Builder::new()
        .name("splice-files-ipc".into())
        .spawn(move || loop {
            match receiver.recv::<ServiceToHelper>() {
                Ok(Some((msg, fds))) => {
                    let msg = match ipc::validate_service_fds(fds.len()) {
                        Ok(()) => msg,
                        Err(err) => ServiceToHelper::Error {
                            message: format!("invalid service message: {err}"),
                        },
                    };
                    if tx.send(Incoming::Message(msg)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Incoming::Lost(LOST.into()));
                    return;
                }
                Err(err) => {
                    let _ = tx.send(Incoming::Lost(format!("{LOST} ({err})")));
                    return;
                }
            }
        })?;

    let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<(HelperToService, Vec<OwnedFd>)>(256);
    {
        let sender = Arc::clone(&sender);
        std::thread::Builder::new()
            .name("splice-files-ipc-write".into())
            .spawn(move || {
                while let Ok((msg, fds)) = out_rx.recv() {
                    let refs: Vec<&OwnedFd> = fds.iter().collect();
                    if sender.lock().unwrap().send(&msg, &refs).is_err() {
                        return;
                    }
                }
            })?;
    }
    let lost: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let lost_reason: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let send: Rc<dyn Fn(HelperToService, Vec<OwnedFd>) -> bool> = {
        let lost = Rc::clone(&lost);
        let lost_reason = Rc::clone(&lost_reason);
        let connection = Rc::clone(&connection);
        Rc::new(move |msg: HelperToService, fds: Vec<OwnedFd>| {
            if lost.get() {
                return false;
            }
            match out_tx.try_send((msg, fds)) {
                Ok(()) => true,
                Err(_) => {
                    lost.set(true);
                    *lost_reason.borrow_mut() = Some("Splice stopped keeping up with the file shelf; close this window and open the file shelf again".into());
                    let _ = connection.shutdown();
                    tracing::warn!("outgoing shelf queue overflowed; disconnecting so the service releases this shelf's drags");
                    false
                }
            }
        })
    };

    let offers: Rc<RefCell<Armory<FileOfferId>>> = Rc::new(RefCell::new(Armory::new()));
    let receipts: Rc<RefCell<Armory<String>>> = Rc::new(RefCell::new(Armory::new()));
    let generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));

    let hooks = ShelfHooks {
        take_view: {
            let offers = Rc::clone(&offers);
            Rc::new(move |offer| offers.borrow_mut().take(&offer))
        },
        arm_view: {
            let send = Rc::clone(&send);
            let offers = Rc::clone(&offers);
            let receipts = Rc::clone(&receipts);
            Rc::new(move |offer| {
                let mut offers = offers.borrow_mut();
                let mut receipts = receipts.borrow_mut();
                if offers.want(offer) {
                    make_room(&mut offers, &mut receipts, &*send);
                    (send)(HelperToService::CreateView { offer }, Vec::new());
                }
            })
        },
        take_receipt_view: {
            let receipts = Rc::clone(&receipts);
            Rc::new(move |receipt| receipts.borrow_mut().take(&receipt))
        },
        arm_receipt_view: {
            let send = Rc::clone(&send);
            let offers = Rc::clone(&offers);
            let receipts = Rc::clone(&receipts);
            Rc::new(move |receipt: String| {
                let mut offers = offers.borrow_mut();
                let mut receipts = receipts.borrow_mut();
                if receipts.want(receipt.clone()) {
                    make_room(&mut offers, &mut receipts, &*send);
                    (send)(HelperToService::CreateReceiptView { receipt }, Vec::new());
                }
            })
        },
        drag_started: {
            let send = Rc::clone(&send);
            Rc::new(move |view| {
                (send)(HelperToService::DragStarted { view }, Vec::new());
            })
        },
        drop_performed: {
            let send = Rc::clone(&send);
            Rc::new(move |view| {
                (send)(HelperToService::DropPerformed { view }, Vec::new());
            })
        },
        drag_cancelled: {
            let send = Rc::clone(&send);
            Rc::new(move |view| {
                (send)(HelperToService::DragCancelled { view }, Vec::new());
            })
        },
        drag_finished: {
            let send = Rc::clone(&send);
            Rc::new(move |view| {
                (send)(HelperToService::DragFinished { view }, Vec::new());
            })
        },
        source_drop: {
            let send = Rc::clone(&send);
            Rc::new(move |recipient, paths, fds| {
                let msg = HelperToService::SourceDrop {
                    id: FileOfferId::new(),
                    recipient,
                    paths,
                    portal_fds: fds.len() as u32,
                };
                ipc::validate_outgoing(&msg, fds.len()).map_err(|error| {
                    format!("{error}; select fewer files or use a folder with a shorter path")
                })?;
                if (send)(msg, fds) {
                    Ok(())
                } else {
                    Err(LOST.into())
                }
            })
        },
        receive: {
            let send = Rc::clone(&send);
            let generation = Rc::clone(&generation);
            Rc::new(move |offer| {
                generation.set(generation.get() + 1);
                (send)(
                    HelperToService::ReceiveToClipboard {
                        offer,
                        generation: generation.get(),
                    },
                    Vec::new(),
                );
            })
        },
        save_to: {
            let send = Rc::clone(&send);
            Rc::new(move |offer, dest| {
                (send)(HelperToService::SaveTo { offer, dest }, Vec::new());
            })
        },
        dismiss: {
            let send = Rc::clone(&send);
            Rc::new(move |offer| {
                (send)(HelperToService::Dismiss { offer }, Vec::new());
            })
        },
        cancel_receive: Some({
            let send = Rc::clone(&send);
            Rc::new(move |offer| {
                (send)(HelperToService::CancelReceive { offer }, Vec::new());
            })
        }),
        retry_receive: Some({
            let send = Rc::clone(&send);
            Rc::new(move |offer| {
                (send)(HelperToService::RetryReceive { offer }, Vec::new());
            })
        }),
        republish: {
            let send = Rc::clone(&send);
            let generation = Rc::clone(&generation);
            Rc::new(move |receipt| {
                generation.set(generation.get() + 1);
                (send)(
                    HelperToService::RepublishReceipt {
                        receipt,
                        generation: generation.get(),
                    },
                    Vec::new(),
                );
            })
        },
        clear_receipt: {
            let send = Rc::clone(&send);
            Rc::new(move |receipt| {
                (send)(HelperToService::ClearReceipt { receipt }, Vec::new());
            })
        },
        reveal: {
            let send = Rc::clone(&send);
            Rc::new(move |receipt| {
                (send)(HelperToService::ReceiptPaths { receipt }, Vec::new());
            })
        },
    };

    let shelf = Shelf::build("Splice Files", hooks);
    let published = Rc::new(Cell::new(false));
    let ui = HelperUi {
        shelf: Rc::clone(&shelf),
        clipboard_published: Rc::clone(&published),
    };
    let offer_pages: Rc<RefCell<Vec<OfferDesc>>> = Rc::new(RefCell::new(Vec::new()));
    let receipt_pages: Rc<RefCell<Vec<ReceiptDesc>>> = Rc::new(RefCell::new(Vec::new()));
    let path_pages: Rc<RefCell<(String, Vec<PathBuf>)>> =
        Rc::new(RefCell::new((String::new(), Vec::new())));

    {
        let offers = Rc::clone(&offers);
        let receipts = Rc::clone(&receipts);
        let send = Rc::clone(&send);
        let lost = Rc::clone(&lost);
        glib::timeout_add_local(Duration::from_secs(15), move || {
            if lost.get() {
                return glib::ControlFlow::Break;
            }
            let mut offers = offers.borrow_mut();
            let mut receipts = receipts.borrow_mut();
            let mut released = Vec::new();
            for key in offers.idle(IDLE_RELEASE) {
                released.extend(offers.take(&key).map(|outcome| outcome.view));
            }
            for key in receipts.idle(IDLE_RELEASE) {
                released.extend(receipts.take(&key).map(|outcome| outcome.view));
            }
            for view in released {
                (send)(HelperToService::ReleaseView { view }, Vec::new());
            }
            glib::ControlFlow::Continue
        });
    }

    {
        let published = Rc::clone(&published);
        glib::timeout_add_local(Duration::from_millis(30), move || {
            let mut batch = Vec::new();
            let mut lost_now = lost_reason.borrow_mut().take();
            while batch.len() < 64 {
                match rx.try_recv() {
                    Ok(Incoming::Message(msg)) => batch.push(msg),
                    Ok(Incoming::Lost(reason)) => {
                        lost_now.get_or_insert(reason);
                        break;
                    }
                    Err(_) => break,
                }
            }
            let last_offers = batch
                .iter()
                .rposition(|msg| matches!(msg, ServiceToHelper::Offers { more: false, .. }));
            let last_receipts = batch
                .iter()
                .rposition(|msg| matches!(msg, ServiceToHelper::Receipts { more: false, .. }));
            let mut last_progress: HashMap<FileOfferId, usize> = HashMap::new();
            let mut last_error = None;
            for (index, msg) in batch.iter().enumerate() {
                match msg {
                    ServiceToHelper::Progress { offer, .. } => {
                        last_progress.insert(*offer, index);
                    }
                    ServiceToHelper::Error { message } => last_error = Some(message.clone()),
                    _ => {}
                }
            }
            for (index, msg) in batch.into_iter().enumerate() {
                match msg {
                    ServiceToHelper::Peers { peers } => shelf.set_peers(peers),
                    ServiceToHelper::Offers { offers: page, more } => {
                        offer_pages.borrow_mut().extend(page);
                        if more {
                            continue;
                        }
                        let list = std::mem::take(&mut *offer_pages.borrow_mut());
                        if last_offers.is_some_and(|last| last != index) {
                            continue;
                        }
                        let wanted: HashSet<FileOfferId> = list
                            .iter()
                            .filter(|offer| draggable(&offer.state))
                            .map(|offer| offer.offer)
                            .collect();
                        offers.borrow_mut().retain(&wanted);
                        shelf.set_offers(list);
                    }
                    ServiceToHelper::Receipts {
                        receipts: page,
                        more,
                    } => {
                        receipt_pages.borrow_mut().extend(page);
                        if more {
                            continue;
                        }
                        let list = std::mem::take(&mut *receipt_pages.borrow_mut());
                        if last_receipts.is_some_and(|last| last != index) {
                            continue;
                        }
                        let wanted: HashSet<String> = list
                            .iter()
                            .filter(|receipt| receipt.state == ReceiptStateDesc::Retained)
                            .map(|receipt| receipt.receipt.clone())
                            .collect();
                        receipts.borrow_mut().retain(&wanted);
                        shelf.set_receipts(list);
                    }
                    ServiceToHelper::ViewReady {
                        view,
                        offer,
                        uris,
                        portal_key,
                    } => {
                        offers.borrow_mut().ready(
                            offer,
                            ViewOutcome {
                                view,
                                uris,
                                portal_key,
                            },
                        );
                        shelf.clear_view_error(offer);
                    }
                    ServiceToHelper::ViewFailed { offer, error, .. } => {
                        offers.borrow_mut().failed(&offer);
                        shelf.set_view_error(offer, error.clone());
                        shelf.set_status(&format!("Cannot prepare files: {error}"));
                    }
                    ServiceToHelper::ReceiptViewReady {
                        receipt,
                        view,
                        uris,
                    } => {
                        receipts.borrow_mut().ready(
                            receipt,
                            ViewOutcome {
                                view,
                                uris,
                                portal_key: None,
                            },
                        );
                    }
                    ServiceToHelper::ReceiptViewFailed { receipt, error } => {
                        receipts.borrow_mut().failed(&receipt);
                        shelf.set_status(&format!("Cannot prepare files: {error}"));
                    }
                    ServiceToHelper::ReceiptPaths {
                        receipt,
                        paths,
                        more,
                    } => {
                        let mut acc = path_pages.borrow_mut();
                        if acc.0 != receipt {
                            acc.0 = receipt;
                            acc.1.clear();
                        }
                        acc.1.extend(paths);
                        if more {
                            continue;
                        }
                        let paths = std::mem::take(&mut acc.1);
                        acc.0.clear();
                        drop(acc);
                        shelf.reveal(&paths);
                    }
                    ServiceToHelper::Progress {
                        offer,
                        state,
                        done,
                        total,
                    } => {
                        if last_progress.get(&offer) != Some(&index) {
                            continue;
                        }
                        let text = match total {
                            Some(total) if total > 0 => {
                                format!(
                                    "{state} — {} of {} ({}%)",
                                    format_size(done),
                                    format_size(total),
                                    (done as u128 * 100 / total as u128).min(100)
                                )
                            }
                            _ => format!("{state} — {}", format_size(done)),
                        };
                        shelf.set_progress(offer, text);
                    }
                    ServiceToHelper::PublishClipboard {
                        offer,
                        generation: gen,
                        uris,
                        portal_key,
                    } => {
                        if gen != generation.get() {
                            shelf.set_status(
                                "Clipboard changed while receiving; received files stay in the shelf",
                            );
                            continue;
                        }
                        match publish_completed(&uris, portal_key.as_deref()) {
                            Ok(()) => {
                                published.set(true);
                                shelf.set_status("Files received — paste in any folder");
                                shelf.set_progress(offer, "Received to clipboard".into());
                            }
                            Err(err) => {
                                shelf.set_status(&format!("clipboard publish failed: {err}"))
                            }
                        }
                    }
                    ServiceToHelper::RetireView { view } => {
                        offers.borrow_mut().retire(view);
                        receipts.borrow_mut().retire(view);
                    }
                    ServiceToHelper::Present => shelf.window.present(),
                    ServiceToHelper::Error { message } => shelf.set_status(&message),
                }
            }
            if let Some(reason) = lost_now {
                lost.set(true);
                offers.borrow_mut().clear();
                receipts.borrow_mut().clear();
                shelf.set_offers(Vec::new());
                shelf.set_receipts(Vec::new());
                shelf.set_peers(Vec::new());
                match last_error {
                    Some(error) => shelf.set_status(&format!("{error} — {reason}")),
                    None => shelf.set_status(&reason),
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    run_window(&ui);
    Ok(())
}

pub struct PilotLog {
    file: Mutex<std::fs::File>,
}

impl PilotLog {
    pub fn new(path: &std::path::Path) -> Result<Arc<PilotLog>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Arc::new(PilotLog {
            file: Mutex::new(file),
        }))
    }

    pub fn log(&self, event: &str, fields: serde_json::Value) {
        use std::io::Write;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut map = match fields {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        map.insert("ts_ms".into(), serde_json::Value::from(ts));
        map.insert("event".into(), serde_json::Value::from(event));
        let line = serde_json::Value::Object(map).to_string();
        let _ = writeln!(self.file.lock().unwrap(), "{line}");
    }
}
