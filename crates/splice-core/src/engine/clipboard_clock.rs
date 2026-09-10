use super::NativeClipboardEvent;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Phase {
    Invalidated,
    Changed,
    Captured,
    Replaced,
}

impl NativeClipboardEvent {
    pub fn epoch(&self) -> u64 {
        match self {
            Self::Invalidated { epoch, .. }
            | Self::Changed { epoch, .. }
            | Self::Captured { epoch, .. } => *epoch,
        }
    }

    fn bind_epoch(&mut self, value: u64) {
        match self {
            Self::Invalidated { epoch, .. }
            | Self::Changed { epoch, .. }
            | Self::Captured { epoch, .. } => *epoch = value,
        }
    }

    pub fn generation(&self) -> u64 {
        self.stamp().0
    }

    fn stamp(&self) -> (u64, Phase) {
        match self {
            Self::Invalidated { generation, .. } => (*generation, Phase::Invalidated),
            Self::Changed { generation, .. } => (*generation, Phase::Changed),
            Self::Captured { generation, .. } => (*generation, Phase::Captured),
        }
    }
}

#[derive(Default)]
struct Pending {
    epoch: u64,
    closed: bool,
    latest: Option<(u64, Phase)>,
    event: Option<NativeClipboardEvent>,
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
    pub fn send(&self, mut event: NativeClipboardEvent) -> anyhow::Result<()> {
        anyhow::ensure!(!self.wake.is_closed(), "native clipboard engine stopped");
        let old = {
            let mut pending = self.pending.lock();
            anyhow::ensure!(!pending.closed, "native clipboard engine stopped");
            let stamp = event.stamp();
            if pending.latest.is_some_and(|latest| latest >= stamp) {
                return Ok(());
            }
            if pending.latest.is_none_or(|latest| latest.0 < stamp.0) {
                pending.epoch = event.epoch();
            }
            event.bind_epoch(pending.epoch);
            pending.latest = Some(stamp);
            pending.event.replace(event)
        };
        drop(old);
        self.wake.send_replace(());
        Ok(())
    }

    pub fn latest_generation(&self) -> Option<u64> {
        self.pending.lock().latest.map(|stamp| stamp.0)
    }

