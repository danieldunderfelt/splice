use super::{Focus, Inner};
use crate::files::{FileClipboardRef, FileCommand, LocalSelection};
use splice_proto::{caps, Frame, MachineId, Stamp};
use std::path::PathBuf;

impl Inner {
    pub(super) fn native_clipboard_event(&mut self, event: crate::engine::NativeClipboardEvent) {
        if event.epoch() != self.files.handle.clipboard_epoch() {
            self.clipboard_clock
                .invalidate_pending(Some(event.generation()));
            self.retire_stale_file_clipboard();
            return;
        }
        if !self.clipboard_clock.accept(&event) {
            return;
        }
        match event {
            crate::engine::NativeClipboardEvent::Invalidated { .. } => {
                self.retire_local_file_clipboard()
            }
            crate::engine::NativeClipboardEvent::Changed {
                mimes, inline_text, ..
            } => self.on_clipboard_changed(mimes, inline_text),
            crate::engine::NativeClipboardEvent::Captured {
                generation,
                selection,
                epoch,
            } => self.local_native_file_clipboard(selection, generation, epoch),
        }
    }

    pub(super) fn native_clipboard_ordered(&self) -> bool {
        self.clipboard_clock.ordered() || self.native_selection_tx.latest_generation().is_some()
    }

    pub(super) fn invalidate_pending_native_clipboard(&mut self) {
        self.clipboard_clock
            .invalidate_pending(self.native_selection_tx.replace_pending());
        self.retire_local_file_clipboard();
    }

    pub(super) fn retire_stale_file_clipboard(&mut self) {
        if self.local_clipboard_epoch != self.files.handle.clipboard_epoch()
            && self
                .file_clipboard
                .as_ref()
                .is_some_and(|reference| reference.stamp.writer == self.self_info.id)
        {
            self.retire_local_file_clipboard();
        }
    }

    fn retire_local_file_clipboard(&mut self) {
        if self
            .file_clipboard
            .as_ref()
            .is_some_and(|reference| reference.stamp.writer == self.self_info.id)
        {
            self.file_clipboard = None;
            self.file_route = None;
        }
        if let Err(error) = self.files.handle.clear_clipboard() {
            self.config_error = Some(error.to_string());
        }
        self.touch_ui();
    }

    pub(super) fn send_file_clipboard_ref(&self, peer: &MachineId) {
        if !self.cfg.clipboard_sync
            || !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.files.handle.summary().borrow().enabled
            || !self.machine_enabled(peer)
            || !self
                .peers
                .get(peer)
                .is_some_and(|p| p.connected && p.caps.iter().any(|c| c == caps::FILES_V2))
        {
            return;
        }
        if let (Some(reference), Some(net)) = (&self.file_clipboard, &self.net) {
            if reference.stamp.writer == self.self_info.id {
                net.send_to(
                    peer,
                    Frame::FileClipboardRef {
                        stamp: reference.stamp.clone(),
                        generation: reference.generation,
                    },
                );
            }
        }
    }

    pub(super) fn clear_file_clipboard(&mut self) {
        self.file_clipboard = None;
        self.file_route = None;
        if let Err(error) = self.files.handle.clear_clipboard() {
            self.config_error = Some(error.to_string());
        }
        self.touch_ui();
    }

    pub(super) fn local_file_clipboard(&mut self, paths: Vec<PathBuf>, generation: u64) {
        if self.native_clipboard_ordered() {
            self.config_error =
                Some("use the ordered native clipboard bridge for this observer".into());
            self.touch_ui();
            return;
        }
        self.local_native_file_clipboard(
            LocalSelection::paths(paths),
            generation,
            self.files.handle.clipboard_epoch(),
        );
    }

