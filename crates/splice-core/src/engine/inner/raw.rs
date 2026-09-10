use super::*;
use crate::{
    input_settings::InputSettings,
    raw_transport::{self, Event},
};
use splice_proto::raw::{InputMode, QUEUE_REPORTS};

pub(super) struct RawState {
    pub operation: Arc<splice_platform::raw::RawOperation>,
    pub capture: Option<Arc<dyn splice_platform::raw::RawCapture>>,
    pub emulate: Option<Arc<dyn splice_platform::raw::RawEmulate>>,
    pub settings: InputSettings,
    pub error: Option<String>,
    pub preparing: Option<MachineId>,
    pub active: bool,
    pub boundary: bool,
    pub pending_policy: bool,
    pub pending_boundary: Option<(u64, u32)>,
    pub pending_target: Option<(MachineId, u64)>,
    pub connecting: bool,
    pub edge: Option<u32>,
    pub job: Option<tokio::task::JoinHandle<()>>,
    pub finish: Option<tokio::sync::oneshot::Sender<()>>,
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
            operation: Arc::default(),
            capture,
            emulate,
            settings,
            error: None,
            preparing: None,
            active: false,
            boundary: false,
            pending_policy: false,
            pending_boundary: None,
            pending_target: None,
            connecting: false,
            edge: None,
            job: None,
            finish: None,
            events,
            tx,
        }
    }
}

impl Inner {
    pub(super) async fn raw_boundary_hit(&mut self, session: u64, edge: u32, along: f64) {
        let Focus::Driven(source) = self.focus.clone() else { return };
        if !self.raw.active
            || !self.raw.boundary
            || session != self.active_session
            || !along.is_finite()
            || self.raw.pending_boundary == Some((session, edge))
            || !self.raw_source_allowed(&source)
        {
            return;
        }
        let Some(link) = self.armed.get(edge as usize) else { return };
        if !self.links.contains(link) {
            return;
        }
        let margin = f64::from(self.cfg.corner_dead_zone)
            .min(f64::from(link.from_range.1 - link.from_range.0) / 4.0);
        if along <= f64::from(link.from_range.0) + margin
            || along >= f64::from(link.from_range.1) - margin
        {
            return;
        }
        let pos = match link.side {
            EdgeSide::Left | EdgeSide::Right => Vec2 { x: f64::from(link.at), y: along },
            EdgeSide::Top | EdgeSide::Bottom => Vec2 { x: along, y: f64::from(link.at) },
        };
        if let Some(net) = &self.net {
            if net.send_to(&source, Frame::RawBoundary { session, target: link.to.clone(), pos }) {
                self.raw.pending_boundary = Some((session, edge));
            }
        }
    }

    pub(super) async fn cross_raw_boundary(
        &mut self,
        from: &MachineId,
        session: u64,
        target: MachineId,
        pos: Vec2,
    ) {
        if self.focus != Focus::Remote(from.clone()) || !self.raw.active
            || session != self.active_session
        {
            return;
        }
        let link = (!self.raw.settings.focus_lock && pos.x.is_finite() && pos.y.is_finite())
            .then(|| {
                self.links.iter().find(|link| {
                    if &link.from != from || link.to != target { return false; }
                    let (cross, along) = match link.side {
                        EdgeSide::Left | EdgeSide::Right => (pos.x, pos.y),
                        EdgeSide::Top | EdgeSide::Bottom => (pos.y, pos.x),
                    };
                    let margin = f64::from(self.cfg.corner_dead_zone)
                        .min(f64::from(link.from_range.1 - link.from_range.0) / 4.0);
                    cross == f64::from(link.at)
                        && along > f64::from(link.from_range.0) + margin
                        && along < f64::from(link.from_range.1) - margin
                }).cloned()
            })
            .flatten();
        let Some(link) = link else {
            if let Some(net) = &self.net {
                net.send_to(from, Frame::RawBoundaryAck { session });
            }
            return;
        };
        let landing = layout::clamp_into_displays(self.display_slice_of(&target), position_inside_to_edge(&link, pos));
        if target == self.self_info.id {
            self.end_remote(from, LeaveReason::Crossed, Some(landing), true).await;
        } else {
            self.handoff_remote(target, landing).await;
        }
    }

    pub(super) async fn raw_boundary_policy(&mut self, from: &MachineId, session: u64, boundary: bool) {
        if !self.raw.active
            || session != self.active_session
            || self.focus != Focus::Driven(from.clone())
        {
            return;
        }
        self.raw.boundary = boundary;
        self.raw.pending_boundary = None;
        let Some(target) = self.raw.emulate.clone() else { return };
        if let Err(error) = target.boundary_policy(session, boundary) {
            self.raw.error = Some(format!("Cannot update the raw boundary: {error}"));
            self.end_driven(from, Some(LeaveReason::CaptureLost)).await;
        }
        self.touch_ui();
    }