    pub fn replace_pending(&self) -> Option<u64> {
        let (generation, old) = {
            let mut pending = self.pending.lock();
            let generation = pending.latest.map(|stamp| stamp.0);
            if let Some(generation) = generation {
                pending.latest = Some((generation, Phase::Replaced));
            }
            (generation, pending.event.take())
        };
        drop(old);
        generation
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<NativeClipboardEvent> {
        loop {
            if let Some(event) = self.pending.lock().event.take() {
                return Some(event);
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
            pending.event.take()
        };
        drop(old);
    }
}

#[derive(Default)]
pub(super) struct ClipboardClock {
    stamp: Option<(u64, Phase)>,
}

impl ClipboardClock {
    pub fn ordered(&self) -> bool {
        self.stamp.is_some()
    }

    pub fn accept(&mut self, event: &NativeClipboardEvent) -> bool {
        let stamp = event.stamp();
        if self.stamp.is_some_and(|old| old >= stamp) {
            return false;
        }
        self.stamp = Some(stamp);
        true
    }

    pub fn invalidate_pending(&mut self, submitted: Option<u64>) {
        if let Some(generation) = self.stamp.map(|stamp| stamp.0).max(submitted) {
            self.stamp = Some((generation, Phase::Replaced));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::LocalSelection;

    fn event(generation: u64, phase: Phase) -> NativeClipboardEvent {
        match phase {
            Phase::Invalidated => NativeClipboardEvent::Invalidated {
                generation,
                epoch: 0,
            },
            Phase::Changed => NativeClipboardEvent::Changed {
                generation,
                mimes: vec![],
                inline_text: None,
                epoch: 0,
            },
            Phase::Captured => NativeClipboardEvent::Captured {
                epoch: 0,
                generation,
                selection: LocalSelection::paths(vec![]),
            },
            Phase::Replaced => unreachable!(),
        }
    }

    #[test]
    fn every_same_generation_permutation_advances_only_to_higher_phases() {
        use Phase::*;
        for phases in [
            [Invalidated, Changed, Captured],
            [Invalidated, Captured, Changed],
            [Changed, Invalidated, Captured],
            [Changed, Captured, Invalidated],
            [Captured, Invalidated, Changed],
            [Captured, Changed, Invalidated],
        ] {
            let mut clock = ClipboardClock::default();
            let mut previous = None;
            for phase in phases {
                assert_eq!(
                    clock.accept(&event(10, phase)),
                    previous.is_none_or(|old| phase > old)
                );
                previous = Some(previous.map_or(phase, |old| old.max(phase)));
            }
            assert!(!clock.accept(&event(10, Captured)));
            assert!(!clock.accept(&event(9, Captured)));
            assert!(clock.accept(&event(11, Invalidated)));
        }
    }

    #[tokio::test]
    async fn bursts_coalesce_and_consumption_releases_capabilities_but_keeps_watermark() {
        let (sender, mut receiver) = channel();
        let access = Arc::new(());
        let weak = Arc::downgrade(&access);
        sender
            .send(NativeClipboardEvent::Captured {
                epoch: 0,
                generation: 1,
                selection: LocalSelection {
                    roots: vec![],
                    access: Some(access),
                },
            })
            .unwrap();
        for generation in 2..10_000 {
            sender.send(event(generation, Phase::Changed)).unwrap();
        }
        assert!(weak.upgrade().is_none());
        assert_eq!(
            receiver.recv().await.unwrap().stamp(),
            (9999, Phase::Changed)
        );
        assert_eq!(sender.latest_generation(), Some(9999));
        sender.send(event(9998, Phase::Captured)).unwrap();
        sender.send(event(9999, Phase::Invalidated)).unwrap();
        assert!(sender.pending.lock().event.is_none());
        let access = Arc::new(());
        let weak = Arc::downgrade(&access);
        sender
            .send(NativeClipboardEvent::Captured {
                epoch: 0,
                generation: 9999,
                selection: LocalSelection {
                    roots: vec![],
                    access: Some(access),
                },
            })
            .unwrap();
        drop(receiver.recv().await.unwrap());
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn slow_capture_keeps_the_epoch_of_early_invalidation_after_disable() {
        let (sender, mut receiver) = channel();
        sender.send(event(10, Phase::Invalidated)).unwrap();
        drop(receiver.recv().await.unwrap());
        let mut late = event(10, Phase::Captured);
        late.bind_epoch(1);
        sender.send(late).unwrap();
        assert_eq!(receiver.recv().await.unwrap().epoch(), 0);
        let mut fresh = event(11, Phase::Invalidated);
        fresh.bind_epoch(1);
        sender.send(fresh).unwrap();
        assert_eq!(receiver.recv().await.unwrap().epoch(), 1);
    }

    #[test]
    fn receiver_shutdown_releases_pending_capture_and_rejects_submission() {
        let (sender, receiver) = channel();
        let access = Arc::new(());
        let weak = Arc::downgrade(&access);
        sender
            .send(NativeClipboardEvent::Captured {
                epoch: 0,
                generation: 1,
                selection: LocalSelection {
                    roots: vec![],
                    access: Some(access),
                },
            })
            .unwrap();
        drop(receiver);
        assert!(weak.upgrade().is_none());
        assert!(sender.send(event(2, Phase::Changed)).is_err());
    }

    #[tokio::test]
    async fn replacement_blocks_queued_and_taken_generation_without_enabling_legacy_clock() {
        let (sender, mut receiver) = channel();
        let mut clock = ClipboardClock::default();
        clock.invalidate_pending(sender.replace_pending());
        assert!(!clock.ordered());
        assert!(clock.accept(&event(1, Phase::Captured)));
        sender.send(event(100, Phase::Captured)).unwrap();
        clock.invalidate_pending(sender.replace_pending());
        assert!(sender.pending.lock().event.is_none());
        sender.send(event(100, Phase::Captured)).unwrap();
        assert!(sender.pending.lock().event.is_none());
        assert!(!clock.accept(&event(100, Phase::Captured)));
        sender.send(event(101, Phase::Captured)).unwrap();
        let taken = receiver.recv().await.unwrap();
        clock.invalidate_pending(sender.replace_pending());
        assert!(!clock.accept(&taken));
        sender.send(event(102, Phase::Invalidated)).unwrap();
        assert!(clock.accept(&receiver.recv().await.unwrap()));
        sender.send(event(102, Phase::Changed)).unwrap();
        assert!(clock.accept(&receiver.recv().await.unwrap()));
        sender.send(event(102, Phase::Captured)).unwrap();
        assert!(clock.accept(&receiver.recv().await.unwrap()));
    }
}
