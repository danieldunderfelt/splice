mod commands;
mod control;

use super::{
    offers::*,
    storage,
    transport::{self, Accepted, Guard},
    *,
};
use anyhow::{Context, Result};
use splice_proto::{
    files::{FileMessage, Offer, FILE_PORT, OFFER_LIFETIME_MS},
    Frame,
};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, task::JoinSet};

struct Service {
    clipboard: super::clipboard::Receiver,
    id: MachineId,
    net: crate::net::NetControl,
    ts: Arc<dyn crate::net::TsApi>,
    base: PathBuf,
    policy: watch::Receiver<Policy>,
    state: watch::Sender<FileState>,
    summary: watch::Sender<FileSummary>,
    enabled: bool,
    setting_pending: bool,
    offers: HashMap<FileOfferId, RegisteredOffer>,
    transfers: HashMap<TransferId, Delivery>,
    drags: HashMap<NativeDragId, Drag>,
    drop_replies: HashMap<NativeDragId, (Instant, oneshot::Sender<Result<FileReply, String>>)>,
    enumerations: HashMap<u64, Enumeration>,
    enumeration_id: u64,
    clipboard_generation: Option<u64>,
    clipboard_selection: Option<LocalSelection>,
    pins: HashMap<TransferId, std::sync::Weak<()>>,
    clearing: std::collections::HashSet<TransferId>,
    retired_sent: u64,
    retired_received: u64,
    jobs: JoinSet<Completed>,
    error: Option<String>,
    recovery_errors: Vec<RecoveryIssue>,
    recovery_error_count: usize,
    untracked_cache_bytes: u64,
    cleanup_failures: std::collections::HashSet<TransferId>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    id: MachineId,
    net: crate::net::NetControl,
    ts: Arc<dyn crate::net::TsApi>,
    data_dir: PathBuf,
    mut commands: mpsc::Receiver<Request>,
    mut incoming: mpsc::Receiver<(MachineId, u64, FileMessage)>,
    clipboard: super::clipboard::Receiver,
    policy: watch::Receiver<Policy>,
    state: watch::Sender<FileState>,
    summary: watch::Sender<FileSummary>,
    wire: WireSender,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let base = data_dir.join("files");
    let cache = base.clone();
    let (recovered, enabled) = tokio::task::spawn_blocking(move || -> Result<_> {
        Ok((storage::initialize(&cache)?, storage::load_enabled(&cache)?))
    })
    .await??;
    let listener = TcpListener::bind(std::net::SocketAddr::new(net.bind_ip(), FILE_PORT)).await?;
    net.file_receiver(Some(wire));
    let _ = ready.send(());
    let (accepted_tx, mut accepted) = mpsc::channel(8);
    let server = tokio::spawn(transport::listen(listener, ts.clone(), accepted_tx));
    let mut service = Service {
        clipboard,
        id,
        net,
        ts,
        base,
        policy,
        state,
        summary,
        enabled,
        setting_pending: false,
        offers: HashMap::new(),
        transfers: HashMap::new(),
        drags: HashMap::new(),
        drop_replies: HashMap::new(),
        enumerations: HashMap::new(),
        enumeration_id: 0,
        clipboard_generation: None,
        clipboard_selection: None,
        pins: HashMap::new(),
        clearing: std::collections::HashSet::new(),
        retired_sent: 0,
        retired_received: 0,
        jobs: JoinSet::new(),
        error: None,
        cleanup_failures: recovered.cleanup_failures,
        recovery_errors: recovered.errors,
        recovery_error_count: recovered.error_count,
        untracked_cache_bytes: recovered.untracked_cache_bytes,
    };
    for journal in recovered.journals {
        service.transfers.insert(
            journal.record.id,
            Delivery {
                manifest: journal.manifest,
                destination: Some(journal.destination),
                generation: 0,
                epoch: 0,
                cancel: Cancellation::default(),
                bytes: Arc::new(AtomicU64::new(journal.record.bytes)),
                record: journal.record,
                token: None,
                source: None,
                deadline: Instant::now(),
                running: false,
                receiver_ready: false,
            },
        );
    }
    service.publish();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            changed = service.policy.changed() => {
                if changed.is_err() { break; }
                service.enforce();
            }
            update = service.clipboard.recv() => {
                let Some(update) = update else { break; };
                service.apply_clipboard(update);
            }
            command = commands.recv() => {
                let Some(request) = command else { break; };
                service.command(request);
            }
            message = incoming.recv() => {
                let Some((peer, generation, message)) = message else { break; };
                if let Err(error) = service.message(&peer, generation, message.clone()) {
                    let (transfer, drag) = match message {
                        FileMessage::Commit { transfer, .. } => (Some(transfer), None),
                        FileMessage::Prepare { drag, .. } | FileMessage::Dropped { drag } => (None, Some(drag)),
                        _ => (None, None),
                    };
                    if transfer.is_some() || drag.is_some() {
                        let _ = service.send(&peer, FileMessage::Reject { transfer, drag, reason: short_error(&error) });
                    }
                    service.error = Some(short_error(&error));
                }
            }
            connection = accepted.recv() => {
                let Some(connection) = connection else { anyhow::bail!("file listener stopped"); };
                if let Err(error) = service.accept(connection) { tracing::debug!(%error, "file connection rejected"); }
            }
            result = service.jobs.join_next(), if !service.jobs.is_empty() => {
                match result {
                    Some(Ok(completed)) => service.completed(completed),
                    Some(Err(error)) => {
                        service.error = Some(format!("file worker failed: {error}"));
                        service.cancel_all("file worker failed");
                    }
                    None => {}
                }
            }
            _ = tick.tick() => service.maintain(),
        }
        service.publish();
    }
    service.cancel_all("file service stopped");
    service.publish();
    service.net.file_receiver(None);
    server.abort();
    while let Some(result) = service.jobs.join_next().await {
        if let Ok(completed) = result {
            service.completed(completed);
        }
    }
    service.publish();
    Ok(())
}

