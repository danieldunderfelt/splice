//! Per-peer session: handshake (Hello/Welcome), dedupe, framing, heartbeats.
//!
//! A session task owns its socket exclusively. A dedicated reader task pumps frames
//! into an mpsc so the main loop can select over reads, engine commands and the
//! heartbeat timer without cancel-safety hazards (framing::read_frame is only
//! cancel-safe at the length-prefix boundary).

use crate::net::{NetControlInner, PeerEvent};
use splice_proto::framing::{read_frame, read_frame_buffered, write_frame_buffered};
use splice_proto::{caps, Frame, Hello, MachineId, ProtoError, Welcome};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWrite, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, Notify};

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
    frames: mpsc::Sender<Frame>,
    shutdown: watch::Sender<Option<String>>,
}

impl SessionControl {
    pub async fn send_wait(&self, frame: Frame, timeout: Duration) -> bool {
        matches!(tokio::time::timeout(timeout, self.frames.send(frame)).await, Ok(Ok(())))
    }

    pub fn send(&self, frame: Frame) -> bool {
        match self.frames.try_send(frame) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
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

struct SessionCommands {
    frames: mpsc::Receiver<Frame>,
    shutdown: watch::Receiver<Option<String>>,
}

fn our_caps() -> Vec<String> {
    [caps::INPUT_V1, caps::CLIPBOARD_V2, caps::LAYOUT_V1, caps::MASTER_V1].iter().map(|s| s.to_string()).collect()
}

fn reject(inner: &NetControlInner, peer: &MachineId, reason: String) {
    tracing::warn!(%peer, %reason, "peer connection rejected");
    let _ = inner.events.send(PeerEvent::Rejected { id: peer.clone(), reason });
}

fn supports_required_capabilities(caps: &[String]) -> bool {
    our_caps().iter().all(|required| caps.contains(required))
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
) -> (Registration, u64, SessionCommands, Arc<Liveness>) {
    let (frames, frame_rx) = mpsc::channel(128);
    let (shutdown, shutdown_rx) = watch::channel(None);
    let cmd_rx = SessionCommands { frames: frame_rx, shutdown: shutdown_rx };
    let active = Arc::new(Liveness::default());
    let seq = inner.next_seq.fetch_add(1, Ordering::Relaxed);
    let slot = PeerSlot {
        seq,
        control: SessionControl { frames, shutdown },
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
) -> bool {
    let _ = sock.set_nodelay(true);
    let peer_addr = sock
        .peer_addr()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
    let self_info = inner.self_info.read().clone();
    let deadline = tokio::time::Instant::now() + inner.opts.handshake_timeout;

    match role {
        Role::Dialer => {
            let hello = Frame::Hello(Hello {
                proto_min: inner.opts.proto_min,
                proto_max: inner.opts.proto_max,
                machine: self_info.clone(),
                caps: our_caps(),
            });
            if write_frame(&inner, &mut sock, &hello).await.is_err() {
                return false;
            }
            let frame = match tokio::time::timeout_at(deadline, read_frame(&mut sock)).await {
                Ok(Ok(f)) => f,
                _ => return false,
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
            let (reg, seq, cmd_rx, active) = register(&inner, &self_info.id, &peer, role);
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
            let _ = inner.events.send(PeerEvent::Connected {
                id: peer.clone(),
                hello: welcome.machine,
                caps: welcome.caps,
                addr: peer_addr,
            });
            session_loop(inner, sock, peer, cmd_rx, active, seq).await
        }
        Role::Listener => {
            let frame = match tokio::time::timeout_at(deadline, read_frame(&mut sock)).await {
                Ok(Ok(f)) => f,
                _ => return false,
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
            let caps = our_caps();
            let peer = hello.machine.id.clone();
            let welcome = Frame::Welcome(Welcome { proto, machine: self_info.clone(), caps: caps.clone() });
            if write_frame(&inner, &mut sock, &welcome).await.is_err() {
                reject(&inner, &peer, "cannot send handshake response".into());
                return false;
            }
            if !matches!(tokio::time::timeout_at(deadline, read_frame(&mut sock)).await, Ok(Ok(Frame::Ready))) {
                reject(&inner, &peer, "peer did not confirm the handshake; check Tailnet connectivity".into());
                return false;
            }
            let (reg, seq, cmd_rx, active) = register(&inner, &self_info.id, &peer, role);
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
            let _ = inner.events.send(PeerEvent::Connected {
                id: peer.clone(),
                hello: hello.machine,
                caps,
                addr: peer_addr,
            });
            session_loop(inner, sock, peer, cmd_rx, active, seq).await
        }
    }
}

fn cadence(inner: &NetControlInner, active: &Liveness) -> Duration {
    if active.enabled.load(Ordering::Relaxed) {
        inner.opts.active_hb
    } else {
        inner.opts.idle_hb
    }
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
            frame = cmd_rx.frames.recv() => match frame {
                Some(frame) => {
                    if let Err(error) = write_with_timeout(inner.opts.write_timeout, &mut wr, &frame, &mut write_buf).await {
                        break format!("write: {error}");
                    }
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
                Some(Ok(Frame::Hello(_) | Frame::Welcome(_) | Frame::Ready)) => break "unexpected handshake frame".into(),
                Some(Ok(f)) => {
                    let _ = inner.events.send(PeerEvent::Frame(event_peer.clone(), f));
                }
                Some(Err(e)) => break format!("read: {e}"),
                None => break "reader stopped".to_string(),
            },
            _ = tokio::time::sleep_until(next_ping.into()) => {
                let cad = cadence(&inner, &active);
                if let Some((_, _, sent)) = &outstanding {
                    if sent.elapsed() > cad.mul_f64(MISS_WINDOW) {
                        misses += 1;
                        outstanding = None;
                        if misses >= inner.opts.max_misses && !degraded {
                            degraded = true;
                            degraded_since = Some(Instant::now());
                            let _ = inner.events.send(PeerEvent::Degraded(peer.clone()));
                        }
                    }
                }
                if degraded_since
                    .is_some_and(|since| since.elapsed() >= inner.opts.degraded_timeout)
                {
                    break "heartbeat timeout".to_string();
                }
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
    };

    reader.abort();
    if unregister_if_ours(&inner, &peer, seq) {
        let _ = inner.events.send(PeerEvent::Disconnected(peer, reason));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn input_queue_overflow_closes_the_session_explicitly() {
        let (frames, _receiver) = mpsc::channel(2);
        let (shutdown, reason) = watch::channel(None);
        let control = SessionControl { frames, shutdown };
        assert!(control.send(Frame::Panic));
        assert!(control.send(Frame::Panic));
        assert!(!control.send(Frame::Panic));
        assert_eq!(reason.borrow().as_deref(), Some("outgoing queue exceeded its limit"));
    }
}
