use splice_core::{files as core, UiConnection, UiState};
use splice_platform::file_shelf as native;

pub fn snapshot(ui: &UiState, files: &core::FileState, pins: &std::collections::HashMap<core::TransferId, native::ReceivedSelection>, scopes: &std::collections::HashMap<core::TransferId, Vec<u32>>, error: Option<String>) -> native::ShelfSnapshot {
    native::ShelfSnapshot {
        self_id: ui.self_id.clone(),
        enabled: ui.master_enabled && files.enabled,
        recipients: ui.machines.iter().filter(|machine| {
            machine.id != ui.self_id && machine.enabled
                && matches!(machine.connection, UiConnection::Direct { .. } | UiConnection::Derp { .. })
        }).map(|machine| native::Recipient {
            id: machine.id.clone(),
            name: machine.hostname.clone(),
        }).collect(),
        names: ui.machines.iter().map(|machine| native::MachineName {
            id: machine.id.clone(),
            name: machine.hostname.clone(),
        }).collect(),
        offers: files.offers.iter().map(|offer| native::ShelfOffer {
            id: native::OfferId(offer.id.0),
            owner: offer.owner.clone(),
            recipient: offer.recipient.clone(),
            entries: offer.manifest.entries.iter().filter(|entry| entry.parent.is_none()).filter_map(|entry| Some(native::ShelfEntry {
                id: entry.id.0,
                parent: None,
                name: entry.name.clone(),
                kind: match entry.kind {
                    core::EntryKind::Directory => native::EntryKind::Directory,
                    core::EntryKind::File { size } => native::EntryKind::File { size },
                    core::EntryKind::Symlink { .. } => return None,
                },
            })).collect(),
            total_bytes: offer.manifest.total_bytes,
            expires_unix_ms: offer.expires_unix_ms,
            state: match offer.state {
                core::OfferState::Available => native::OfferState::Available,
                core::OfferState::Revoked => native::OfferState::Revoked,
                core::OfferState::Expired => native::OfferState::Expired,
            },
        }).collect(),
        transfers: files.transfers.iter().map(|transfer| native::ShelfTransfer {
            id: native::TransferId(transfer.id.0),
            offer: native::OfferId(transfer.offer.0),
            peer: transfer.peer.clone(),
            direction: match transfer.direction {
                core::TransferDirection::Send => native::TransferDirection::Send,
                core::TransferDirection::Receive => native::TransferDirection::Receive,
            },
            state: match transfer.state {
                core::TransferState::Preparing => native::TransferState::Preparing,
                core::TransferState::Committed => native::TransferState::Committed,
                core::TransferState::Receiving => native::TransferState::Receiving,
                core::TransferState::Verifying => native::TransferState::Verifying,
                core::TransferState::Ready => native::TransferState::Ready,
                core::TransferState::Cancelled => native::TransferState::Cancelled,
                core::TransferState::Failed => native::TransferState::Failed,
            },
            roots: scopes.get(&transfer.id).cloned().unwrap_or_else(|| files.offers.iter().find(|offer| offer.id == transfer.offer).map(|offer| {
                offer.manifest.entries.iter().filter(|entry| entry.parent.is_none())
                    .filter(|entry| transfer.paths.iter().any(|path| path.file_name().is_some_and(|name| name == entry.name.as_str())))
                    .map(|entry| entry.id.0).collect()
            }).unwrap_or_default()),
            received: pins.get(&transfer.id).cloned(),
            bytes: transfer.bytes,
            total_bytes: transfer.total_bytes,
            paths: transfer.paths.clone(),
            error: transfer.error.clone(),
        }).collect(),
        error: error.or_else(|| files.error.clone()).or_else(|| {
            files.recovery_errors.first().map(|issue| {
                if files.recovery_error_count > 1 {
                    format!("{} ({} stored transfer issues)", issue.message, files.recovery_error_count)
                } else {
                    issue.message.clone()
                }
            })
        }),
    }
}