fn short_error(error: &anyhow::Error) -> String {
    let mut message = format!("{error:#}");
    let mut length = message.len().min(1024);
    while !message.is_char_boundary(length) {
        length -= 1;
    }
    message.truncate(length);
    message
}

impl Service {
    fn epoch(&self, peer: &MachineId) -> u64 {
        self.policy.borrow().epochs.get(peer).copied().unwrap_or(0)
    }

    fn authorize(&self, peer: &MachineId) -> Result<u64> {
        let policy = self.policy.borrow();
        anyhow::ensure!(self.enabled && policy.enabled, "file sharing disabled");
        let (generation, _) = policy
            .peers
            .get(peer)
            .context("peer is not authorized for file sharing")?;
        anyhow::ensure!(
            self.net.current_connection(peer, *generation),
            "file control connection changed"
        );
        Ok(*generation)
    }

    fn guard(&self, peer: &MachineId, generation: u64, cancel: Cancellation) -> Guard {
        Guard {
            cancel,
            policy: self.policy.clone(),
            net: self.net.clone(),
            peer: peer.clone(),
            generation,
            epoch: self.epoch(peer),
        }
    }

    fn send(&self, peer: &MachineId, message: FileMessage) -> Result<()> {
        self.authorize(peer)?;
        message.validate()?;
        anyhow::ensure!(
            self.net.send_to(peer, Frame::Files(message)),
            "file control queue unavailable"
        );
        Ok(())
    }

    fn available(&self, id: FileOfferId) -> Result<&RegisteredOffer> {
        let offer = self.offers.get(&id).context("unknown file offer")?;
        anyhow::ensure!(
            offer.record.state == OfferState::Available && offer.record.expires_unix_ms > now(),
            "file offer revoked or expired"
        );
        let peer = if offer.record.owner == self.id {
            &offer.record.recipient
        } else {
            &offer.record.owner
        };
        anyhow::ensure!(
            self.authorize(peer)? == offer.generation && self.epoch(peer) == offer.epoch,
            "offer belongs to a previous peer connection"
        );
        Ok(offer)
    }

    fn capacity(&mut self, peer: &MachineId) -> Result<()> {
        self.retire_history();
        anyhow::ensure!(
            self.transfers.len() < storage::MAX_RECORDS,
            "retained transfer record limit reached"
        );
        let active: Vec<_> = self
            .transfers
            .values()
            .filter(|d| !d.record.state.terminal() || d.running)
            .collect();
        anyhow::ensure!(
            active.len() < MAX_ACTIVE
                && active.iter().filter(|d| &d.record.peer == peer).count() < MAX_PEER_ACTIVE,
            "file transfer concurrency limit reached"
        );
        Ok(())
    }

