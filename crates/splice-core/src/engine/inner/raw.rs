use super::*;
use crate::{
    input_settings::InputSettings,
    raw_transport::{self, Event},
};
use splice_proto::raw::{InputMode, QUEUE_REPORTS};

pub(super) struct RawState {
    pub operation: Arc<()>,
    pub capture: Option<Arc<dyn splice_platform::raw::RawCapture>>,
    pub emulate: Option<Arc<dyn splice_platform::raw::RawEmulate>>,
    pub settings: InputSettings,
    pub error: Option<String>,
    pub preparing: Option<MachineId>,
    pub active: bool,
    pub pending_target: Option<(MachineId, u64)>,
    pub connecting: bool,
    pub edge: Option<u32>,
    pub job: Option<tokio::task::JoinHandle<()>>,
    pub events: mpsc::UnboundedReceiver<Event>,
    pub tx: mpsc::UnboundedSender<Event>,
}

impl RawState {
    pub fn new(
        capture: Option<Arc<dyn splice_platform::raw::RawCapture>>,
        emulate: Option<Arc<dyn splice_platform::raw::RawEmulate>>,
        settings: InputSettings,
    ) -> Self {
        let (tx, events) = mpsc::unbounded_channel();
        Self {
            operation: Arc::new(()),
            capture,
            emulate,
            settings,
            error: None,
            preparing: None,
            active: false,
            pending_target: None,
            connecting: false,
            edge: None,
            job: None,
            events,
            tx,
        }
    }
}

impl Inner {
    pub(super) async fn stop_raw(&mut self) {
        self.raw.operation = Arc::new(());
        if let Some(capture) = &self.raw.capture {
            capture.end();
        }
        if let Some(job) = self.raw.job.take() {
            job.abort();
            let _ = job.await;
        }
        if let Some(emulate) = &self.raw.emulate {
            if let Err(error) = emulate.end(self.active_session) {
                self.raw.error = Some(format!("Cannot release raw input: {error}"));
            }
        }
        self.raw.active = false;
        self.raw.pending_target = None;
        self.raw.connecting = false;
        self.raw.edge = None;
        self.raw.preparing = None;
    }

