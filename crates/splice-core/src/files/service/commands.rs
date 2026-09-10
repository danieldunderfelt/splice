use super::*;

impl Service {
    pub(super) fn sync_clipboard(&mut self) {
        if let Some(update) = self.clipboard.take() {
            self.apply_clipboard(update);
        }
    }

    pub(super) fn apply_clipboard(&mut self, mut update: super::super::clipboard::Update) {
        if let Some(latest) = self.clipboard.take() {
            update = latest;
        }
        self.clear_clipboard_selection();
        if let super::super::clipboard::Update::Capture {
            selection,
            generation,
            epoch,
        } = update
        {
            let policy = self.policy.borrow();
            if self.enabled
                && policy.enabled
                && policy.clipboard_enabled
                && self.clipboard.current(epoch)
            {
                self.clipboard_generation = Some(generation);
                self.clipboard_selection = Some(selection);
            }
        }
    }

    fn clear_clipboard_selection(&mut self) {
        self.clipboard_selection = None;
        self.clipboard_generation = None;
        for enumeration in self.enumerations.values() {
            if matches!(enumeration.origin, OfferOrigin::Clipboard { .. }) {
                enumeration.cancellation.cancel();
            }
        }
        let offers: Vec<_> = self
            .offers
            .values()
            .filter(|o| {
                o.record.owner == self.id
                    && matches!(o.record.origin, OfferOrigin::Clipboard { .. })
            })
            .map(|o| (o.record.id, o.record.recipient.clone()))
            .collect();
        for (offer, peer) in offers {
            let _ = self.send(
                &peer,
                FileMessage::Revoke {
                    offer,
                    unaccepted_only: true,
                },
            );
            self.revoke(offer, true);
        }
    }

