use super::{promises::Promises, state};
use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use splice_core::{files as core, EngineHandle};
use splice_platform::file_shelf as native;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

#[derive(Default)]
pub struct NativeState {
    pub shelf: Option<Arc<dyn native::FileShelf>>,
    pub error: Option<String>,
}

pub type NativeSlot = Arc<RwLock<NativeState>>;

struct ContextState {
    engine: EngineHandle,
    files: core::FileHandle,
    shelf: Arc<dyn native::FileShelf>,
    slot: NativeSlot,
    pins: RwLock<HashMap<core::TransferId, native::ReceivedSelection>>,
    pinning: Mutex<HashSet<core::TransferId>>,
    clearing: Mutex<HashSet<core::TransferId>>,
    scopes: Arc<RwLock<HashMap<core::TransferId, Vec<u32>>>>,
}

pub async fn attach(engine: EngineHandle, adapter: native::FileAdapter, slot: NativeSlot) {
    slot.write().shelf = Some(adapter.shelf.clone());
    let context = Arc::new(ContextState {
        files: engine.files(),
        engine,
        shelf: adapter.shelf,
        slot,
        pins: RwLock::new(HashMap::new()),
        pinning: Mutex::new(HashSet::new()),
        clearing: Mutex::new(HashSet::new()),
        scopes: Arc::new(RwLock::new(HashMap::new())),
    });
    tokio::spawn(run(context, adapter.events));
}

async fn run(context: Arc<ContextState>, mut events: tokio::sync::mpsc::Receiver<native::FileEvent>) {
    let mut files = context.files.state();
    let mut ui = context.engine.state();
    let mut jobs = JoinSet::<Result<()>>::new();
    let mut promises = Promises::new(context.files.clone(), context.scopes.clone());
    let mut ticker = tokio::time::interval(Duration::from_millis(150));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = None;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                if matches!(&event, native::FileEvent::SourceSelected { .. } | native::FileEvent::Receive { .. } | native::FileEvent::Republish { .. } | native::FileEvent::Save { .. } | native::FileEvent::Retry { .. } | native::FileEvent::ClearReceived { .. } | native::FileEvent::Cancel { .. } | native::FileEvent::Dismiss { .. }) {
                    context.slot.write().error = None;
                }
                match event {
                    native::FileEvent::ClipboardInvalidated { generation } => {
                        if clipboard_generation() == generation {
                            if let Err(error) = context.engine.invalidate_file_clipboard(generation) {
                                context.error(error);
                            }
                        }
                    }
                    native::FileEvent::ClipboardChanged { generation, mimes, inline_text } => {
                        if clipboard_generation() == generation {
                            if let Err(error) = context.engine.clipboard_changed(generation, mimes, inline_text) {
                                context.error(error);
                            }
                        }
                    }
                    native::FileEvent::ClipboardFiles { paths, generation, lease } => {
                        if clipboard_generation() == generation {
                            let selection = core::LocalSelection {
                                roots: paths.into_iter().map(core::SelectedRoot::Path).collect(),
                                access: Some(lease),
                            };
                            if let Err(error) = context.engine.capture_file_clipboard(selection, generation) {
                                context.error(error);
                            }
                        }
                    }
                    native::FileEvent::DragStarted { attempt, offer, roots } => {
                        if let Err(error) = promises.register(attempt, offer, roots) { context.error(error); }
                    }
                    native::FileEvent::DragEnded { attempt, outcome } => promises.ended(attempt, outcome),
                    native::FileEvent::DragRetired { attempt } => promises.retired(attempt),
                    native::FileEvent::PromiseFinished { result, .. } => {
                        if let Err(error) = result { context.error(anyhow::Error::msg(error)); }
                    }
                    native::FileEvent::Overflow { dropped } => context.error(anyhow::anyhow!("The native file event queue was full; {dropped} actions could not be handled")),
                    native::FileEvent::ClearReceived { transfer } => {
                        let transfer = core::TransferId(transfer.0);
                        if context.clearing.lock().insert(transfer) {
                            context.pins.write().remove(&transfer);
                            let snapshot = snapshot(&context, &ui.borrow(), &files.borrow());
                            context.shelf.sync(snapshot.clone());
                            last = Some(snapshot);
                            let context = context.clone();
                            jobs.spawn(async move {
                                let result = async {
                                    main_queue_barrier().await?;
                                    context.files.request(core::FileCommand::ClearReceived { transfer }).await
                                }.await;
                                context.clearing.lock().remove(&transfer);
                                result.map(|_| ())
                            });
                        }
                    }
                    event @ native::FileEvent::Dismiss { .. } => {
                        jobs.spawn(action(context.clone(), event));
                    }
                    native::FileEvent::Cancel { transfer } => {
                        promises.cancel_transfer(core::TransferId(transfer.0));
                        jobs.spawn(action(context.clone(), native::FileEvent::Cancel { transfer }));
                    }
                    native::FileEvent::PromiseRequested { attempt, root, writer, .. } => {
                        if jobs.len() >= 128 {
                            writer.complete(Err("Too many file operations are waiting".into()));
                        } else {
                            jobs.spawn(promises.requested(attempt, root, writer));
                        }
                    }
                    event => {
                        if jobs.len() >= 128 {
                            context.error(anyhow::anyhow!("Finish or cancel an existing file operation first"));
                        } else {
                            jobs.spawn(action(context.clone(), event));
                        }
                    }
                }
            }
            result = jobs.join_next(), if !jobs.is_empty() => {
                match result {
                    Some(Ok(Err(error))) => context.error(error),
                    Some(Err(error)) => context.error(error.into()),
                    _ => {}
                }
            }
            _ = ticker.tick() => {
            }
            changed = files.changed() => { if changed.is_err() { break; } }
            changed = ui.changed() => { if changed.is_err() { break; } }
        }
        pin_ready(&context, &files.borrow(), &mut jobs);
        let snapshot = snapshot(&context, &ui.borrow(), &files.borrow());
        if last.as_ref() != Some(&snapshot) {
            context.shelf.sync(snapshot.clone());
            last = Some(snapshot);
        }
    }
    jobs.abort_all();
    context.slot.write().shelf = None;
}