    pub(super) async fn start_raw(&mut self, target: MachineId, pos: Vec2, edge: Option<u32>) {
        let readiness = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                self.self_info.os == Os::Macos,
                "Raw capture is available on macOS only"
            );
            anyhow::ensure!(
                self.cfg.master_enabled
                    && self.peer_usable(&target)
                    && self.machine_enabled(&target),
                "destination is not available"
            );
            anyhow::ensure!(self.raw.settings.focus_lock, "Raw input requires Focus lock: automatic destination edge observations are not available on this release. Use Ctrl+Alt+F12 to switch computers.");
            anyhow::ensure!(
                self.peers
                    .get(&target)
                    .and_then(|p| p.info.as_ref())
                    .is_some_and(|i| i.os == Os::Linux),
                "Raw input supports Mac sources and Linux destinations"
            );
            self.raw
                .capture
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Raw capture is available on macOS only"))?
                .readiness()?;
            Ok(())
        })();
        if let Err(error) = readiness {
            self.raw.error = Some(error.to_string());
            self.capture
                .end_capture(Some(self.last_local_pos))
                .await
                .ok();
            self.touch_ui();
            return;
        }
        self.stop_raw().await;
        self.claim_source();
        self.session += 1;
        self.active_session = self.session;
        self.raw.error = None;
        self.raw.edge = edge;
        self.raw.preparing = Some(target.clone());
        self.focus = Focus::Remote(target.clone());
        self.virtual_pos = pos;
        if !self.net.as_ref().is_some_and(|net| {
            net.send_to(
                &target,
                Frame::RawPrepare {
                    session: self.active_session,
                    pos,
                },
            )
        }) {
            self.raw.error = Some("The control connection closed before raw preparation".into());
            self.end_remote(
                &target,
                LeaveReason::CaptureLost,
                Some(self.last_local_pos),
                true,
            )
            .await;
            return;
        }
        let tx = self.raw.tx.clone();
        let operation = self.raw.operation.clone();
        let session = self.active_session;
        self.raw.job = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let _ = tx.send(Event::Ended {
                operation,
                peer: target,
                session,
                error: "Raw destination preparation timed out".into(),
            });
        }));
        self.touch_ui();
    }

    fn raw_source_allowed(&self, from: &MachineId) -> bool {
        self.cfg.master_enabled
            && self.machine_enabled(&self.self_info.id)
            && self.machine_enabled(from)
            && self.peer_usable(from)
            && self.claim.as_ref().is_some_and(|c| &c.writer == from)
    }

    fn reject_raw(&mut self, from: &MachineId, session: u64, reason: String) {
        self.raw.error = Some(reason.clone());
        if let Some(net) = &self.net {
            net.send_to(from, Frame::RawReject { session, reason });
        }
        self.touch_ui();
    }

    pub(super) async fn prepare_raw_target(&mut self, from: MachineId, session: u64, pos: Vec2) {
        if session == 0
            || !self.raw_source_allowed(&from)
            || self.focus != Focus::Local
            || self.raw.pending_target.is_some()
        {
            self.reject_raw(
                &from,
                session,
                "Raw source does not own an available destination".into(),
            );
            return;
        }
        let peer = self.peers.get_mut(&from).expect("authorized peer exists");
        if session <= peer.raw_generation {
            self.reject_raw(
                &from,
                session,
                "Raw session generation was already used".into(),
            );
            return;
        }
        peer.raw_generation = session;
        let Some(target) = self.raw.emulate.clone() else {
            self.reject_raw(
                &from,
                session,
                "Raw input injection is available on Linux only".into(),
            );
            return;
        };
        let Some(net) = &self.net else {
            return;
        };
        let bind = net.bind_ip();
        self.stop_raw().await;
        self.raw.pending_target = Some((from.clone(), session));
        let tx = self.raw.tx.clone();
        let operation = self.raw.operation.clone();
        self.raw.job = Some(tokio::spawn(async move {
            let prepare = async {
                let reservation = raw_transport::Reservation::bind(bind).await?;
                target.prepare().await?;
                Ok::<_, anyhow::Error>(reservation)
            };
            let result = match tokio::time::timeout(Duration::from_secs(5), prepare).await {
                Ok(result) => result.map_err(|e| format!("Raw input is unavailable: {e:#}")),
                Err(_) => Err("Raw device preparation timed out".into()),
            };
            let _ = tx.send(Event::Prepared {
                operation,
                peer: from,
                session,
                pos,
                result,
            });
        }));
    }

    async fn prepared_raw_target(
        &mut self,
        from: MachineId,
        session: u64,
        pos: Vec2,
        result: Result<raw_transport::Reservation, String>,
    ) {
        if self.raw.pending_target.as_ref() != Some(&(from.clone(), session)) {
            return;
        }
        self.raw.pending_target = None;
        if !self.raw_source_allowed(&from) || self.focus != Focus::Local {
            self.reject_raw(
                &from,
                session,
                "Input ownership changed during raw preparation".into(),
            );
            return;
        }
        let reservation = match result {
            Ok(reservation) => reservation,
            Err(reason) => {
                self.reject_raw(&from, session, reason);
                return;
            }
        };
        let target = self
            .raw
            .emulate
            .clone()
            .expect("preparation checked injection backend");
        if let Err(error) = self.emulate.enter(pos).await {
            self.reject_raw(&from, session, format!("Cannot place raw pointer: {error}"));
            return;
        }
        if let Err(error) = target.begin(session) {
            self.emulate.leave().await.ok();
            self.reject_raw(
                &from,
                session,
                format!("Cannot begin raw injection: {error}"),
            );
            return;
        }
        self.focus = Focus::Driven(from.clone());
        self.active_session = session;
        self.raw.active = true;
        let port = reservation.port;
        let ticket = reservation.ticket;
        let ts = self.ts.clone();
        let tx = self.raw.tx.clone();
        let operation = self.raw.operation.clone();
        let peer = from.clone();
        let Some(expected_ip) = self.net.as_ref().and_then(|net| net.peer_ip(&from)) else {
            self.raw.error = Some("Raw source lost its control address".into());
            self.end_driven(&from, Some(LeaveReason::CaptureLost)).await;
            return;
        };
        self.raw.job = Some(tokio::spawn(async move {
            let error = match reservation
                .receive(peer.clone(), expected_ip, session, ts, target)
                .await
            {
                Ok(()) => "Raw source closed the connection".into(),
                Err(error) => format!("Raw input ended: {error:#}"),
            };
            let _ = tx.send(Event::Ended {
                operation,
                peer,
                session,
                error,
            });
        }));
        if !self.net.as_ref().is_some_and(|net| {
            net.set_active(&from, true);
            net.send_to(
                &from,
                Frame::RawReady {
                    session,
                    port,
                    ticket,
                },
            )
        }) {
            self.end_driven(&from, None).await;
        }
        self.touch_ui();
    }

    pub(super) async fn raw_ready(
        &mut self,
        from: MachineId,
        session: u64,
        port: u16,
        ticket: [u8; 32],
    ) {
        if self.raw.connecting
            || self.active_session != session
            || self.raw.preparing.as_ref() != Some(&from)
            || self.focus != Focus::Remote(from.clone())
        {
            return;
        }
        let Some(net) = &self.net else {
            return;
        };
        let Some(ip) = net.peer_ip(&from) else {
            self.raw.error = Some("Raw destination has no tailnet address".into());
            return;
        };
        self.raw.connecting = true;
        let bind = net.bind_ip();
        let ts = self.ts.clone();
        let tx = self.raw.tx.clone();
        let operation = self.raw.operation.clone();
        if let Some(job) = self.raw.job.take() {
            job.abort();
            let _ = job.await;
        }
        self.raw.job = Some(tokio::spawn(async move {
            let event = match raw_transport::connect(
                bind,
                SocketAddr::new(ip, port),
                session,
                ticket,
                ts,
                &from,
            )
            .await
            {
                Ok(stream) => Event::Connected {
                    operation,
                    peer: from,
                    session,
                    stream,
                },
                Err(error) => Event::Ended {
                    operation,
                    peer: from,
                    session,
                    error: format!("Cannot connect raw input: {error:#}"),
                },
            };
            let _ = tx.send(event);
        }));
    }

    pub(super) async fn on_raw_event(&mut self, event: Event) {
        if !event.belongs_to(&self.raw.operation) {
            return;
        }
        match event {
            Event::Prepared {
                peer,
                session,
                pos,
                result,
                ..
            } => self.prepared_raw_target(peer, session, pos, result).await,
            Event::Connected {
                peer,
                session,
                stream,
                ..
            } => {
                if session != self.active_session
                    || self.raw.preparing.as_ref() != Some(&peer)
                    || self.focus != Focus::Remote(peer.clone())
                {
                    return;
                }
                let (output, reports) = mpsc::channel(QUEUE_REPORTS);
                let capture = self
                    .raw
                    .capture
                    .as_ref()
                    .expect("raw preparation checked capture");
                if let Err(error) = capture.begin(output, self.raw.edge) {
                    self.raw.error = Some(format!("Cannot capture raw input: {error}"));
                    self.end_remote(
                        &peer,
                        LeaveReason::CaptureLost,
                        Some(self.last_local_pos),
                        true,
                    )
                    .await;
                    return;
                }
                self.raw.active = true;
                self.raw.preparing = None;
                let tx = self.raw.tx.clone();
                let operation = self.raw.operation.clone();
                let capture = capture.clone();
                if let Some(net) = &self.net {
                    net.set_active(&peer, true);
                }
                self.raw.job = Some(tokio::spawn(async move {
                    let result = raw_transport::send(stream, session, reports).await;
                    capture.end();
                    let error = match result {
                        Ok(()) => "Raw capture ended".into(),
                        Err(error) => format!("Raw input ended: {error:#}"),
                    };
                    let _ = tx.send(Event::Ended {
                        operation,
                        peer,
                        session,
                        error,
                    });
                }));
            }
            Event::Ended {
                peer,
                session,
                error,
                ..
            } => {
                if session != self.active_session {
                    return;
                }
                if self.focus == Focus::Remote(peer.clone()) {
                    self.raw.error = Some(error);
                    self.end_remote(
                        &peer,
                        LeaveReason::CaptureLost,
                        Some(self.last_local_pos),
                        true,
                    )
                    .await;
                } else if self.focus == Focus::Driven(peer.clone()) {
                    self.raw.error = Some(error);
                    self.end_driven(&peer, Some(LeaveReason::CaptureLost)).await;
                }
            }
        }
        self.touch_ui();
    }

    pub(super) async fn select_target(&mut self, target: MachineId) {
        if self.self_info.os != Os::Macos && target != self.self_info.id {
            self.raw.error = Some("Selecting a destination by shortcut is supported on Mac sources. Use screen edges on Linux.".into());
            self.touch_ui();
            return;
        }
        if let Focus::Remote(old) = self.focus.clone() {
            self.end_remote(&old, LeaveReason::Crossed, Some(self.last_local_pos), true)
                .await;
        }
        if target == self.self_info.id {
            return;
        }
        if self.focus != Focus::Local
            || !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.machine_enabled(&target)
            || !self.peer_usable(&target)
        {
            self.raw.error = Some("The selected destination is not available".into());
            self.touch_ui();
            return;
        }
        if self.raw.settings.mode(&target) == InputMode::Raw {
            let pos = layout::clamp_into_displays(
                self.display_slice_of(&target),
                Vec2 { x: 100.0, y: 100.0 },
            );
            self.start_raw(target, pos, None).await;
        } else {
            let pos = layout::clamp_into_displays(
                self.display_slice_of(&target),
                Vec2 { x: 100.0, y: 100.0 },
            );
            self.start_desktop(target, pos).await;
        }
    }

    pub(super) async fn switch_target(&mut self) {
        let current = match &self.focus {
            Focus::Remote(peer) => peer,
            _ => &self.self_info.id,
        };
        let mut machines: Vec<_> = self
            .layout
            .as_ref()
            .into_iter()
            .flat_map(|d| d.machines.iter())
            .filter(|(id, p)| p.enabled && (**id == self.self_info.id || self.peer_usable(id)))
            .map(|(id, p)| (p.offset.x, p.offset.y, id.clone()))
            .collect();
        machines.sort();
        if let Some(index) = machines.iter().position(|(_, _, id)| id == current) {
            self.select_target(machines[(index + 1) % machines.len()].2.clone())
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splice_proto::raw::{RawEvent, RawReport};

    fn fixture() -> (tempfile::TempDir, Inner, splice_platform::mock::MockHandle) {
        let dir = tempfile::tempdir().unwrap();
        let (platform, mock) = splice_platform::mock::create(splice_platform::mock::one_display());
        let (_commands, cmd) = mpsc::unbounded_channel();
        let (ui, _) = watch::channel(UiState::initial(MachineId("b".into())));
        let (ready, _) = watch::channel(None);
        let inner = Inner::new(
            platform,
            Arc::new(crate::raw_transport::tests::Identity("b")),
            dir.path().into(),
            NetOpts::default(),
            Duration::from_secs(1),
            cmd,
            ui,
            ready,
            None,
        )
        .unwrap();
        (dir, inner, mock)
    }

    #[tokio::test]
    async fn a_late_socket_failure_cannot_release_a_reconnected_session() {
        let (_dir, mut inner, mock) = fixture();
        let old_operation = inner.raw.operation.clone();
        inner.stop_raw().await;
        let peer = MachineId("a".into());
        inner.active_session = 1;
        inner.focus = Focus::Driven(peer.clone());
        inner.raw.active = true;
        let target = inner.raw.emulate.clone().unwrap();
        target.prepare().await.unwrap();
        target.begin(1).unwrap();
        target
            .inject(
                1,
                &RawReport {
                    device: 1,
                    sequence: 0,
                    captured_us: 0,
                    events: vec![RawEvent::Key {
                        code: 42,
                        pressed: true,
                    }],
                },
            )
            .unwrap();
        inner
            .on_raw_event(Event::Ended {
                operation: old_operation,
                peer: peer.clone(),
                session: 1,
                error: "old connection closed".into(),
            })
            .await;
        assert!(inner.raw.active);
        assert!(inner.focus == Focus::Driven(peer.clone()));
        assert_eq!(mock.state.lock().raw_session, Some(1));
        assert_eq!(mock.state.lock().raw_events.len(), 1);
        assert!(inner.raw.error.is_none());
        inner
            .on_raw_event(Event::Ended {
                operation: inner.raw.operation.clone(),
                peer,
                session: 1,
                error: "current connection closed".into(),
            })
            .await;
        assert!(inner.focus == Focus::Local);
        assert_eq!(
            mock.state.lock().raw_events.last(),
            Some(&RawEvent::Key {
                code: 42,
                pressed: false
            })
        );
    }
    #[tokio::test]
    async fn cancellation_and_panic_invalidate_uncommitted_preparation() {
        for frame in [
            Frame::Leave {
                session: 1,
                reason: LeaveReason::Crossed,
            },
            Frame::Panic,
        ] {
            let (_dir, mut inner, mock) = fixture();
            let peer = MachineId("a".into());
            inner.raw.pending_target = Some((peer.clone(), 1));
            let operation = inner.raw.operation.clone();
            inner.on_frame(Arc::new(peer.clone()), frame).await;
            assert!(inner.raw.pending_target.is_none());
            inner
                .on_raw_event(Event::Prepared {
                    operation,
                    peer,
                    session: 1,
                    pos: Vec2 { x: 1.0, y: 1.0 },
                    result: Err("late preparation result".into()),
                })
                .await;
            assert!(inner.raw.error.is_none());
            assert!(inner.focus == Focus::Local);
            assert!(mock.state.lock().entered.is_empty());
            assert!(mock.state.lock().raw_session.is_none());
        }
    }
}