    pub(super) fn command(&mut self, request: Request) {
        self.sync_clipboard();
        let (mut command, mut selection, reply) = match request {
            Request::Command {
                command,
                selection,
                reply,
            } => (command, selection, reply),
            Request::Retain { transfer, reply } => {
                let result = self.retain_received(transfer).map_err(|e| short_error(&e));
                let _ = reply.send(result);
                return;
            }
        };
        self.retire_drags();
        if let FileCommand::ClearReceived { transfer } = command {
            self.clear_received(transfer, reply);
            return;
        }
        if let FileCommand::CaptureClipboard { paths, generation } = command {
            let local = selection
                .take()
                .unwrap_or_else(|| LocalSelection::paths(paths));
            let result = local.validate().and_then(|_| {
                self.action(FileCommand::ClipboardChanged { generation })?;
                self.clipboard_selection = Some(local);
                Ok(FileReply::Done)
            });
            self.reply(reply, result);
            return;
        }
        if let FileCommand::SetEnabled(enabled) = command {
            if self.setting_pending {
                self.reply(
                    reply,
                    Err(anyhow::anyhow!("file policy write already pending")),
                );
                return;
            }
            self.setting_pending = true;
            if !enabled {
                self.clipboard.revoke();
                let offers: Vec<_> = self
                    .offers
                    .values()
                    .filter(|o| o.record.state == OfferState::Available)
                    .map(|o| {
                        (
                            if o.record.owner == self.id {
                                o.record.recipient.clone()
                            } else {
                                o.record.owner.clone()
                            },
                            o.record.id,
                        )
                    })
                    .collect();
                for (peer, offer) in offers {
                    let _ = self.send(
                        &peer,
                        FileMessage::Revoke {
                            offer,
                            unaccepted_only: false,
                        },
                    );
                }
                self.enabled = false;
                self.enforce();
            }
            let base = self.base.clone();
            self.jobs.spawn(async move {
                let result =
                    tokio::task::spawn_blocking(move || storage::save_enabled(&base, enabled))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|r| r);
                Completed::Enabled(enabled, reply, result)
            });
            return;
        }
        if let FileCommand::RouteClipboard {
            recipient,
            generation,
        } = &command
        {
            if self.clipboard_generation != Some(*generation) || self.clipboard_selection.is_none()
            {
                self.reply(reply, Err(anyhow::anyhow!("clipboard selection replaced")));
                return;
            }
            if let Some(offer) = self.offers.values().find(|o| {
                o.record.owner == self.id
                    && &o.record.recipient == recipient
                    && o.record.origin
                        == (OfferOrigin::Clipboard {
                            generation: *generation,
                        })
                    && self.available(o.record.id).is_ok()
            }) {
                self.reply(reply, Ok(FileReply::Offered(offer.record.id)));
                return;
            }
            selection = self.clipboard_selection.clone();
            command = FileCommand::Offer {
                paths: vec![],
                recipient: recipient.clone(),
                origin: OfferOrigin::Clipboard {
                    generation: *generation,
                },
            };
        }
        if let FileCommand::Offer {
            paths,
            recipient,
            origin,
        } = command
        {
            let selection = selection.unwrap_or_else(|| LocalSelection::paths(paths));
            let result = (|| {
                selection.validate()?;
                let generation = self.authorize(&recipient)?;
                self.offer_capacity(&recipient)?;
                anyhow::ensure!(
                    self.enumerations.len() < 2,
                    "file enumeration limit reached"
                );
                if let OfferOrigin::Clipboard { generation } = origin {
                    anyhow::ensure!(
                        self.policy.borrow().clipboard_enabled,
                        "clipboard sharing disabled"
                    );
                    anyhow::ensure!(
                        self.clipboard_generation.is_none_or(|g| g == generation),
                        "clipboard selection is stale"
                    );
                    self.clipboard_generation = Some(generation);
                }
                Ok(generation)
            })();
            match result {
                Ok(generation) => {
                    self.enumeration_id += 1;
                    let id = self.enumeration_id;
                    let cancellation = Cancellation::default();
                    let cancel = cancellation.clone();
                    self.enumerations.insert(
                        id,
                        Enumeration {
                            epoch: self.epoch(&recipient),
                            recipient,
                            generation,
                            origin,
                            cancellation,
                            reply,
                        },
                    );
                    self.jobs.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            manifest::Selection::build_local(selection, &cancel)
                        })
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|r| r);
                        Completed::Enumerated(id, result)
                    });
                }
                Err(error) => self.reply(reply, Err(error)),
            }
            return;
        }
        let dropped = if let FileCommand::DropDrag { drag } = &command {
            Some(*drag)
        } else {
            None
        };
        if dropped.is_some_and(|drag| self.drop_replies.contains_key(&drag)) {
            self.reply(
                reply,
                Err(anyhow::anyhow!("native drop confirmation already pending")),
            );
            return;
        }
        let result = self.action(command);
        self.publish();
        match (dropped, reply, result) {
            (Some(drag), Some(reply), Ok(_)) => {
                self.drop_replies
                    .insert(drag, (Instant::now() + transport::AUTH_DEADLINE, reply));
            }
            (_, reply, result) => self.reply(reply, result),
        }
    }

    pub(super) fn reply(
        &mut self,
        reply: Option<oneshot::Sender<Result<FileReply, String>>>,
        result: Result<FileReply>,
    ) {
        let result = result.map_err(|e| short_error(&e));
        self.error = result.as_ref().err().cloned();
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }

    pub(super) fn action(&mut self, command: FileCommand) -> Result<FileReply> {
        match command {
            FileCommand::CancelDrags | FileCommand::CancelPeerDrags { .. } => {
                let peer = if let FileCommand::CancelPeerDrags { peer } = command {
                    Some(peer)
                } else {
                    None
                };
                let drags: Vec<_> = self
                    .drags
                    .iter()
                    .filter(|(_, d)| {
                        d.transfer.is_none() && peer.as_ref().is_none_or(|p| p == &d.peer)
                    })
                    .map(|(id, _)| *id)
                    .collect();
                for drag in drags {
                    self.action(FileCommand::CancelDrag { drag })?;
                }
                Ok(FileReply::Done)
            }
            FileCommand::CaptureClipboard { .. } | FileCommand::ClearReceived { .. } => {
                anyhow::bail!("operation requires command handling")
            }
            FileCommand::RouteClipboard { .. } => {
                anyhow::bail!("clipboard routing must run through enumeration")
            }
            FileCommand::ClearClipboard => {
                self.clear_clipboard_selection();
                Ok(FileReply::Done)
            }
            FileCommand::Offer { .. } => anyhow::bail!("offer must run through enumeration"),
            FileCommand::Receive { offer, destination } => {
                self.begin_receive(offer, destination, None, vec![])
            }
            FileCommand::Retry { transfer } => {
                let delivery = self.transfers.get(&transfer).context("unknown transfer")?;
                anyhow::ensure!(
                    matches!(
                        delivery.record.state,
                        TransferState::Failed | TransferState::Cancelled
                    ) && !delivery.running,
                    "only stopped receives can retry"
                );
                let destination = delivery
                    .destination
                    .clone()
                    .context("retry must be requested at receiver")?;
                let offer = delivery.record.offer;
                let roots = delivery
                    .manifest
                    .entries
                    .iter()
                    .filter(|e| e.parent.is_none())
                    .map(|e| e.id)
                    .collect();
                self.begin_receive(offer, destination, None, roots)
            }
            FileCommand::Revoke { offer } => {
                let registered = self.offers.get(&offer).context("unknown file offer")?;
                let peer = if registered.record.owner == self.id {
                    registered.record.recipient.clone()
                } else {
                    registered.record.owner.clone()
                };
                let _ = self.send(
                    &peer,
                    FileMessage::Revoke {
                        offer,
                        unaccepted_only: false,
                    },
                );
                self.revoke(offer, false);
                Ok(FileReply::Done)
            }
            FileCommand::Cancel { transfer } => {
                let delivery = self.transfers.get(&transfer).context("unknown transfer")?;
                let peer = delivery.record.peer.clone();
                self.cancel(transfer, "cancelled by user");
                let _ = self.send(&peer, FileMessage::Cancel { transfer });
                Ok(FileReply::Done)
            }
            FileCommand::SetEnabled(_) => {
                anyhow::bail!("file policy must be persisted through command handling")
            }
            FileCommand::ClipboardChanged { generation } => {
                if self.clipboard_generation != Some(generation) {
                    self.clipboard_selection = None;
                }
                self.clipboard_generation = Some(generation);
                for enumeration in self.enumerations.values() {
                    if matches!(enumeration.origin, OfferOrigin::Clipboard { generation: g } if g != generation)
                    {
                        enumeration.cancellation.cancel();
                    }
                }
                let offers: Vec<_> = self.offers.values().filter(|o| o.record.owner == self.id && matches!(o.record.origin, OfferOrigin::Clipboard { generation: g } if g != generation)).map(|o| (o.record.id, o.record.recipient.clone())).collect();
                for (offer, peer) in offers {
                    let _ = self.send(
                        &peer,
                        FileMessage::Revoke {
                            offer,
                            unaccepted_only: true,
                        },
                    );
                    self.revoke(offer, true);
                }
                Ok(FileReply::Done)
            }
            FileCommand::PrepareDrag { offer, entries } => {
                let registered = self.available(offer)?;
                anyhow::ensure!(
                    registered.record.recipient == self.id,
                    "only a received offer can be dragged"
                );
                let manifest = registered.record.manifest.select(&entries)?;
                let peer = registered.record.owner.clone();
                let generation = registered.generation;
                self.drag_capacity(&peer)?;
                let drag = NativeDragId(random()?);
                self.send(
                    &peer,
                    FileMessage::Prepare {
                        offer,
                        generation: manifest.generation,
                        drag,
                        roots: entries.clone(),
                    },
                )?;
                self.drags.insert(
                    drag,
                    Drag {
                        epoch: self.epoch(&peer),
                        offer,
                        roots: entries,
                        dropped: false,
                        prepared: false,
                        transfer: None,
                        peer,
                        generation,
                        expires: now() + OFFER_LIFETIME_MS,
                        source: None,
                        manifest,
                    },
                );
                Ok(FileReply::DragPrepared(drag))
            }
            FileCommand::DropDrag { drag } => {
                let lease = self
                    .drags
                    .get(&drag)
                    .context("unknown or cancelled native drag")?;
                anyhow::ensure!(
                    lease.expires > now() && self.authorize(&lease.peer)? == lease.generation,
                    "native drag authorization expired"
                );
                self.send(&lease.peer, FileMessage::Dropped { drag })?;
                self.drags.get_mut(&drag).unwrap().dropped = true;
                Ok(FileReply::Done)
            }
            FileCommand::CommitDrag { drag, destination } => {
                let lease = self
                    .drags
                    .get(&drag)
                    .context("unknown or cancelled native drag")?;
                anyhow::ensure!(
                    lease.dropped && lease.expires > now(),
                    "native content read occurred before accepted drop or after expiry"
                );
                if let Some(transfer) = lease.transfer {
                    anyhow::ensure!(
                        !self.clearing.contains(&transfer),
                        "receive is being cleared"
                    );
                    return Ok(FileReply::Receiving(transfer));
                }
                self.begin_receive(lease.offer, destination, Some(drag), vec![])
            }
            FileCommand::CancelDrag { drag } => {
                if let Some(lease) = self.drags.remove(&drag) {
                    if let Some(transfer) = lease.transfer {
                        self.cancel(transfer, "native drag cancelled");
                    }
                    let _ = self.send(&lease.peer, FileMessage::CancelDrag { drag });
                }
                Ok(FileReply::Done)
            }
        }
    }

    pub(super) fn begin_receive(
        &mut self,
        offer: FileOfferId,
        destination: ReceiveDestination,
        drag: Option<NativeDragId>,
        roots: Vec<EntryId>,
    ) -> Result<FileReply> {
        let (peer, generation, manifest, selected) = if let Some(drag) = drag {
            let lease = self.drags.get(&drag).context("unknown native drag")?;
            anyhow::ensure!(
                lease.dropped && lease.expires > now(),
                "native drag is not committed"
            );
            (
                lease.peer.clone(),
                lease.generation,
                lease.manifest.clone(),
                lease.roots.clone(),
            )
        } else {
            let registered = self.available(offer)?;
            anyhow::ensure!(
                registered.record.recipient == self.id,
                "cannot receive own offer"
            );
            (
                registered.record.owner.clone(),
                registered.generation,
                registered.record.manifest.select(&roots)?,
                roots,
            )
        };
        anyhow::ensure!(
            self.authorize(&peer)? == generation,
            "receive authorization changed"
        );
        self.capacity(&peer)?;
        if destination == ReceiveDestination::Cache {
            let retained: u64 = self
                .transfers
                .values()
                .filter(|d| d.destination == Some(ReceiveDestination::Cache))
                .map(|d| {
                    if matches!(
                        d.record.state,
                        TransferState::Failed | TransferState::Cancelled
                    ) && d.record.paths.is_empty()
                        && !d.running
                        && !self.cleanup_failures.contains(&d.record.id)
                    {
                        0
                    } else {
                        d.record.total_bytes
                    }
                })
                .fold(self.untracked_cache_bytes, u64::saturating_add);
            anyhow::ensure!(
                retained
                    .checked_add(manifest.total_bytes)
                    .is_some_and(|total| total <= storage::CACHE_QUOTA),
                "retained file cache quota exceeded"
            );
        }
        let id = TransferId(random()?);
        self.send(
            &peer,
            FileMessage::Commit {
                offer,
                generation: manifest.generation,
                transfer: id,
                roots: selected,
                drag,
            },
        )?;
        self.transfers.insert(
            id,
            Delivery {
                epoch: self.epoch(&peer),
                record: TransferRecord {
                    id,
                    offer,
                    peer,
                    direction: TransferDirection::Receive,
                    state: TransferState::Committed,
                    bytes: 0,
                    total_bytes: manifest.total_bytes,
                    paths: vec![],
                    error: None,
                },
                manifest,
                destination: Some(destination),
                generation,
                cancel: Cancellation::default(),
                bytes: Arc::new(AtomicU64::new(0)),
                token: None,
                source: None,
                deadline: Instant::now() + transport::DEADLINE,
                running: false,
                receiver_ready: false,
            },
        );
        if let Some(drag) = drag {
            self.drags.get_mut(&drag).unwrap().transfer = Some(id);
        }
        Ok(FileReply::Receiving(id))
    }
}