impl ContextState {
    fn error(&self, error: anyhow::Error) {
        tracing::warn!(%error, "file shelf operation failed");
        self.slot.write().error = Some(format!("{error:#}"));
    }
}

async fn action(context: Arc<ContextState>, event: native::FileEvent) -> Result<()> {
    match event {
        native::FileEvent::SourceSelected { paths, recipient, lease, .. } => {
            let selection = core::LocalSelection {
                roots: paths.into_iter().map(core::SelectedRoot::Path).collect(),
                access: Some(lease),
            };
            context.files.offer_local(selection, recipient, core::OfferOrigin::Selection).await?;
        }
        native::FileEvent::Receive { offer, clipboard_generation, intent } => {
            let transfer = receive(&context.files, offer, core::ReceiveDestination::Cache).await?;
            context.files.wait(transfer).await?;
            let lease = context.files.retain_received(transfer).await?;
            context.shelf.publish_clipboard_files(received_selection(lease), clipboard_generation, intent)?;
        }
        native::FileEvent::Republish { transfer, clipboard_generation, intent } => {
            let lease = context.files.retain_received(core::TransferId(transfer.0)).await?;
            context.shelf.publish_clipboard_files(received_selection(lease), clipboard_generation, intent)?;
        }
        native::FileEvent::Save { offer, directory, lease } => {
            let result = async {
                let transfer = receive(&context.files, offer, core::ReceiveDestination::Directory(directory)).await?;
                context.files.wait(transfer).await
            }.await;
            drop(lease);
            result?;
        }
        native::FileEvent::Dismiss { offer } => {
            context.files.request(core::FileCommand::Revoke { offer: core::FileOfferId(offer.0) }).await?;
        }
        native::FileEvent::Cancel { transfer } => {
            context.files.request(core::FileCommand::Cancel { transfer: core::TransferId(transfer.0) }).await?;
        }
        native::FileEvent::Retry { transfer } => {
            context.files.request(core::FileCommand::Retry { transfer: core::TransferId(transfer.0) }).await?;
        }
        native::FileEvent::ClipboardFiles { .. }
        | native::FileEvent::ClipboardInvalidated { .. }
        | native::FileEvent::ClipboardChanged { .. }
        | native::FileEvent::DragStarted { .. }
        | native::FileEvent::DragEnded { .. }
        | native::FileEvent::DragRetired { .. }
        | native::FileEvent::PromiseFinished { .. }
        | native::FileEvent::Overflow { .. }
        | native::FileEvent::ClearReceived { .. }
        | native::FileEvent::PromiseRequested { .. } => anyhow::bail!("Invalid asynchronous file shelf action"),
    }
    Ok(())
}

