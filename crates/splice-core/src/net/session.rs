//! Per-peer session: handshake (Hello/Welcome), dedupe, framing, heartbeats.
//!
//! A session task owns its socket exclusively. A dedicated reader task pumps frames
//! into an mpsc so the main loop can select over reads, engine commands and the
//! heartbeat timer without cancel-safety hazards (framing::read_frame is only
//! cancel-safe at the length-prefix boundary).

use crate::net::{NetControlInner, PeerEvent};
use crate::diagnostics::{ConnectionPhase, Traffic, unix_ms};
use splice_proto::framing::{read_frame, read_frame_buffered, write_frame_buffered};
use splice_proto::{caps, Frame, Hello, MachineId, ProtoError, Welcome};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, Notify, OwnedSemaphorePermit};

#[derive(Default)]
pub(crate) struct Liveness {
    pub enabled: AtomicBool,
    pub changed: Notify,
}

async fn write_frame<W: AsyncWrite + Unpin>(
    inner: &NetControlInner,
    writer: &mut W,
    frame: &Frame,
) -> Result<(), ProtoError> {
    write_with_timeout(inner.opts.write_timeout, writer, frame, &mut Vec::new()).await
}

async fn write_with_timeout<W: AsyncWrite + Unpin>(
    timeout: Duration,
    writer: &mut W,
    frame: &Frame,
    buffer: &mut Vec<u8>,
) -> Result<(), ProtoError> {
    tokio::time::timeout(timeout, write_frame_buffered(writer, frame, buffer))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "peer write timed out"))?
}

#[derive(Clone)]
pub(crate) struct SessionControl {
    pub(crate) input: crate::input_transport::desktop::Control,
    frames: mpsc::Sender<QueuedFrame>,
    bulk: mpsc::Sender<QueuedFrame>,
    files: mpsc::Sender<QueuedFrame>,
    traffic: Arc<Traffic>,
    shutdown: watch::Sender<Option<String>>,
}

struct QueuedFrame {
    frame: Frame,
    queued: Instant,
}

impl SessionControl {
    fn sender(&self, frame: &Frame) -> &mpsc::Sender<QueuedFrame> {
        match frame {
            Frame::Files(_) | Frame::FileClipboardRef { .. } | Frame::FileRoute { .. } => &self.files,
            Frame::ClipChunk { .. } => &self.bulk,
            _ => &self.frames,
        }
    }

    fn depth(&self) -> usize {
        [&self.frames, &self.files, &self.bulk].into_iter()
            .map(|queue| queue.max_capacity() - queue.capacity()).sum()
    }

    pub async fn send_wait(&self, frame: Frame, timeout: Duration) -> bool {
        if matches!(frame, Frame::Input { .. } | Frame::Enter { .. }) {
            return self.send(frame);
        }
        let sender = self.sender(&frame);
        match tokio::time::timeout(timeout, sender.reserve()).await {
            Ok(Ok(permit)) => {
                self.traffic.queued(self.depth());
                permit.send(QueuedFrame { frame, queued: Instant::now() });
                true
            }
            _ => false,
        }
    }