    fn retain_received(&mut self, transfer: TransferId) -> Result<ReceivedLease> {
        let delivery = self.transfers.get(&transfer).context("unknown transfer")?;
        anyhow::ensure!(
            delivery.record.direction == TransferDirection::Receive
                && delivery.record.state == TransferState::Ready
                && !delivery.running
                && !self.clearing.contains(&transfer),
            "receive is not ready for retention"
        );
        let pin = self
            .pins
            .get(&transfer)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| Arc::new(()));
        self.pins.insert(transfer, Arc::downgrade(&pin));
        Ok(ReceivedLease {
            files: ReceivedFiles {
                transfer,
                paths: delivery.record.paths.clone(),
            },
            manifest: Arc::new(delivery.manifest.clone()),
            _pin: pin,
        })
    }

    fn clear_received(
        &mut self,
        transfer: TransferId,
        reply: Option<oneshot::Sender<Result<FileReply, String>>>,
    ) {
        let result = (|| {
            anyhow::ensure!(
                self.clearing.len() < 2,
                "receive cleanup concurrency limit reached"
            );
            let delivery = self.transfers.get(&transfer).context("unknown transfer")?;
            anyhow::ensure!(
                delivery.record.direction == TransferDirection::Receive
                    && delivery.record.state.terminal()
                    && !delivery.running,
                "receive is still active"
            );
            anyhow::ensure!(
                !self.clearing.contains(&transfer)
                    && self
                        .pins
                        .get(&transfer)
                        .is_none_or(|pin| pin.strong_count() == 0),
                "receive is retained by a native consumer or already clearing"
            );
            Ok(())
        })();
        if let Err(error) = result {
            self.reply(reply, Err(error));
            return;
        }
        self.clearing.insert(transfer);
        let base = self.base.clone();
        self.jobs.spawn(async move {
            let result =
                tokio::task::spawn_blocking(move || storage::clear_received(&base, transfer))
                    .await
                    .unwrap_or_else(|error| {
                        Err(storage::ClearFailure {
                            record: None,
                            error: error.into(),
                        })
                    });
            Completed::Cleared(transfer, reply, result)
        });
    }

    fn remove_record(&mut self, id: TransferId) {
        if let Some(delivery) = self.transfers.remove(&id) {
            match delivery.record.direction {
                TransferDirection::Send => {
                    self.retired_sent += delivery.bytes.load(Ordering::Relaxed)
                }
                TransferDirection::Receive => {
                    self.retired_received += delivery.bytes.load(Ordering::Relaxed)
                }
            }
        }
        self.pins.remove(&id);
        self.cleanup_failures.remove(&id);
        let previous = self.recovery_errors.len();
        self.recovery_errors
            .retain(|issue| issue.transfer != Some(id));
        self.recovery_error_count = self
            .recovery_error_count
            .saturating_sub(previous - self.recovery_errors.len());
        self.drags.retain(|_, d| d.transfer != Some(id));
    }

    fn retire_history(&mut self) {
        if self.transfers.len() < 192 {
            return;
        }
        let mut candidates: Vec<_> = self
            .transfers
            .values()
            .filter(|d| {
                d.record.state.terminal()
                    && !self.cleanup_failures.contains(&d.record.id)
                    && !d.running
                    && (d.record.direction == TransferDirection::Send || d.record.paths.is_empty())
            })
            .map(|d| (d.deadline, d.record.id, d.record.direction))
            .collect();
        candidates.sort_by_key(|d| d.0);
        for (_, id, direction) in candidates
            .into_iter()
            .take(self.transfers.len().saturating_sub(128))
        {
            if direction == TransferDirection::Send {
                self.remove_record(id);
            } else if !self.clearing.contains(&id) {
                if self.clearing.len() >= 2 {
                    break;
                }
                self.clear_received(id, None);
            }
        }
    }

    fn retire_drags(&mut self) {
        for drag in self.drags.values_mut() {
            if drag.transfer.is_some_and(|id| {
                self.transfers
                    .get(&id)
                    .is_none_or(|d| d.record.state.terminal() && !d.running)
            }) {
                drag.source = None;
                drag.manifest.entries.clear();
            }
        }
        if self.drags.len() >= 256 {
            self.drags.retain(|_, d| !d.manifest.entries.is_empty());
        }
    }

    fn drag_capacity(&mut self, peer: &MachineId) -> Result<()> {
        self.retire_drags();
        let active: Vec<_> = self
            .drags
            .values()
            .filter(|d| !d.manifest.entries.is_empty())
            .collect();
        anyhow::ensure!(
            active.len() < MAX_DRAGS && active.iter().filter(|d| &d.peer == peer).count() < 8,
            "native drag lease limit reached"
        );
        Ok(())
    }

    fn offer_capacity(&mut self, peer: &MachineId) -> Result<()> {
        if self.offers.len() + self.enumerations.len() >= MAX_OFFERS {
            self.offers
                .retain(|_, o| o.record.state == OfferState::Available);
        }
        anyhow::ensure!(
            self.offers.len() + self.enumerations.len() < MAX_OFFERS,
            "file offer registry full; revoke old offers"
        );
        let count = self
            .offers
            .values()
            .filter(|o| {
                o.record.state == OfferState::Available
                    && (&o.record.owner == peer || &o.record.recipient == peer)
            })
            .count()
            + self
                .enumerations
                .values()
                .filter(|e| &e.recipient == peer)
                .count();
        anyhow::ensure!(count < MAX_PEER_OFFERS, "peer file offer limit reached");
        Ok(())
    }

    fn completed(&mut self, completed: Completed) {
        self.sync_clipboard();
        match completed {
            Completed::Cleared(id, reply, result) => {
                self.clearing.remove(&id);
                let result = match result {
                    Ok(()) => {
                        self.remove_record(id);
                        Ok(FileReply::Done)
                    }
                    Err(failure) => {
                        if let Some(delivery) = self.transfers.get_mut(&id) {
                            if let Some(record) = failure.record {
                                delivery.record = *record;
                            }
                            delivery.record.error = Some(short_error(&failure.error));
                            self.cleanup_failures.insert(id);
                        }
                        Err(failure.error)
                    }
                };
                self.publish();
                self.reply(reply, result);
            }
            Completed::Enabled(enabled, reply, result) => {
                self.setting_pending = false;
                self.enabled = result.is_ok() && enabled;
                self.enforce();
                self.publish();
                self.reply(reply, result.map(|_| FileReply::Done));
            }
            Completed::Enumerated(id, result) => {
                let Some(enumeration) = self.enumerations.remove(&id) else {
                    return;
                };
                let result = result.and_then(|selection| {
                    enumeration.cancellation.check()?;
                    anyhow::ensure!(
                        self.authorize(&enumeration.recipient)? == enumeration.generation
                            && self.epoch(&enumeration.recipient) == enumeration.epoch,
                        "recipient changed during enumeration"
                    );
                    let id = FileOfferId(random()?);
                    let offer = Offer {
                        id,
                        owner: self.id.clone(),
                        recipient: enumeration.recipient.clone(),
                        origin: enumeration.origin,
                        manifest: selection.manifest.clone(),
                        expires_unix_ms: now() + OFFER_LIFETIME_MS,
                    };
                    self.send(&enumeration.recipient, FileMessage::Offer(offer.clone()))?;
                    let record = OfferRecord {
                        id,
                        owner: offer.owner,
                        recipient: offer.recipient,
                        origin: offer.origin,
                        manifest: offer.manifest,
                        expires_unix_ms: offer.expires_unix_ms.min(now() + OFFER_LIFETIME_MS),
                        state: OfferState::Available,
                    };
                    self.offers.insert(
                        id,
                        RegisteredOffer {
                            record,
                            source: Some(Arc::new(selection)),
                            generation: enumeration.generation,
                            epoch: enumeration.epoch,
                        },
                    );
                    Ok(FileReply::Offered(id))
                });
                self.publish();
                self.reply(enumeration.reply, result);
            }
            Completed::Sent(id, result) => {
                let Some(delivery) = self.transfers.get_mut(&id) else {
                    return;
                };
                delivery.running = false;
                delivery.source = None;
                delivery.record.bytes = delivery.bytes.load(Ordering::Relaxed);
                if delivery.cancel.check().is_err() {
                    delivery.record.state = TransferState::Cancelled;
                }
                if !delivery.record.state.terminal() {
                    match result {
                        Ok(()) => {
                            delivery.record.state = TransferState::Verifying;
                            delivery.deadline = Instant::now() + transport::DEADLINE;
                            if delivery.receiver_ready {
                                delivery.record.state = TransferState::Ready;
                            }
                        }
                        Err(error) => {
                            delivery.record.state = TransferState::Failed;
                            delivery.record.error = Some(short_error(&error));
                        }
                    }
                }
            }
            Completed::Received(id, result) => {
                let Some(delivery) = self.transfers.get_mut(&id) else {
                    return;
                };
                delivery.running = false;
                match result {
                    Ok(outcome) => {
                        if outcome.cleanup_failed {
                            self.cleanup_failures.insert(id);
                        }
                        delivery.record = outcome.journal.record;
                    }
                    Err(error) => {
                        delivery.record.state = if delivery.cancel.check().is_err() {
                            TransferState::Cancelled
                        } else {
                            TransferState::Failed
                        };
                        delivery.record.error = Some(short_error(&error));
                        delivery.record.bytes = delivery.bytes.load(Ordering::Relaxed);
                    }
                }
                let message = FileMessage::Result {
                    transfer: id,
                    error: delivery.record.error.clone(),
                };
                let peer = delivery.record.peer.clone();
                let _ = self.send(&peer, message);
            }
        }
    }

    fn revoke(&mut self, offer: FileOfferId, clipboard_only: bool) {
        if let Some(registered) = self.offers.get_mut(&offer) {
            registered.record.state = OfferState::Revoked;
            registered.source = None;
        }
        let drags: Vec<_> = self
            .drags
            .iter()
            .filter(|(_, d)| d.offer == offer && (!clipboard_only || !d.dropped))
            .map(|(id, _)| *id)
            .collect();
        for id in drags {
            self.drags.remove(&id);
        }
        if !clipboard_only {
            let transfers: Vec<_> = self
                .transfers
                .values()
                .filter(|d| d.record.offer == offer)
                .map(|d| d.record.id)
                .collect();
            for id in transfers {
                self.cancel(id, "file offer explicitly revoked");
            }
        }
    }

    fn cancel(&mut self, id: TransferId, reason: &str) {
        if let Some(delivery) = self.transfers.get_mut(&id) {
            if delivery.record.state.terminal() {
                return;
            }
            delivery.cancel.cancel();
            delivery.token = None;
            delivery.source = None;
            if !delivery.running {
                delivery.record.state = TransferState::Cancelled;
            }
            delivery.record.error = Some(reason.to_string());
        }
    }

    fn fail(&mut self, id: TransferId, reason: &str) {
        if self
            .transfers
            .get(&id)
            .is_none_or(|d| d.record.state.terminal())
        {
            return;
        }
        self.cancel(id, reason);
        if let Some(delivery) = self.transfers.get_mut(&id) {
            if !delivery.running {
                delivery.record.state = TransferState::Failed;
            }
        }
    }

    fn cancel_all(&mut self, reason: &str) {
        for enumeration in self.enumerations.values() {
            enumeration.cancellation.cancel();
        }
        let ids: Vec<_> = self.transfers.keys().copied().collect();
        for id in ids {
            self.cancel(id, reason);
        }
        self.drags.clear();
        for (_, (_, reply)) in self.drop_replies.drain() {
            let _ = reply.send(Err(reason.to_string()));
        }
    }

    fn enforce(&mut self) {
        let rejected: Vec<_> = self
            .transfers
            .iter()
            .filter(|(_, d)| {
                self.authorize(&d.record.peer).ok() != Some(d.generation)
                    || self.epoch(&d.record.peer) != d.epoch
            })
            .map(|(id, _)| *id)
            .collect();
        for id in rejected {
            self.fail(id, "file authorization revoked or connection changed");
        }
        let policy = self.policy.borrow().clone();
        if !self.enabled || !policy.enabled || !policy.clipboard_enabled {
            self.clipboard_selection = None;
            self.clipboard_generation = None;
        }
        let revoked: Vec<_> = self
            .offers
            .values()
            .filter(|o| {
                let peer = if o.record.owner == self.id {
                    &o.record.recipient
                } else {
                    &o.record.owner
                };
                self.authorize(peer).ok() != Some(o.generation)
                    || self.epoch(peer) != o.epoch
                    || !self.enabled
                    || !policy.enabled
                    || (!policy.clipboard_enabled
                        && matches!(o.record.origin, OfferOrigin::Clipboard { .. }))
            })
            .map(|o| o.record.id)
            .collect();
        for offer in revoked {
            self.revoke(offer, false);
        }
        for enumeration in self.enumerations.values() {
            if self.authorize(&enumeration.recipient).ok() != Some(enumeration.generation)
                || self.epoch(&enumeration.recipient) != enumeration.epoch
                || enumeration
                    .reply
                    .as_ref()
                    .is_some_and(|reply| reply.is_closed())
            {
                enumeration.cancellation.cancel();
            }
        }
        let stale: Vec<_> = self
            .drags
            .iter()
            .filter(|(_, d)| {
                self.authorize(&d.peer).ok() != Some(d.generation) || self.epoch(&d.peer) != d.epoch
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.drags.remove(&id);
        }
    }

    fn maintain(&mut self) {
        self.enforce();
        self.retire_drags();
        self.retire_history();
        let now = now();
        for registered in self.offers.values_mut() {
            if registered.record.state == OfferState::Available
                && registered.record.expires_unix_ms <= now
            {
                registered.record.state = OfferState::Expired;
                registered.source = None;
            }
        }
        self.drags.retain(|_, d| d.expires > now);
        let pending: Vec<_> = self
            .drop_replies
            .iter()
            .filter(|(id, (deadline, _))| {
                *deadline <= Instant::now() || !self.drags.contains_key(id)
            })
            .map(|(id, _)| *id)
            .collect();
        for drag in pending {
            let _ = self.action(FileCommand::CancelDrag { drag });
            if let Some((_, reply)) = self.drop_replies.remove(&drag) {
                self.reply(
                    Some(reply),
                    Err(anyhow::anyhow!(
                        "native drop confirmation expired or cancelled"
                    )),
                );
            }
        }
        let expired: Vec<_> = self
            .transfers
            .values()
            .filter(|d| !d.running && !d.record.state.terminal() && d.deadline <= Instant::now())
            .map(|d| d.record.id)
            .collect();
        for id in expired {
            self.fail(id, "file grant or durable result deadline exceeded");
        }
    }

    fn publish(&self) {
        let mut offers: Vec<_> = self.offers.values().map(|o| o.record.clone()).collect();
        offers.sort_by_key(|o| o.id);
        let mut transfers: Vec<_> = self
            .transfers
            .values()
            .map(|d| {
                let mut r = d.record.clone();
                r.bytes = d.bytes.load(Ordering::Relaxed);
                if r.state == TransferState::Receiving && r.bytes == r.total_bytes {
                    r.state = TransferState::Verifying;
                }
                r
            })
            .collect();
        transfers.sort_by_key(|d| d.id);
        let sent: u64 = transfers
            .iter()
            .filter(|d| d.direction == TransferDirection::Send)
            .map(|d| d.bytes)
            .sum();
        let received: u64 = transfers
            .iter()
            .filter(|d| d.direction == TransferDirection::Receive)
            .map(|d| d.bytes)
            .sum();
        let next = FileState {
            recovery_errors: self.recovery_errors.clone(),
            recovery_error_count: self.recovery_error_count,
            offers,
            transfers,
            error: self.error.clone(),
            enabled: self.enabled && self.policy.borrow().enabled,
            payload_bytes_sent: sent + self.retired_sent,
            payload_bytes_received: received + self.retired_received,
        };
        let summary = FileSummary::from(&next);
        self.summary.send_if_modified(|current| {
            if *current == summary {
                false
            } else {
                *current = summary;
                true
            }
        });
        self.state.send_if_modified(|state| {
            if *state == next {
                false
            } else {
                *state = next;
                true
            }
        });
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.cancel_all("file service stopped");
    }
}
