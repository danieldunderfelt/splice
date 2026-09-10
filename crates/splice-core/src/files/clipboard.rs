use super::LocalSelection;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::watch;

pub(super) enum Update {
    Clear,
    Capture {
        selection: LocalSelection,
        generation: u64,
        epoch: u64,
    },
}

#[derive(Default)]
struct Pending {
    epoch: u64,
    closed: bool,
    update: Option<Update>,
}

#[derive(Clone)]
pub(super) struct Sender {
    pending: Arc<Mutex<Pending>>,
    wake: watch::Sender<()>,
}

pub(super) struct Receiver {
    pending: Arc<Mutex<Pending>>,
    wake: watch::Receiver<()>,
}

pub(super) fn channel() -> (Sender, Receiver) {
    let pending = Arc::new(Mutex::new(Pending::default()));
    let (wake, receiver) = watch::channel(());
    (
        Sender {
            pending: pending.clone(),
            wake,
        },
        Receiver {
            pending,
            wake: receiver,
        },
    )
}

impl Sender {
    pub fn epoch(&self) -> u64 {
        self.pending.lock().epoch
    }

    pub fn send(&self, update: Update) -> anyhow::Result<()> {
        anyhow::ensure!(!self.wake.is_closed(), "file clipboard service stopped");
        let old = {
            let mut pending = self.pending.lock();
            anyhow::ensure!(!pending.closed, "file clipboard service stopped");
            pending.update.replace(update)
        };
        drop(old);
        self.wake.send_replace(());
        Ok(())
    }
}

impl Receiver {
    pub fn current(&self, epoch: u64) -> bool {
        self.pending.lock().epoch == epoch
    }

    pub fn revoke(&mut self) {
        let old = {
            let mut pending = self.pending.lock();
            pending.epoch = pending
                .epoch
                .checked_add(1)
                .expect("clipboard epoch exhausted");
            pending.update.take()
        };
        drop(old);
    }

    pub fn take(&mut self) -> Option<Update> {
        self.pending.lock().update.take()
    }

    pub async fn recv(&mut self) -> Option<Update> {
        loop {
            if let Some(update) = self.take() {
                return Some(update);
            }
            self.wake.changed().await.ok()?;
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let old = {
            let mut pending = self.pending.lock();
            pending.closed = true;
            pending.update.take()
        };
        drop(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_rejects_queued_and_taken_capture_epochs() {
        let (sender, mut receiver) = channel();
        let epoch = sender.epoch();
        sender
            .send(Update::Capture {
                selection: LocalSelection::paths(vec![]),
                generation: 1,
                epoch,
            })
            .unwrap();
        let taken = receiver.take().unwrap();
        receiver.revoke();
        let Update::Capture {
            epoch: taken_epoch, ..
        } = taken
        else {
            panic!()
        };
        assert!(!receiver.current(taken_epoch));
        sender
            .send(Update::Capture {
                selection: LocalSelection::paths(vec![]),
                generation: 2,
                epoch,
            })
            .unwrap();
        let Update::Capture {
            epoch: queued_epoch,
            ..
        } = receiver.take().unwrap()
        else {
            panic!()
        };
        assert!(!receiver.current(queued_epoch));
        assert!(receiver.current(sender.epoch()));
    }

    #[tokio::test]
    async fn full_command_queue_cannot_lose_clear_or_retain_superseded_capture() {
        let mut bridge = super::super::Bridge::new();
        for _ in 0..32 {
            bridge
                .handle
                .send(super::super::FileCommand::CancelDrags)
                .unwrap();
        }
        assert!(bridge
            .handle
            .send(super::super::FileCommand::CancelDrags)
            .is_err());
        let file = tempfile::tempfile().unwrap();
        let access = Arc::new(());
        let weak = Arc::downgrade(&access);
        let selection = LocalSelection {
            roots: vec![super::super::SelectedRoot::Open {
                name: "selected".into(),
                file: Arc::new(file),
            }],
            access: Some(access),
        };
        bridge.handle.capture_clipboard(selection, 1, 0).unwrap();
        bridge.handle.clear_clipboard().unwrap();
        assert!(weak.upgrade().is_none());
        assert!(matches!(
            bridge.clipboard.as_mut().unwrap().recv().await,
            Some(Update::Clear)
        ));
        for generation in 2..10_000 {
            bridge
                .handle
                .capture_clipboard(
                    LocalSelection::paths(vec!["/selected".into()]),
                    generation,
                    0,
                )
                .unwrap();
        }
        assert!(matches!(
            bridge.clipboard.as_mut().unwrap().recv().await,
            Some(Update::Capture {
                generation: 9999,
                ..
            })
        ));
    }
}