    pub fn send(&self, frame: Frame) -> bool {
        if let Frame::Input { session, ev } = frame {
            let sent = self.input.push(session, ev);
            if !sent { self.close("UDP input queue exceeded its limit"); }
            return sent;
        }
        if let Frame::Enter { session, .. } = &frame {
            if !self.input.start(*session) { self.close("UDP session queue exceeded its limit"); return false; }
        }
        let sender = self.sender(&frame);
        let depth = self.depth() + 1;
        match sender.try_send(QueuedFrame { frame, queued: Instant::now() }) {
            Ok(()) => { self.traffic.queued(depth); true },
            Err(mpsc::error::TrySendError::Full(queued)) => {
                if matches!(queued.frame, Frame::Files(_) | Frame::FileClipboardRef { .. } | Frame::FileRoute { .. }) { return false; }
                self.close("outgoing queue exceeded its limit");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub fn close(&self, reason: &str) {
        self.shutdown.send_replace(Some(reason.into()));
    }
}

struct OutgoingFrames {
    priority: mpsc::Receiver<QueuedFrame>,
    bulk: mpsc::Receiver<QueuedFrame>,
    files: mpsc::Receiver<QueuedFrame>,
}

impl OutgoingFrames {
    async fn recv(&mut self) -> Option<QueuedFrame> {
        tokio::select! {
            biased;
            frame = self.priority.recv() => frame,
            frame = self.files.recv() => frame,
            frame = self.bulk.recv() => frame,
        }
    }
}

struct SessionCommands {
    input: crate::input_transport::desktop::Control,
    frames: OutgoingFrames,
    traffic: Arc<Traffic>,
    shutdown: watch::Receiver<Option<String>>,
}

fn our_caps(inner: &NetControlInner) -> Vec<String> {
    let mut caps: Vec<String> = [caps::INPUT_V1, caps::CLIPBOARD_V2, caps::LAYOUT_V1, caps::MASTER_V1].iter().map(|s| s.to_string()).collect();
    if inner.file_incoming.read().is_some() { caps.push(caps::FILES_V2.to_string()); }
    caps
}

fn reject(inner: &NetControlInner, peer: &MachineId, reason: String) {
    inner.phase(peer, ConnectionPhase::Rejected, None, Some(reason.clone()));
    tracing::warn!(%peer, %reason, "peer connection rejected");
    let _ = inner.events.send(PeerEvent::Rejected { id: peer.clone(), reason });
}

fn supports_required_capabilities(caps: &[String]) -> bool {
    [caps::INPUT_V1, caps::CLIPBOARD_V2, caps::LAYOUT_V1, caps::MASTER_V1].iter().all(|required| caps.iter().any(|cap| cap == required))
}

/// A Ping is missed when no Pong arrives within this multiple of the current cadence.
const MISS_WINDOW: f64 = 1.5;
/// PeerEvent::Rtt is emitted at most this often per peer.
const RTT_EMIT_MIN: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(crate) enum Role {
    Dialer,
    Listener,
}

/// Registered per-peer state shared with NetControl.
pub(crate) struct PeerSlot {
    pub seq: u64,
    pub traffic: Arc<Traffic>,
    pub control: SessionControl,
    /// True when this connection follows the smaller-id-dials rule from our side.
    pub rule_following: bool,
    /// Heartbeat cadence hint flipped by NetControl::set_active.
    pub active: Arc<Liveness>,
}

enum Registration {
    Fresh,
    /// We displaced a non-rule-following (or stale same-direction) connection.
    Replaced(SessionControl),
    /// A rule-following connection is already up; this one must go away.
    Lose,
}

fn try_register(inner: &NetControlInner, id: &MachineId, slot: PeerSlot) -> Registration {
    let mut peers = inner.peers.write();
    match peers.entry(id.clone()) {
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(slot);
            Registration::Fresh
        }
        std::collections::hash_map::Entry::Occupied(mut e) => {
            let keep_new = if slot.rule_following != e.get().rule_following {
                slot.rule_following
            } else {
                // Same direction twice: the old socket may be a half-open corpse
                // (heartbeats never close sockets), so the fresh one wins.
                true
            };
            if keep_new {
                Registration::Replaced(e.insert(slot).control)
            } else {
                Registration::Lose
            }
        }
    }
}

/// Remove the registration only if it still points at this session.
fn unregister_if_ours(inner: &NetControlInner, id: &MachineId, seq: u64) -> bool {
    let mut peers = inner.peers.write();
    match peers.get(id) {
        Some(slot) if slot.seq == seq => {
            inner.diagnostics.write().entry(id.clone()).or_default().traffic = slot.traffic.snapshot();
            peers.remove(id);
            true
        }
        _ => false,
    }
}

/// Build a PeerSlot for `peer` and insert it, applying the dedupe rule.
fn register(
    inner: &Arc<NetControlInner>,
    self_id: &MachineId,
    peer: &MachineId,
    role: Role,
    input: crate::input_transport::Connection,
) -> (Registration, u64, SessionCommands, Arc<Liveness>) {
    let (frames, frame_rx) = mpsc::channel(128);
    let (bulk, bulk_rx) = mpsc::channel(4);
    let (files, files_rx) = mpsc::channel(16);
    let (shutdown, shutdown_rx) = watch::channel(None);
    let traffic = Arc::new(Traffic::default());
    let seq = inner.next_seq.fetch_add(1, Ordering::Relaxed);
    let input = crate::input_transport::desktop::Control::spawn(input, peer.clone(), seq, inner.events.clone(), shutdown.clone(), traffic.clone());
    let cmd_rx = SessionCommands { input: input.clone(), frames: OutgoingFrames { priority: frame_rx, bulk: bulk_rx, files: files_rx }, shutdown: shutdown_rx, traffic: traffic.clone() };
    let active = Arc::new(Liveness::default());
    let slot = PeerSlot {
        seq,
        traffic: traffic.clone(),
        control: SessionControl { input, frames, bulk, files, shutdown, traffic },
        rule_following: matches!(role, Role::Dialer) == (self_id < peer),
        active: active.clone(),
    };
    let reg = try_register(inner, peer, slot);
    (reg, seq, cmd_rx, active)
}

/// Run one connection to completion. Returns true iff the handshake completed and the
/// peer was Connected (the dialer uses this to reset reconnect backoff).
pub(crate) async fn run(
    inner: Arc<NetControlInner>,
    mut sock: TcpStream,
    role: Role,
    expected: Option<MachineId>,
    admission: Option<OwnedSemaphorePermit>,
) -> bool {
    let _ = sock.set_nodelay(true);
    let peer_addr = sock
        .peer_addr()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
    let self_info = inner.self_info.read().clone();
    let deadline = tokio::time::Instant::now() + inner.opts.handshake_timeout;

    match role {
        Role::Dialer => {
            if let Some(id) = &expected { inner.phase(id, ConnectionPhase::SendingHello, Some(peer_addr), None); }
            let hello = Frame::Hello(Hello {
                proto_min: inner.opts.proto_min,
                proto_max: inner.opts.proto_max,
                machine: self_info.clone(),
                caps: our_caps(&inner),
            });
            if let Err(error) = write_frame(&inner, &mut sock, &hello).await {
                if let Some(id) = &expected { reject(&inner, id, format!("Sending Hello failed: {error}")); }
                return false;
            }
            if let Some(id) = &expected { inner.phase(id, ConnectionPhase::AwaitingWelcome, None, None); }
            let frame = match tokio::time::timeout_at(deadline, read_frame(&mut sock)).await {
                Ok(Ok(f)) => f,
                error => {
                    if let Some(id) = &expected { reject(&inner, id, format!("Waiting for Welcome failed: {error:?}; check the Tailnet route and VPN split-tunnel rules")); }
                    return false;
                }
            };
            let welcome = match frame {
                Frame::Welcome(w) => w,
                Frame::Bye { reason } => {
                    if let Some(peer) = &expected {
                        reject(&inner, peer, reason);
                    }
                    return false;
                }
                _ => return false,
            };
            if welcome.proto != inner.opts.proto_max || !supports_required_capabilities(&welcome.caps) {
                reject(
                    &inner,
                    &welcome.machine.id,
                    "peer does not implement the current Splice protocol; update every client".into(),
                );
                return false;
            }
            if welcome.machine.id == self_info.id {
                return false;
            }
            if let Some(exp) = &expected {
                if welcome.machine.id != *exp {
                    return false;
                }
            }
            let peer = welcome.machine.id.clone();
            if write_frame(&inner, &mut sock, &Frame::Ready).await.is_err() {
                reject(&inner, &peer, "cannot confirm the peer handshake".into());
                return false;
            }
            let input = match input_handshake(&inner, &mut sock, peer_addr, deadline).await {
                Ok(input) => input,
                Err(error) => { reject(&inner, &peer, format!("UDP input handshake failed: {error}")); return false; }
            };
            let (reg, seq, cmd_rx, active) = register(&inner, &self_info.id, &peer, role, input);
            match reg {
                Registration::Lose => {
                    let _ = write_frame(&inner, &mut sock, &Frame::Bye { reason: "dup".into() }).await;
                    return false;
                }
                Registration::Fresh => {}
                Registration::Replaced(old) => {
                    old.close("duplicate connection replaced");
                }
            }
            inner.phase(&peer, ConnectionPhase::Connected, Some(peer_addr), None);
            inner.diagnostics.write().entry(peer.clone()).or_default().build = Some(welcome.machine.build.clone());
            let _ = inner.events.send(PeerEvent::Connected {
                id: peer.clone(),
                hello: welcome.machine,
                caps: welcome.caps,
                addr: peer_addr,
            });
            drop(admission);
            session_loop(inner, sock, peer, cmd_rx, active, seq).await
        }
        Role::Listener => {
            if let Some(id) = &expected { inner.phase(id, ConnectionPhase::AwaitingHello, Some(peer_addr), None); }
            let frame = match tokio::time::timeout_at(deadline, read_frame(&mut sock)).await {
                Ok(Ok(f)) => f,
                error => {
                    if let Some(id) = &expected { reject(&inner, id, format!("Could not read a protocol {} Hello: {error:?}; update every client to the same release", splice_proto::PROTO_VERSION)); }
                    return false;
                }
            };
            let hello = match frame {
                Frame::Hello(h) => h,
                _ => return false,
            };
            if hello.machine.id == self_info.id {
                return false;
            }
            // Claimed identity must match the transport identity (WhoIs) when known.
            if let Some(exp) = &expected {
                if hello.machine.id != *exp {
                    return false;
                }
            }
            let proto = inner.opts.proto_max;
            if hello.proto_min != proto || hello.proto_max != proto || !supports_required_capabilities(&hello.caps) {
                let reason =
                    format!("Splice protocol {proto} and all current capabilities are required; update every client");
                reject(&inner, &hello.machine.id, reason.clone());
                let _ = write_frame(&inner, &mut sock, &Frame::Bye { reason }).await;
                return false;
            }
            let caps = our_caps(&inner);
            let peer = hello.machine.id.clone();
            let welcome = Frame::Welcome(Welcome { proto, machine: self_info.clone(), caps: caps.clone() });
            if write_frame(&inner, &mut sock, &welcome).await.is_err() {
                reject(&inner, &peer, "cannot send handshake response".into());
                return false;
            }
            inner.phase(&peer, ConnectionPhase::AwaitingReady, None, None);
            if !matches!(tokio::time::timeout_at(deadline, read_frame(&mut sock)).await, Ok(Ok(Frame::Ready))) {
                reject(&inner, &peer, "peer did not confirm the handshake; check Tailnet connectivity".into());
                return false;
            }
            let input = match input_handshake(&inner, &mut sock, peer_addr, deadline).await {
                Ok(input) => input,
                Err(error) => { reject(&inner, &peer, format!("UDP input handshake failed: {error}")); return false; }
            };
            let (reg, seq, cmd_rx, active) = register(&inner, &self_info.id, &peer, role, input);
            match reg {
                Registration::Lose => {
                    let _ = write_frame(&inner, &mut sock, &Frame::Bye { reason: "dup".into() }).await;
                    return false;
                }
                Registration::Fresh => {}
                Registration::Replaced(old) => {
                    old.close("duplicate connection replaced");
                }
            }
            inner.phase(&peer, ConnectionPhase::Connected, Some(peer_addr), None);
            inner.diagnostics.write().entry(peer.clone()).or_default().build = Some(hello.machine.build.clone());
            let _ = inner.events.send(PeerEvent::Connected {
                id: peer.clone(),
                hello: hello.machine,
                caps: hello.caps,
                addr: peer_addr,
            });
            drop(admission);
            session_loop(inner, sock, peer, cmd_rx, active, seq).await
        }
    }
}

async fn input_handshake(
    inner: &NetControlInner,
    sock: &mut TcpStream,
    peer: SocketAddr,
    deadline: tokio::time::Instant,
) -> anyhow::Result<crate::input_transport::Connection> {
    tokio::time::timeout_at(deadline, async {
        let local = crate::input_transport::token()?;
        write_frame(inner, sock, &Frame::InputOffer { port: inner.input_endpoint.address()?.port(), token: local }).await?;
        let Frame::InputOffer { port, token } = read_frame(sock).await? else { anyhow::bail!("peer did not offer UDP input"); };
        anyhow::ensure!(port != 0 && token != [0; 16], "invalid UDP input authorization");
        let mut input = inner.input_endpoint.subscribe(SocketAddr::new(peer.ip(), port), local, token)?;
        input.probe().await?;
        Ok(input)
    }).await.map_err(|_| anyhow::anyhow!("UDP input handshake expired"))?
}

fn cadence(inner: &NetControlInner, active: &Liveness) -> Duration {
    if active.enabled.load(Ordering::Relaxed) {
        inner.opts.active_hb
    } else {
        inner.opts.idle_hb
    }
}

fn heartbeat_wake(
    inner: &NetControlInner,
    active: &Liveness,
    next_ping: Instant,
    outstanding: &Option<(u64, u64, Instant)>,
    degraded_since: Option<Instant>,
) -> Instant {
    let cad = cadence(inner, active);
    let mut wake = next_ping;
    if let Some((_, _, sent)) = outstanding {
        wake = wake.min(*sent + cad.mul_f64(MISS_WINDOW));
    }
    if let Some(since) = degraded_since {
        wake = wake.min(since + inner.opts.degraded_timeout);
    }
    wake
}

/// Frame pump + heartbeat until the socket, the reader, or NetControl ends the session.
/// Emits Disconnected only when this session is still the registered one, so a
/// connection displaced by the dedupe rule never produces a spurious event.
async fn session_loop(
    inner: Arc<NetControlInner>,
    sock: TcpStream,
    peer: MachineId,
    mut cmd_rx: SessionCommands,
    active: Arc<Liveness>,
    seq: u64,
) -> bool {
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::with_capacity(16 * 1024, rd);
    let (frame_tx, mut frame_rx) = mpsc::channel::<Result<Frame, ProtoError>>(64);
    let reader = tokio::spawn(async move {
        let mut read_buf = Vec::with_capacity(256);
        loop {
            let result = tokio::select! {
                _ = frame_tx.closed() => return,
                result = read_frame_buffered(&mut rd, &mut read_buf) => result,
            };
            match result {
                Ok(f) => {
                    if frame_tx.send(Ok(f)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = frame_tx.send(Err(e)).await;
                    return;
                }
            }
        }
    });

    // t_us is local monotonic micros; only the Ping sender ever interprets the echo.
    let epoch = Instant::now();
    let mut nonce: u64 = 0;
    let mut outstanding: Option<(u64, u64, Instant)> = None;
    let mut misses: u32 = 0;
    let mut degraded = false;
    let mut degraded_since: Option<Instant> = None;
    let mut last_rtt_emit: Option<Instant> = None;
    let mut next_ping = Instant::now() + cadence(&inner, &active);
    let mut write_buf = Vec::with_capacity(256);
    let event_peer = Arc::new(peer.clone());
    let mut pending_leave: Option<futures::future::BoxFuture<'static, (QueuedFrame, bool)>> = None;

    let reason: String = loop {
        tokio::select! {
            _ = inner.events.closed() => break "engine stopped".to_string(),
            _ = active.changed.notified() => {
                next_ping = Instant::now() + cadence(&inner, &active);
            }
            changed = cmd_rx.shutdown.changed() => {
                let reason = cmd_rx.shutdown.borrow().clone();
                if let Some(reason) = reason {
                    let _ = write_with_timeout(inner.opts.write_timeout, &mut wr, &Frame::Bye { reason: reason.clone() }, &mut write_buf).await;
                    break reason;
                }
                if changed.is_err() {
                    break "session control closed".to_string();
                }
            }
            completed = async { pending_leave.as_mut().expect("departure is pending").await }, if pending_leave.is_some() => {
                let (queued, delivered) = completed;
                pending_leave = None;
                if !delivered { break "UDP input did not finish before session departure".into(); }
                let queue_time = queued.queued.elapsed();
                let started = Instant::now();
                if let Err(error) = write_with_timeout(inner.opts.write_timeout, &mut wr, &queued.frame, &mut write_buf).await {
                    break format!("write: {error}");
                }
                cmd_rx.traffic.sent(false, write_buf.len(), queue_time, started.elapsed());
            }
            frame = cmd_rx.frames.recv(), if pending_leave.is_none() => match frame {
                Some(queued) => {
                    if let Frame::Leave { session, .. } = queued.frame {
                        let input = cmd_rx.input.clone();
                        pending_leave = Some(Box::pin(async move { let delivered = input.finish(session).await; (queued, delivered) }));
                        continue;
                    }
                    let queue_time = queued.queued.elapsed();
                    let started = Instant::now();
                    if let Err(error) = write_with_timeout(inner.opts.write_timeout, &mut wr, &queued.frame, &mut write_buf).await {
                        break format!("write: {error}");
                    }
                    cmd_rx.traffic.sent(matches!(queued.frame, Frame::Input { .. }), write_buf.len(), queue_time, started.elapsed());
                }
                None => break "control channel closed".to_string(),
            },
            frame = frame_rx.recv() => match frame {
                Some(Ok(Frame::Ping { nonce: n, t_us })) => {
                    if inner.opts.answer_pings.load(Ordering::Relaxed) {
                        let pong = Frame::Pong { nonce: n, t_us };
                        if let Err(e) =
                            write_with_timeout(inner.opts.write_timeout, &mut wr, &pong, &mut write_buf).await
                        {
                            break format!("write: {e}");
                        }
                    }
                }
                Some(Ok(Frame::Pong { nonce: n, t_us })) => {
                    let rtt = match outstanding {
                        Some((on, sent_us, sent)) if on == n && sent_us == t_us => {
                            outstanding = None;
                            sent.elapsed().as_secs_f64() * 1000.0
                        }
                        _ => continue,
                    };
                    {
                        let mut diagnostics = inner.diagnostics.write();
                        let entry = diagnostics.entry(peer.clone()).or_default();
                        entry.last_heartbeat_ms = Some(unix_ms());
                        if entry.phase != ConnectionPhase::Connected { entry.phase_changed_ms = unix_ms(); }
                        entry.phase = ConnectionPhase::Connected;
                    }
                    misses = 0;
                    degraded_since = None;
                    if degraded {
                        degraded = false;
                        let _ = inner
                            .events
                            .send(PeerEvent::Healthy(peer.clone(), rtt));
                    }
                    let due = last_rtt_emit
                        .map(|t| t.elapsed() >= RTT_EMIT_MIN)
                        .unwrap_or(true);
                    if due {
                        last_rtt_emit = Some(Instant::now());
                        let _ = inner.events.send(PeerEvent::Rtt(peer.clone(), rtt));
                    }
                }
                Some(Ok(Frame::Bye { reason })) => break reason,
                Some(Ok(Frame::Hello(_) | Frame::Welcome(_) | Frame::Ready | Frame::InputOffer { .. })) => break "unexpected handshake frame".into(),
                Some(Ok(Frame::Input { .. })) => break "input requires the authenticated UDP channel".into(),
                Some(Ok(Frame::Files(message))) => {
                    let peers = inner.peers.read();
                    if peers.get(&peer).is_some_and(|slot| slot.seq == seq) {
                        let incoming = inner.file_incoming.read();
                        if let Some(tx) = incoming.as_ref() {
                            let _ = tx.try_send((peer.clone(), seq, message));
                        }
                    }
                }
                Some(Ok(f)) => {
                    let peers = inner.peers.read();
                    if peers.get(&peer).is_some_and(|slot| slot.seq == seq) {
                        let _ = inner.events.send(PeerEvent::Frame(event_peer.clone(), f));
                    }
                }
                Some(Err(e)) => break format!("read: {e}"),
                None => break "reader stopped".to_string(),
            },
            _ = tokio::time::sleep_until(heartbeat_wake(&inner, &active, next_ping, &outstanding, degraded_since).into()) => {
                let now = Instant::now();
                let cad = cadence(&inner, &active);
                let expired = outstanding
                    .as_ref()
                    .is_some_and(|(_, _, sent)| now >= *sent + cad.mul_f64(MISS_WINDOW));
                if expired {
                    misses += 1;
                    outstanding = None;
                    if misses >= inner.opts.max_misses && !degraded {
                        { let mut entries = inner.diagnostics.write(); let entry = entries.entry(peer.clone()).or_default(); entry.phase = ConnectionPhase::Degraded; entry.phase_changed_ms = unix_ms(); }
                        degraded = true;
                        degraded_since = Some(now);
                        let _ = inner.events.send(PeerEvent::Degraded(peer.clone()));
                    }
                }
                if degraded_since
                    .is_some_and(|since| now >= since + inner.opts.degraded_timeout)
                {
                    break "heartbeat timeout".to_string();
                }
                if next_ping <= now || expired {
                    if outstanding.is_none() {
                        nonce += 1;
                        let t_us = epoch.elapsed().as_micros() as u64;
                        let ping = Frame::Ping { nonce, t_us };
                        if let Err(e) =
                            write_with_timeout(inner.opts.write_timeout, &mut wr, &ping, &mut write_buf).await
                        {
                            break format!("write: {e}");
                        }
                        outstanding = Some((nonce, t_us, Instant::now()));
                    }
                    next_ping = Instant::now() + cad;
                }
            }
        }
    };

    reader.abort();
    if unregister_if_ours(&inner, &peer, seq) {
        inner.diagnostics.write().entry(peer.clone()).or_default().disconnects += 1;
        inner.phase(&peer, ConnectionPhase::Disconnected, None, Some(reason.clone()));
        let _ = inner.events.send(PeerEvent::Disconnected(peer, reason));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn controls() -> (SessionControl, OutgoingFrames, watch::Receiver<Option<String>>, mpsc::UnboundedReceiver<PeerEvent>, crate::input_transport::desktop::Control) {
        use crate::input_transport::{Endpoint, desktop::Control};
        let left = Endpoint::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let right = Endpoint::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let a = left.subscribe(right.address().unwrap(), [1; 16], [2; 16]).unwrap();
        let b = right.subscribe(left.address().unwrap(), [2; 16], [1; 16]).unwrap();
        let (frames, priority) = mpsc::channel(2);
        let (bulk, bulk_rx) = mpsc::channel(2);
        let (files, files_rx) = mpsc::channel(2);
        let (shutdown, reason) = watch::channel(None);
        let (events, received) = mpsc::unbounded_channel();
        let traffic = Arc::new(Traffic::default());
        let input = Control::spawn(a, MachineId("b".into()), 1, events.clone(), shutdown.clone(), traffic.clone());
        let destination = Control::spawn(b, MachineId("a".into()), 1, events, shutdown.clone(), Arc::new(Traffic::default()));
        let control = SessionControl { input, frames, bulk, files, shutdown, traffic };
        (control, OutgoingFrames { priority, bulk: bulk_rx, files: files_rx }, reason, received, destination)
    }

    #[tokio::test]
    async fn file_control_overflow_preserves_priority_control_session() {
        let (control, mut frames, reason, _events, _destination) = controls().await;
        let frame = Frame::Files(splice_proto::files::FileMessage::Cancel { transfer: splice_proto::files::TransferId([1; 16]) });
        assert!(control.send(frame.clone()));
        assert!(control.send(frame.clone()));
        assert!(!control.send(frame));
        assert!(reason.borrow().is_none());
        assert!(control.send(Frame::ReleaseAll));
        assert!(matches!(frames.recv().await.unwrap().frame, Frame::ReleaseAll));
        assert!(reason.borrow().is_none());
    }

    #[tokio::test]
    async fn clipboard_backpressure_cannot_refuse_file_control() {
        let (control, mut frames, reason, _events, _destination) = controls().await;
        for request in [1, 2] {
            assert!(control.send_wait(Frame::ClipChunk { request, data: vec![7; splice_proto::CLIP_CHUNK], last: true }, Duration::from_secs(1)).await);
        }
        let transfer = splice_proto::files::TransferId([8; 16]);
        assert!(control.send(Frame::Files(splice_proto::files::FileMessage::Cancel { transfer })));
        assert!(matches!(frames.recv().await.unwrap().frame, Frame::Files(splice_proto::files::FileMessage::Cancel { transfer: got }) if got == transfer));
        assert!(reason.borrow().is_none());
    }

    #[tokio::test]
    async fn clipboard_backpressure_cannot_delay_udp_clicks_or_fractional_motion() {
        let (control, mut tcp, reason, mut events, destination) = controls().await;
        for request in [1, 2] {
            assert!(control.send_wait(Frame::ClipChunk { request, data: vec![7; splice_proto::CLIP_CHUNK], last: true }, Duration::from_secs(1)).await);
        }
        assert!(control.send(Frame::Enter { session: 1, pos: splice_proto::Vec2 { x: 40.0, y: 40.0 } }));
        assert!(matches!(tcp.recv().await.unwrap().frame, Frame::Enter { .. }));
        destination.allow(Some(1));
        let expected = vec![
            splice_proto::InputEvent::Key { code: 42, pressed: true },
            splice_proto::InputEvent::Motion { dx: 0.5, dy: 1.25 },
            splice_proto::InputEvent::Key { code: 42, pressed: false },
        ];
        for ev in &expected { assert!(control.send(Frame::Input { session: 1, ev: *ev })); }
        let received = tokio::time::timeout(Duration::from_millis(500), async {
            let mut received = Vec::new();
            while received.len() < expected.len() {
                match events.recv().await.unwrap() {
                    PeerEvent::Input { events, .. } => received.extend(events),
                    other => panic!("unexpected event: {other:?}"),
                }
            }
            received
        }).await.unwrap();
        assert_eq!(received, expected);
        assert!(control.input.finish(1).await);
        for request in [1, 2] { assert!(matches!(tcp.recv().await.unwrap().frame, Frame::ClipChunk { request: actual, .. } if actual == request)); }
        assert!(reason.borrow().is_none());
        assert!(tcp.priority.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_blocked_writer_has_a_deadline() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let result = write_with_timeout(
            Duration::from_millis(20),
            &mut writer,
            &Frame::Ping { nonce: 1, t_us: 1 },
            &mut Vec::new(),
        )
        .await;
        assert!(matches!(result, Err(ProtoError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
    }

    #[tokio::test]
    async fn rejected_desktop_session_abandons_its_journal_without_disconnect() {
        let (control, _tcp, reason, mut events, _destination) = controls().await;
        assert!(control.input.start(1));
        assert!(control.input.push(1, splice_proto::InputEvent::Key { code: 42, pressed: true }));
        let finishing = control.input.clone();
        let finished = tokio::spawn(async move { finishing.finish(1).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(control.input.abandon(1));
        assert!(tokio::time::timeout(Duration::from_millis(200), finished).await.unwrap().unwrap());
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(reason.borrow().is_none());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_finished_source_keeps_the_target_live_until_the_control_leave_arrives() {
        let (control, _tcp, reason, mut events, destination) = controls().await;
        destination.allow(Some(1));
        assert!(control.input.start(1));
        assert!(control.input.finish(1).await);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(reason.borrow().is_none());
        assert!(events.try_recv().is_err());
        destination.allow(None);
    }

    #[tokio::test]
    async fn control_queue_overflow_closes_the_session_explicitly() {
        let (control, _receiver, reason, _events, _destination) = controls().await;
        assert!(control.send(Frame::Panic));
        assert!(control.send(Frame::Panic));
        assert!(!control.send(Frame::Panic));
        assert_eq!(reason.borrow().as_deref(), Some("outgoing queue exceeded its limit"));
    }
}
