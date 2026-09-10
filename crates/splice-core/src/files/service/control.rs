use super::*;

impl Service {
    pub(super) fn message(
        &mut self,
        peer: &MachineId,
        generation: u64,
        message: FileMessage,
    ) -> Result<()> {
        self.sync_clipboard();
        anyhow::ensure!(
            self.authorize(peer)? == generation,
            "stale file control frame"
        );
        message.validate()?;
        match message {
            FileMessage::Offer(offer) => {
                anyhow::ensure!(
                    &offer.owner == peer && offer.recipient == self.id,
                    "file offer identity mismatch"
                );
                anyhow::ensure!(
                    offer.expires_unix_ms > now()
                        && offer.expires_unix_ms <= now() + OFFER_LIFETIME_MS + 300_000,
                    "invalid file offer lifetime"
                );
                if self.offers.contains_key(&offer.id) {
                    anyhow::bail!("replayed file offer");
                }
                self.offer_capacity(peer)?;
                let record = OfferRecord {
                    id: offer.id,
                    owner: offer.owner,
                    recipient: offer.recipient,
                    origin: offer.origin,
                    manifest: offer.manifest,
                    expires_unix_ms: offer.expires_unix_ms.min(now() + OFFER_LIFETIME_MS),
                    state: OfferState::Available,
                };
                self.offers.insert(
                    record.id,
                    RegisteredOffer {
                        epoch: self.epoch(peer),
                        record,
                        source: None,
                        generation,
                    },
                );
            }
            FileMessage::Revoke {
                offer,
                unaccepted_only,
            } => {
                let record = self.offers.get(&offer).context("unknown revoked offer")?;
                anyhow::ensure!(
                    &record.record.owner == peer || &record.record.recipient == peer,
                    "revoke peer mismatch"
                );
                self.revoke(offer, unaccepted_only);
            }
            FileMessage::Prepare {
                offer,
                generation: version,
                drag,
                roots,
            } => {
                self.drag_capacity(peer)?;
                anyhow::ensure!(
                    !self.drags.contains_key(&drag),
                    "native drag lease limit or replay"
                );
                let registered = self.available(offer)?;
                anyhow::ensure!(
                    &registered.record.recipient == peer
                        && registered.record.owner == self.id
                        && registered.record.manifest.generation == version,
                    "native prepare scope mismatch"
                );
                let manifest = registered.record.manifest.select(&roots)?;
                let source = registered
                    .source
                    .clone()
                    .context("missing local source selection")?;
                self.drags.insert(
                    drag,
                    Drag {
                        epoch: self.epoch(peer),
                        offer,
                        roots,
                        dropped: false,
                        prepared: true,
                        transfer: None,
                        peer: peer.clone(),
                        generation,
                        expires: now() + OFFER_LIFETIME_MS,
                        source: Some(source),
                        manifest,
                    },
                );
                self.send(peer, FileMessage::Prepared { drag })?;
            }
            FileMessage::Prepared { drag } => {
                let lease = self.drags.get_mut(&drag).context("unknown native drag")?;
                anyhow::ensure!(
                    &lease.peer == peer && lease.generation == generation,
                    "native prepare peer mismatch"
                );
                lease.prepared = true;
            }
            FileMessage::Dropped { drag } => {
                let lease = self.drags.get_mut(&drag).context("unknown native drag")?;
                anyhow::ensure!(
                    &lease.peer == peer && lease.generation == generation,
                    "native drop peer mismatch"
                );
                lease.dropped = true;
                self.send(peer, FileMessage::DropConfirmed { drag })?;
            }
            FileMessage::DropConfirmed { drag } => {
                let lease = self
                    .drags
                    .get(&drag)
                    .context("unknown confirmed native drop")?;
                anyhow::ensure!(
                    &lease.peer == peer && lease.generation == generation && lease.dropped,
                    "native drop confirmation mismatch"
                );
                if let Some((_, reply)) = self.drop_replies.remove(&drag) {
                    self.reply(Some(reply), Ok(FileReply::Done));
                }
            }
            FileMessage::Commit {
                offer,
                generation: version,
                transfer,
                roots,
                drag,
            } => {
                anyhow::ensure!(
                    !self.transfers.contains_key(&transfer),
                    "file commit already consumed"
                );
                self.capacity(peer)?;
                let (source, manifest) = if let Some(drag) = drag {
                    let lease = self
                        .drags
                        .get(&drag)
                        .context("unknown native source lease")?;
                    anyhow::ensure!(
                        &lease.peer == peer
                            && lease.generation == generation
                            && lease.offer == offer
                            && lease.dropped
                            && lease.transfer.is_none()
                            && lease.expires > now()
                            && lease.roots == roots
                            && lease.manifest.generation == version,
                        "native commit scope mismatch"
                    );
                    (
                        lease.source.clone().context("not a source lease")?,
                        lease.manifest.clone(),
                    )
                } else {
                    let registered = self.available(offer)?;
                    anyhow::ensure!(
                        &registered.record.recipient == peer
                            && registered.record.owner == self.id
                            && registered.record.manifest.generation == version,
                        "file commit scope mismatch"
                    );
                    (
                        registered
                            .source
                            .clone()
                            .context("not a source selection")?,
                        registered.record.manifest.select(&roots)?,
                    )
                };
                let token = random()?;
                self.send(peer, FileMessage::Grant { transfer, token })?;
                self.transfers.insert(
                    transfer,
                    Delivery {
                        epoch: self.epoch(peer),
                        record: TransferRecord {
                            id: transfer,
                            offer,
                            peer: peer.clone(),
                            direction: TransferDirection::Send,
                            state: TransferState::Committed,
                            bytes: 0,
                            total_bytes: manifest.total_bytes,
                            paths: vec![],
                            error: None,
                        },
                        manifest,
                        destination: None,
                        generation,
                        cancel: Cancellation::default(),
                        bytes: Arc::new(AtomicU64::new(0)),
                        token: Some(token),
                        source: Some(source),
                        deadline: Instant::now() + transport::DEADLINE,
                        running: false,
                        receiver_ready: false,
                    },
                );
                if let Some(drag) = drag {
                    self.drags.get_mut(&drag).unwrap().transfer = Some(transfer);
                }
            }
            FileMessage::Grant { transfer, token } => {
                self.receive_grant(peer, generation, transfer, token)?
            }
            FileMessage::Cancel { transfer } => {
                if let Some(delivery) = self.transfers.get(&transfer) {
                    anyhow::ensure!(
                        &delivery.record.peer == peer && delivery.generation == generation,
                        "cancel scope mismatch"
                    );
                    self.cancel(transfer, "cancelled by peer");
                }
            }
            FileMessage::CancelDrag { drag } => {
                if let Some(lease) = self.drags.get(&drag) {
                    anyhow::ensure!(
                        &lease.peer == peer && lease.generation == generation,
                        "cancel drag scope mismatch"
                    );
                    if let Some(transfer) = lease.transfer {
                        self.cancel(transfer, "native drag cancelled by peer");
                    }
                    self.drags.remove(&drag);
                }
            }
            FileMessage::Result { transfer, error } => {
                let delivery = self
                    .transfers
                    .get_mut(&transfer)
                    .context("unknown transfer result")?;
                anyhow::ensure!(
                    &delivery.record.peer == peer
                        && delivery.generation == generation
                        && delivery.record.direction == TransferDirection::Send,
                    "transfer result scope mismatch"
                );
                if let Some(error) = error {
                    self.fail(transfer, &error);
                } else if !delivery.record.state.terminal() {
                    if !delivery.running && delivery.record.state == TransferState::Verifying {
                        delivery.record.state = TransferState::Ready;
                    } else if delivery.running {
                        delivery.receiver_ready = true;
                    } else {
                        anyhow::bail!("success result precedes payload delivery");
                    }
                }
            }
            FileMessage::Reject {
                transfer,
                drag,
                reason,
            } => {
                if let Some(transfer) = transfer {
                    let delivery = self
                        .transfers
                        .get(&transfer)
                        .context("unknown rejected transfer")?;
                    anyhow::ensure!(
                        &delivery.record.peer == peer && delivery.generation == generation,
                        "reject scope mismatch"
                    );
                    self.fail(transfer, &reason);
                }
                if let Some(drag) = drag {
                    let lease = self.drags.get(&drag).context("unknown rejected drag")?;
                    anyhow::ensure!(
                        &lease.peer == peer && lease.generation == generation,
                        "reject drag scope mismatch"
                    );
                    self.drags.remove(&drag);
                    if let Some((_, reply)) = self.drop_replies.remove(&drag) {
                        self.reply(Some(reply), Err(anyhow::Error::msg(reason)));
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn receive_grant(
        &mut self,
        peer: &MachineId,
        generation: u64,
        transfer: TransferId,
        token: [u8; 32],
    ) -> Result<()> {
        let delivery = self
            .transfers
            .get_mut(&transfer)
            .context("unsolicited file grant")?;
        anyhow::ensure!(
            &delivery.record.peer == peer
                && delivery.generation == generation
                && delivery.record.direction == TransferDirection::Receive
                && delivery.record.state == TransferState::Committed
                && !delivery.running,
            "file grant does not match a live receive commit"
        );
        anyhow::ensure!(
            delivery.deadline > Instant::now() && token != [0; 32],
            "file grant expired or invalid"
        );
        delivery.running = true;
        delivery.record.state = TransferState::Receiving;
        let record = delivery.record.clone();
        let manifest = delivery.manifest.clone();
        let destination = delivery
            .destination
            .clone()
            .context("missing receive destination")?;
        let bytes = delivery.bytes.clone();
        let cancel = delivery.cancel.clone();
        let guard = self.guard(peer, generation, cancel.clone());
        let base = self.base.clone();
        let ts = self.ts.clone();
        let bind = self.net.bind_ip();
        let ip = self
            .policy
            .borrow()
            .peers
            .get(peer)
            .context("peer disappeared")?
            .1;
        let peer = peer.clone();
        self.jobs.spawn(async move {
            let result = async {
                guard.check()?;
                let staging_base = base.clone();
                let stage_cancel = cancel.clone();
                let mut staging = tokio::task::spawn_blocking(move || {
                    storage::Staging::new(
                        staging_base,
                        record,
                        destination,
                        manifest,
                        &stage_cancel,
                    )
                })
                .await??;
                let streamed = async {
                    let mut socket = guard
                        .protect(transport::connect(bind, ip, &peer, transfer, token, &ts))
                        .await?;
                    transport::receive(&mut socket, &staging, &guard, &bytes).await?;
                    guard.check()
                }
                .await;
                tokio::task::spawn_blocking(move || {
                    let result = streamed.and_then(|_| {
                        guard.check()?;
                        staging.complete(&cancel)?;
                        Ok(())
                    });
                    let mut cleanup_failed = false;
                    if let Err(error) = result {
                        let reason = short_error(&error);
                        if let Err(cleanup) = staging.fail(
                            reason.clone(),
                            cancel.check().is_err(),
                            bytes.load(Ordering::Relaxed),
                        ) {
                            cleanup_failed = true;
                            staging.journal.record.error = Some(short_error(&anyhow::anyhow!("{reason}; receipt cleanup: {cleanup:#}")));
                            if let Err(persist) = storage::save(&base, &staging.journal) {
                                staging.journal.record.error = Some(short_error(&anyhow::anyhow!("{reason}; receipt cleanup: {cleanup:#}; cannot persist receipt diagnostic: {persist:#}")));
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(super::super::offers::ReceiveOutcome { journal: staging.journal, cleanup_failed })
                })
                .await?
            }
            .await;
            Completed::Received(transfer, result)
        });
        Ok(())
    }

    pub(super) fn accept(&mut self, accepted: Accepted) -> Result<()> {
        self.sync_clipboard();
        let generation = self.authorize(&accepted.peer)?;
        let delivery = self
            .transfers
            .get_mut(&accepted.transfer)
            .context("unknown bulk transfer")?;
        delivery.claim_bulk(&accepted.peer, generation, accepted.token)?;
        let source = delivery.source.clone().context("missing source lease")?;
        let manifest = delivery.manifest.clone();
        let bytes = delivery.bytes.clone();
        let cancel = delivery.cancel.clone();
        let guard = self.guard(&accepted.peer, generation, cancel);
        self.jobs.spawn(async move {
            Completed::Sent(
                accepted.transfer,
                transport::send(accepted.socket, source, manifest, guard, bytes).await,
            )
        });
        Ok(())
    }
}