    pub(super) async fn finish_raw(&mut self) {
        let Some(finish) = self.raw.finish.take() else { return };
        if let Some(capture) = &self.raw.capture {
            capture.end();
        }
        let _ = finish.send(());
        if let Some(mut job) = self.raw.job.take() {
            if tokio::time::timeout(crate::input_transport::INPUT_TIMEOUT, &mut job).await.is_err() {
                job.abort();
                let _ = job.await;
                self.raw.error = Some("Raw input could not finish delivery before the handoff deadline".into());
            }
        }
        if let Some(error) = self.raw.operation.error() {
            self.raw.error = Some(error);
        }
    }

    pub(super) async fn stop_raw(&mut self) {
        self.raw.operation = Arc::default();
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
        self.raw.finish = None;
        self.raw.active = false;
        self.raw.boundary = false;
        self.raw.pending_policy = false;
        self.raw.pending_boundary = None;
        self.raw.pending_target = None;
        self.raw.connecting = false;
        self.raw.edge = None;
        self.raw.preparing = None;
    }

    pub(super) async fn start_raw(&mut self, target: MachineId, pos: Vec2, edge: Option<u32>) {
        let readiness = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                matches!(self.self_info.os, Os::Macos | Os::Linux),
                "Raw capture requires macOS or Linux"
            );
            anyhow::ensure!(
                self.cfg.master_enabled
                    && self.peer_usable(&target)
                    && self.machine_enabled(&target),
                "destination is not available"
            );
            anyhow::ensure!(
                self.peers
                    .get(&target)
                    .and_then(|p| p.info.as_ref())
                    .is_some_and(|i| i.os == Os::Linux),
                "Raw input requires a Linux destination"
            );
            self.raw
                .capture
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("The platform has no raw input capture backend"))?
                .prepare()?;
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
                    boundary: !self.raw.settings.focus_lock,
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

    pub(super) async fn prepare_raw_target(&mut self, from: MachineId, session: u64, pos: Vec2, boundary: bool) {
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
        let net = net.clone();
        self.stop_raw().await;
        self.raw.pending_policy = boundary;
        self.raw.pending_target = Some((from.clone(), session));
        let tx = self.raw.tx.clone();
        let operation = self.raw.operation.clone();
        self.raw.job = Some(tokio::spawn(async move {
            let prepare = async {
                let reservation = raw_transport::Reservation::from_endpoint(net.raw_endpoint().await?)?;
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
        let _ = self.capture.end_capture(None).await;
        self.crossing = None;
        if let Err(error) = target.begin(session) {
            self.reject_raw(
                &from,
                session,
                format!("Cannot begin raw injection: {error}"),
            );
            return;
        }
        if let Err(error) = target.boundary_policy(session, self.raw.pending_policy) {
            target.end(session).ok();
            self.reject_raw(
                &from,
                session,
                format!("Cannot arm the raw boundary: {error}"),
            );
            return;
        }
        let pos = raw_landing(&self.self_info.displays, pos);
        if let Err(error) = self.emulate.enter(pos).await {
            target.end(session).ok();
            self.reject_raw(&from, session, format!("Cannot place raw pointer: {error}"));
            return;
        }
        self.focus = Focus::Driven(from.clone());
        self.active_session = session;
        self.raw.active = true;
        self.raw.boundary = self.raw.pending_policy;
        self.raw.pending_boundary = None;
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
                if let Err(error) = capture.begin(output, self.raw.edge, self.raw.operation.clone())
                {
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
                let (finish, finished) = tokio::sync::oneshot::channel();
                self.raw.finish = Some(finish);
                self.raw.job = Some(tokio::spawn(async move {
                    let result = raw_transport::send(stream, session, reports, finished).await;
                    capture.end();
                    if let Err(error) = &result {
                        if operation.error().is_none() {
                            operation.fail(format!("Raw input ended: {error:#}"));
                        }
                    }
                    let error = match operation.error() {
                        Some(reason) => reason,
                        None => match result {
                            Ok(()) => "Raw capture ended".into(),
                            Err(error) => format!("Raw input ended: {error:#}"),
                        },
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
        if target == self.self_info.id {
            if let Focus::Remote(old) = self.focus.clone() {
                self.end_remote(&old, LeaveReason::Crossed, Some(self.last_local_pos), true)
                    .await;
            }
            return;
        }
        if self.self_info.os == Os::Linux && !matches!(self.focus, Focus::Remote(_)) {
            self.raw.error = Some("Start control from Linux by crossing a screen edge. Once captured, Ctrl+Alt+F12 switches computers.".into());
            self.touch_ui();
            return;
        }
        if matches!(self.focus, Focus::Driven(_))
            || !self.cfg.master_enabled
            || !self.machine_enabled(&self.self_info.id)
            || !self.machine_enabled(&target)
            || !self.peer_usable(&target)
        {
            if let Focus::Remote(old) = self.focus.clone() {
                self.end_remote(
                    &old,
                    LeaveReason::CaptureLost,
                    Some(self.last_local_pos),
                    true,
                )
                .await;
            }
            self.raw.error = Some("The selected destination is not available".into());
            self.touch_ui();
            return;
        }
        let pos = layout::clamp_into_displays(
            self.display_slice_of(&target),
            Vec2 { x: 100.0, y: 100.0 },
        );
        self.handoff_remote(target, pos).await;
    }

    pub(super) async fn handoff_remote(&mut self, target: MachineId, pos: Vec2) {
        let mut held = None;
        if let Focus::Remote(old) = self.focus.clone() {
            if self.self_info.os == Os::Linux {
                if !self.raw.active && self.raw.preparing.is_none() {
                    held = Some(self.source_ledger.clone());
                }
                self.leave_remote(&old, LeaveReason::Crossed, true).await;
            } else {
                self.end_remote(&old, LeaveReason::Crossed, Some(self.last_local_pos), true)
                    .await;
            }
        }
        if self.raw.settings.mode(&target) == InputMode::Raw {
            self.start_raw(target, pos, None).await;
        } else {
            self.start_desktop(target.clone(), pos).await;
            if self.focus == Focus::Remote(target.clone()) {
                if let Some(held) = held {
                    if let Some(net) = &self.net {
                        for event in held.presses() {
                            net.send_to(
                                &target,
                                Frame::Input {
                                    session: self.active_session,
                                    ev: event,
                                },
                            );
                        }
                    }
                    self.source_ledger = held;
                }
            }
        }
    }

    pub(super) async fn switch_target(&mut self) {
        for code in [29, 56, 88] {
            self.source_ledger.observe(&InputEvent::Key {
                code,
                pressed: false,
            });
        }
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
        } else if let Focus::Remote(old) = self.focus.clone() {
            self.end_remote(&old, LeaveReason::Crossed, Some(self.last_local_pos), true)
                .await;
        }
    }
}

fn raw_landing(displays: &[DisplayRect], pos: Vec2) -> Vec2 {
    let pos = layout::clamp_into_displays(displays, pos);
    let Some(display) = displays
        .iter()
        .filter(|display| display.w > 0 && display.h > 0)
        .find(|display| {
            pos.x >= f64::from(display.x)
                && pos.x < f64::from(display.x) + f64::from(display.w)
                && pos.y >= f64::from(display.y)
                && pos.y < f64::from(display.y) + f64::from(display.h)
        })
    else {
        return pos;
    };
    let pad_x = (f64::from(display.w) / 2.0).min(8.0);
    let pad_y = (f64::from(display.h) / 2.0).min(8.0);
    Vec2 {
        x: pos.x.clamp(
            f64::from(display.x) + pad_x,
            f64::from(display.x) + f64::from(display.w) - pad_x,
        ),
        y: pos.y.clamp(
            f64::from(display.y) + pad_y,
            f64::from(display.y) + f64::from(display.h) - pad_y,
        ),
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
                splice_platform::raw::clock::now_us(),
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
    async fn a_replacement_control_connection_ends_both_desktop_roles() {
        for source in [true, false] {
            let (_dir, mut inner, mock) = fixture();
            let peer = MachineId("a".into());
            inner.active_session = 1;
            if source {
                inner.focus = Focus::Remote(peer.clone());
                inner.capture.begin_capture().await.unwrap();
                inner.source_ledger.observe(&InputEvent::Key { code: 42, pressed: true });
            } else {
                inner.focus = Focus::Driven(peer.clone());
                inner.target_ledger.observe(&InputEvent::Key { code: 42, pressed: true });
                inner.emulate.enter(Vec2 { x: 40.0, y: 40.0 }).await.unwrap();
            }
            inner.on_peer_event(crate::net::PeerEvent::Connected {
                id: peer.clone(),
                hello: splice_proto::MachineInfo { id: peer, hostname: "a".into(), os: Os::Linux, displays: splice_platform::mock::one_display(), build: splice_proto::BuildInfo::current() },
                caps: Vec::new(),
                addr: "127.0.0.1:41717".parse().unwrap(),
            }).await;
            assert!(inner.focus == Focus::Local);
            assert!(!mock.state.lock().capturing);
            if source {
                assert!(inner.source_ledger.presses().is_empty());
            } else {
                assert!(inner.target_ledger.presses().is_empty());
                assert!(mock.state.lock().release_all_calls > 0);
                assert!(mock.state.lock().left > 0);
            }
        }
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

    fn boundary_fixture(focus_lock: bool) -> (tempfile::TempDir, Inner, splice_platform::mock::MockHandle, MachineId) {
        let (dir, mut inner, mock) = fixture();
        let source = MachineId("a".into());
        inner.self_info.id = MachineId("b".into());
        inner.focus = Focus::Remote(source.clone());
        inner.raw.active = true;
        inner.active_session = 1;
        inner.raw.settings.focus_lock = focus_lock;
        inner.links = vec![EdgeLink {
            from: source.clone(),
            to: MachineId("b".into()),
            side: EdgeSide::Right,
            at: 1920,
            from_range: (0, 1080),
            to_range: (0, 1080),
            to_at: 0,
        }];
        (dir, inner, mock, source)
    }

    #[tokio::test]
    async fn a_valid_raw_boundary_crossing_returns_control_home() {
        let (_dir, mut inner, mock, source) = boundary_fixture(false);
        inner
            .cross_raw_boundary(&source, 1, MachineId("b".into()), Vec2 { x: 1920.0, y: 500.0 })
            .await;
        assert!(inner.focus == Focus::Local);
        assert_eq!(mock.state.lock().capture_ends.len(), 1);
        assert!(!inner.raw.active);
    }

    #[tokio::test]
    async fn rejected_raw_boundary_crossings_keep_the_session() {
        for (focus_lock, session, x, y) in [
            (true, 1, 1920.0, 500.0),
            (false, 2, 1920.0, 500.0),
            (false, 1, f64::NAN, 500.0),
            (false, 1, 1920.0, 4.0),
            (false, 1, 1920.0, 1076.0),
        ] {
            let (_dir, mut inner, mock, source) = boundary_fixture(focus_lock);
            inner
                .cross_raw_boundary(&source, session, MachineId("b".into()), Vec2 { x, y })
                .await;
            assert!(inner.focus == Focus::Remote(source.clone()));
            assert!(inner.raw.active);
            assert!(mock.state.lock().capture_ends.is_empty());
        }
    }

    #[tokio::test]
    async fn an_unknown_boundary_target_keeps_the_session() {
        let (_dir, mut inner, mock, source) = boundary_fixture(false);
        inner
            .cross_raw_boundary(&source, 1, MachineId("c".into()), Vec2 { x: 1920.0, y: 500.0 })
            .await;
        assert!(inner.focus == Focus::Remote(source.clone()));
        assert!(mock.state.lock().capture_ends.is_empty());
    }

    #[test]
    fn raw_landing_insets_the_pointer_from_display_edges() {
        let display = DisplayRect {
            id: "1".into(),
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            scale: 1.0,
        };
        assert_eq!(
            raw_landing(std::slice::from_ref(&display), Vec2 { x: 1.0, y: 500.0 }),
            Vec2 { x: 8.0, y: 500.0 }
        );
        assert_eq!(
            raw_landing(std::slice::from_ref(&display), Vec2 { x: 1919.0, y: 500.0 }),
            Vec2 { x: 1912.0, y: 500.0 }
        );
        assert_eq!(
            raw_landing(std::slice::from_ref(&display), Vec2 { x: 900.0, y: 500.0 }),
            Vec2 { x: 900.0, y: 500.0 }
        );
        let small = DisplayRect {
            id: "2".into(),
            x: 0,
            y: 0,
            w: 10,
            h: 10,
            scale: 1.0,
        };
        assert_eq!(
            raw_landing(std::slice::from_ref(&small), Vec2 { x: 0.0, y: 0.0 }),
            Vec2 { x: 5.0, y: 5.0 }
        );
        let right = DisplayRect {
            id: "3".into(),
            x: 1920,
            y: 0,
            w: 1920,
            h: 1080,
            scale: 1.0,
        };
        assert_eq!(
            raw_landing(&[display, right], Vec2 { x: 1920.0, y: 500.0 }),
            Vec2 { x: 1928.0, y: 500.0 }
        );
    }
}
