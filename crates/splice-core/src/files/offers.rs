use super::{
    manifest::Selection, Cancellation, FileOfferId, Manifest, OfferRecord, ReceiveDestination,
    TransferId, TransferRecord,
};
use splice_proto::{files::EntryId, MachineId};
use std::{
    sync::{atomic::AtomicU64, Arc},
    time::Instant,
};

pub(super) const MAX_OFFERS: usize = 16;
pub(super) const MAX_PEER_OFFERS: usize = 4;
pub(super) const MAX_ACTIVE: usize = 4;
pub(super) const MAX_PEER_ACTIVE: usize = 2;
pub(super) const MAX_DRAGS: usize = 32;

pub(super) struct RegisteredOffer {
    pub record: OfferRecord,
    pub source: Option<Arc<Selection>>,
    pub generation: u64,
    pub epoch: u64,
}

pub(super) struct Delivery {
    pub record: TransferRecord,
    pub manifest: Manifest,
    pub destination: Option<ReceiveDestination>,
    pub generation: u64,
    pub epoch: u64,
    pub cancel: Cancellation,
    pub bytes: Arc<AtomicU64>,
    pub token: Option<[u8; 32]>,
    pub source: Option<Arc<Selection>>,
    pub deadline: Instant,
    pub running: bool,
    pub receiver_ready: bool,
}

impl Delivery {
    pub fn claim_bulk(
        &mut self,
        peer: &MachineId,
        generation: u64,
        token: [u8; 32],
    ) -> anyhow::Result<()> {
        self.cancel.check()?;
        anyhow::ensure!(
            &self.record.peer == peer
                && self.generation == generation
                && self.record.direction == super::TransferDirection::Send
                && self.record.state == super::TransferState::Committed
                && !self.running
                && self.deadline > Instant::now(),
            "bulk grant is not live"
        );
        let expected = self
            .token
            .ok_or_else(|| anyhow::anyhow!("bulk grant already consumed"))?;
        let difference = expected
            .iter()
            .zip(token)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b));
        anyhow::ensure!(difference == 0, "bulk token mismatch");
        self.token = None;
        self.running = true;
        self.record.state = super::TransferState::Receiving;
        Ok(())
    }
}

pub(super) struct Drag {
    pub offer: FileOfferId,
    pub roots: Vec<EntryId>,
    pub dropped: bool,
    pub prepared: bool,
    pub transfer: Option<TransferId>,
    pub peer: MachineId,
    pub generation: u64,
    pub epoch: u64,
    pub expires: u64,
    pub source: Option<Arc<Selection>>,
    pub manifest: Manifest,
}

pub(super) struct Enumeration {
    pub recipient: MachineId,
    pub generation: u64,
    pub epoch: u64,
    pub origin: super::OfferOrigin,
    pub cancellation: Cancellation,
    pub reply: Option<tokio::sync::oneshot::Sender<Result<super::FileReply, String>>>,
}

pub(super) enum Completed {
    Cleared(
        TransferId,
        Option<tokio::sync::oneshot::Sender<Result<super::FileReply, String>>>,
        Result<(), super::storage::ClearFailure>,
    ),
    Enabled(
        bool,
        Option<tokio::sync::oneshot::Sender<Result<super::FileReply, String>>>,
        anyhow::Result<()>,
    ),
    Enumerated(u64, anyhow::Result<Selection>),
    Sent(TransferId, anyhow::Result<()>),
    Received(TransferId, anyhow::Result<ReceiveOutcome>),
}

pub(super) struct ReceiveOutcome {
    pub journal: super::storage::Journal,
    pub cleanup_failed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{TransferDirection, TransferState};
    use std::time::Duration;

    fn grant() -> Delivery {
        Delivery {
            record: TransferRecord {
                id: TransferId([1; 16]),
                offer: FileOfferId([2; 16]),
                peer: MachineId("recipient".into()),
                direction: TransferDirection::Send,
                state: TransferState::Committed,
                bytes: 0,
                total_bytes: 0,
                paths: vec![],
                error: None,
            },
            manifest: Manifest {
                generation: [3; 16],
                entries: vec![],
                total_bytes: 0,
            },
            destination: None,
            generation: 7,
            epoch: 0,
            cancel: Cancellation::default(),
            bytes: Arc::new(AtomicU64::new(0)),
            token: Some([4; 32]),
            source: None,
            deadline: Instant::now() + Duration::from_secs(30),
            running: false,
            receiver_ready: false,
        }
    }

    #[test]
    fn grants_bind_peer_generation_and_token_and_are_consumed_once() {
        let mut grant = grant();
        assert!(grant
            .claim_bulk(&MachineId("another-peer".into()), 7, [4; 32])
            .is_err());
        assert!(grant
            .claim_bulk(&MachineId("recipient".into()), 8, [4; 32])
            .is_err());
        assert!(grant
            .claim_bulk(&MachineId("recipient".into()), 7, [5; 32])
            .is_err());
        assert_eq!(grant.token, Some([4; 32]));
        grant
            .claim_bulk(&MachineId("recipient".into()), 7, [4; 32])
            .unwrap();
        assert!(grant.token.is_none());
        assert!(grant
            .claim_bulk(&MachineId("recipient".into()), 7, [4; 32])
            .is_err());
    }

    #[test]
    fn cancellation_expiry_and_uncommitted_deliveries_never_authorize_bulk() {
        let mut cancelled = grant();
        cancelled.cancel.cancel();
        assert!(cancelled
            .claim_bulk(&MachineId("recipient".into()), 7, [4; 32])
            .is_err());
        let mut expired = grant();
        expired.deadline = Instant::now();
        assert!(expired
            .claim_bulk(&MachineId("recipient".into()), 7, [4; 32])
            .is_err());
        let mut uncommitted = grant();
        uncommitted.record.state = TransferState::Preparing;
        assert!(uncommitted
            .claim_bulk(&MachineId("recipient".into()), 7, [4; 32])
            .is_err());
    }
}