    pub(super) fn local_native_file_clipboard(
        &mut self,
        selection: LocalSelection,
        generation: u64,
        epoch: u64,
    ) {
        if !self.cfg.clipboard_sync
            || !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.files.handle.summary().borrow().enabled
        {
            return;
        }
        if selection.validate().is_err() {
            self.config_error = Some("invalid local clipboard file selection".into());
            self.touch_ui();
            return;
        }
        if let Err(error) = self
            .files
            .handle
            .capture_clipboard(selection, generation, epoch)
        {
            self.config_error = Some(error.to_string());
            self.touch_ui();
            return;
        }
        self.local_clipboard_epoch = epoch;
        self.clipboard_offers.clear();
        self.pending_fetches.clear();
        self.live_offer = None;
        self.clip_lamport += 1;
        let stamp = Stamp {
            lamport: self.clip_lamport,
            writer: self.self_info.id.clone(),
        };
        self.clip_seen = Some(stamp.clone());
        self.file_clipboard = Some(FileClipboardRef {
            stamp: stamp.clone(),
            generation,
        });
        self.file_route = None;
        if let Some(net) = &self.net {
            for (peer, state) in &self.peers {
                if state.connected
                    && self.machine_enabled(peer)
                    && state.caps.iter().any(|c| c == caps::FILES_V2)
                {
                    net.send_to(
                        peer,
                        Frame::FileClipboardRef {
                            stamp: stamp.clone(),
                            generation,
                        },
                    );
                }
            }
        }
        self.route_file_clipboard();
        self.touch_ui();
    }

    pub(super) fn remote_file_clipboard(
        &mut self,
        from: &MachineId,
        stamp: Stamp,
        generation: u64,
    ) {
        if !self.cfg.clipboard_sync
            || !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.files.handle.summary().borrow().enabled
            || !self.machine_enabled(from)
            || !self.peer_usable(from)
            || stamp.writer != *from
            || !self
                .peers
                .get(from)
                .is_some_and(|p| p.connected && p.caps.iter().any(|c| c == caps::FILES_V2))
            || self.clip_seen.as_ref().is_some_and(|seen| stamp <= *seen)
        {
            return;
        }
        self.clip_lamport = self.clip_lamport.max(stamp.lamport);
        self.invalidate_pending_native_clipboard();
        self.clear_file_clipboard();
        self.clipboard_offers.clear();
        self.pending_fetches.clear();
        self.live_offer = None;
        self.clip_seen = Some(stamp.clone());
        self.file_clipboard = Some(FileClipboardRef { stamp, generation });
        self.route_file_clipboard();
        self.touch_ui();
    }

    pub(super) fn remote_file_route(
        &mut self,
        from: &MachineId,
        stamp: Stamp,
        generation: u64,
        recipient: MachineId,
    ) {
        if !self.cfg.clipboard_sync
            || !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.files.handle.summary().borrow().enabled
            || self
                .claim
                .as_ref()
                .is_none_or(|claim| claim.writer != *from)
            || self.file_clipboard.as_ref().is_none_or(|reference| {
                reference.stamp != stamp
                    || reference.generation != generation
                    || stamp.writer != self.self_info.id
            })
            || !self.machine_enabled(from)
            || !self.peer_usable(from)
            || !self.machine_enabled(&recipient)
            || !self.peer_usable(&recipient)
        {
            return;
        }
        if let Err(error) = self.files.handle.send(FileCommand::RouteClipboard {
            recipient,
            generation,
        }) {
            self.config_error = Some(error.to_string());
            self.touch_ui();
        }
    }

    pub(super) fn route_file_clipboard(&mut self) {
        if !self.cfg.clipboard_sync
            || !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.files.handle.summary().borrow().enabled
            || self
                .claim
                .as_ref()
                .is_none_or(|claim| claim.writer != self.self_info.id)
        {
            return;
        }
        let recipient = match &self.focus {
            Focus::Remote(recipient) => recipient,
            Focus::Local => &self.self_info.id,
            Focus::Driven(_) => {
                self.file_route = None;
                return;
            }
        };
        let Some(reference) = self.file_clipboard.clone() else {
            return;
        };
        if reference.stamp.writer == *recipient
            || self.file_route.as_ref() == Some(&(reference.stamp.clone(), recipient.clone()))
        {
            return;
        }
        let sent = if reference.stamp.writer == self.self_info.id {
            self.files
                .handle
                .send(FileCommand::RouteClipboard {
                    recipient: recipient.clone(),
                    generation: reference.generation,
                })
                .is_ok()
        } else {
            self.net.as_ref().is_some_and(|net| {
                net.send_to(
                    &reference.stamp.writer,
                    Frame::FileRoute {
                        stamp: reference.stamp.clone(),
                        generation: reference.generation,
                        recipient: recipient.clone(),
                    },
                )
            })
        };
        if sent {
            self.file_route = Some((reference.stamp, recipient.clone()));
        }
    }
}