async fn receive(files: &core::FileHandle, offer: native::OfferId, destination: core::ReceiveDestination) -> Result<core::TransferId> {
    let reply = files.request(core::FileCommand::Receive { offer: core::FileOfferId(offer.0), destination }).await?;
    match reply {
        core::FileReply::Receiving(transfer) => Ok(transfer),
        _ => Err(anyhow::anyhow!("Unexpected file receipt response")).context("Receiving files"),
    }
}

fn clipboard_generation() -> u64 {
    objc2::rc::autoreleasepool(|_| objc2_app_kit::NSPasteboard::generalPasteboard().changeCount()) as u64
}

fn received_selection(lease: core::ReceivedLease) -> native::ReceivedSelection {
    native::ReceivedSelection::new(
        native::TransferId(lease.files().transfer.0),
        lease.files().paths.clone(),
        Some(Arc::new(lease)),
    )
}

fn pin_ready(context: &Arc<ContextState>, state: &core::FileState, jobs: &mut JoinSet<Result<()>>) {
    context.pins.write().retain(|id, _| state.transfers.iter().any(|transfer| transfer.id == *id));
    context.scopes.write().retain(|id, _| state.transfers.iter().any(|transfer| transfer.id == *id));
    for transfer in &state.transfers {
        if jobs.len() >= 16 { break; }
        if transfer.direction != core::TransferDirection::Receive || transfer.state != core::TransferState::Ready { continue; }
        if context.clearing.lock().contains(&transfer.id) { continue; }
        if context.pins.read().contains_key(&transfer.id) || !context.pinning.lock().insert(transfer.id) { continue; }
        let context = context.clone();
        let transfer = transfer.id;
        jobs.spawn(async move {
            let result = context.files.retain_received(transfer).await;
            context.pinning.lock().remove(&transfer);
            match result {
                Ok(lease) => {
                    let clearing = context.clearing.lock();
                    if !clearing.contains(&transfer) {
                        context.pins.write().insert(transfer, received_selection(lease));
                    }
                    Ok(())
                }
                Err(error) => {
                    if context.files.state().borrow().transfers.iter().any(|record| record.id == transfer && record.state == core::TransferState::Ready) {
                        Err(error)
                    } else { Ok(()) }
                }
            }
        });
    }
}

fn snapshot(context: &ContextState, ui: &splice_core::UiState, files: &core::FileState) -> native::ShelfSnapshot {
    state::snapshot(ui, files, &context.pins.read(), &context.scopes.read(), context.slot.read().error.clone())
}

async fn main_queue_barrier() -> Result<()> {
    let (done, wait) = tokio::sync::oneshot::channel();
    dispatch2::DispatchQueue::main().exec_async(move || { let _ = done.send(()); });
    wait.await.context("The native file shelf stopped before releasing its receipt")
}
