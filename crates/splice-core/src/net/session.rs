//! Per-peer session: handshake (Hello/Welcome), dedupe, framing, heartbeats.
//!
//! A session task owns its socket exclusively. A dedicated reader task pumps frames
//! into an mpsc so the main loop can select over reads, engine commands and the
//! heartbeat timer without cancel-safety hazards (framing::read_frame is only
//! cancel-safe at the length-prefix boundary).

use crate::net::{NetControlInner, PeerCmd, PeerEvent};
use splice_proto::framing::{read_frame, read_frame_buffered, write_frame, write_frame_buffered};
use splice_proto::{caps, Frame, Hello, MachineId, ProtoError, Welcome};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Capabilities every Splice peer advertises; the negotiated set is the intersection.
fn our_caps() -> Vec<String> {
    [caps::INPUT_V1, caps::CLIPBOARD_V1, caps::LAYOUT_V1]
        .iter()
        .map(|s| s.to_string())
        .collect()
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
    pub cmd: mpsc::UnboundedSender<PeerCmd>,
    /// True when this connection follows the smaller-id-dials rule from our side.
    pub rule_following: bool,
    /// Heartbeat cadence hint flipped by NetControl::set_active.
    pub active: Arc<AtomicBool>,
}

enum Registration {
    Fresh,
    /// We displaced a non-rule-following (or stale same-direction) connection.
    Replaced(mpsc::UnboundedSender<PeerCmd>),
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
                Registration::Replaced(e.insert(slot).cmd)
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
) -> (Registration, u64, mpsc::UnboundedReceiver<PeerCmd>, Arc<AtomicBool>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let active = Arc::new(AtomicBool::new(false));
    let seq = inner.next_seq.fetch_add(1, Ordering::Relaxed);
    let slot = PeerSlot {
        seq,
        cmd: cmd_tx,
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
    let hs_timeout = inner.opts.handshake_timeout;

    match role {
        Role::Dialer => {
            let hello = Frame::Hello(Hello {
                proto_min: inner.opts.proto_min,
                proto_max: inner.opts.proto_max,
                machine: self_info.clone(),
                caps: our_caps(),
            });
            if write_frame(&mut sock, &hello).await.is_err() {
                return false;
            }
            let frame = match tokio::time::timeout(hs_timeout, read_frame(&mut sock)).await {
                Ok(Ok(f)) => f,
                _ => return false,
            };
            // A Bye here is a refusal (dup / incompatible protocol); leave quietly.
            let welcome = match frame {
                Frame::Welcome(w) => w,
                _ => return false,
            };
            if welcome.proto < inner.opts.proto_min || welcome.proto > inner.opts.proto_max {
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
            let (reg, seq, cmd_rx, active) = register(&inner, &self_info.id, &peer, role);
            match reg {
                Registration::Lose => {
                    let _ = write_frame(&mut sock, &Frame::Bye { reason: "dup".into() }).await;
                    return false;
                }
                Registration::Fresh => {}
                Registration::Replaced(old) => {
                    let _ = old.send(PeerCmd::Shutdown("dup".into()));
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
            let frame = match tokio::time::timeout(hs_timeout, read_frame(&mut sock)).await {
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
            // proto = min(theirs_max, ours_max); refuse only on disjoint ranges.
            let proto = hello.proto_max.min(inner.opts.proto_max);
            if proto < hello.proto_min.max(inner.opts.proto_min) {
                let _ = write_frame(
                    &mut sock,
                    &Frame::Bye {
                        reason: format!(
                            "incompatible protocol ({}..={} vs {}..={})",
                            hello.proto_min,
                            hello.proto_max,
                            inner.opts.proto_min,
                            inner.opts.proto_max
                        ),
                    },
                )
                .await;
                return false;
            }
            let caps: Vec<String> = our_caps()
                .into_iter()
                .filter(|c| hello.caps.iter().any(|c2| c2 == c))
                .collect();
            let peer = hello.machine.id.clone();
            // Dedupe BEFORE sending Welcome so a rejected duplicate never looks Connected.
            let (reg, seq, cmd_rx, active) = register(&inner, &self_info.id, &peer, role);
            match reg {
                Registration::Lose => {
                    let _ = write_frame(&mut sock, &Frame::Bye { reason: "dup".into() }).await;
                    return false;
                }
                Registration::Fresh => {}
                Registration::Replaced(old) => {
                    let _ = old.send(PeerCmd::Shutdown("dup".into()));
                }
            }
            let welcome = Frame::Welcome(Welcome {
                proto,
                machine: self_info.clone(),
                caps: caps.clone(),
            });
            if write_frame(&mut sock, &welcome).await.is_err() {
                unregister_if_ours(&inner, &peer, seq);
                return false;
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

fn cadence(inner: &NetControlInner, active: &AtomicBool) -> Duration {
    if active.load(Ordering::Relaxed) {
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
    mut cmd_rx: mpsc::UnboundedReceiver<PeerCmd>,
    active: Arc<AtomicBool>,
    seq: u64,
) -> bool {
    let (mut rd, mut wr) = sock.into_split();
    let (frame_tx, mut frame_rx) = mpsc::channel::<Result<Frame, ProtoError>>(64);
    let reader = tokio::spawn(async move {
        let mut read_buf = Vec::with_capacity(256);
        loop {
            match read_frame_buffered(&mut rd, &mut read_buf).await {
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
    let mut last_rtt_emit: Option<Instant> = None;
    let mut next_ping = Instant::now() + cadence(&inner, &active);
    let mut write_buf = Vec::with_capacity(256);

    let reason: String = loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(PeerCmd::Send(f)) => {
                    if let Err(e) = write_frame_buffered(&mut wr, &f, &mut write_buf).await {
                        break format!("write: {e}");
                    }
                }
                Some(PeerCmd::Shutdown(r)) => {
                    let _ = write_frame_buffered(
                        &mut wr,
                        &Frame::Bye { reason: r.clone() },
                        &mut write_buf,
                    )
                    .await;
                    break r;
                }
                None => break "control channel closed".to_string(),
            },
            frame = frame_rx.recv() => match frame {
                Some(Ok(Frame::Ping { nonce: n, t_us })) => {
                    if inner.opts.answer_pings.load(Ordering::Relaxed) {
                        let pong = Frame::Pong { nonce: n, t_us };
                        if let Err(e) =
                            write_frame_buffered(&mut wr, &pong, &mut write_buf).await
                        {
                            break format!("write: {e}");
                        }
                    }
                }
                Some(Ok(Frame::Pong { nonce: n, t_us })) => {
                    let rtt = match outstanding {
                        Some((on, _, _)) if on == n => {
                            outstanding = None;
                            let now_us = epoch.elapsed().as_micros() as u64;
                            Some(now_us.saturating_sub(t_us) as f64 / 1000.0)
                        }
                        _ => None,
                    };
                    // Any Pong is liveness, even one for a stale nonce.
                    misses = 0;
                    if degraded {
                        degraded = false;
                        let _ = inner
                            .events
                            .send(PeerEvent::Healthy(peer.clone(), rtt.unwrap_or(0.0)));
                    }
                    if let Some(rtt) = rtt {
                        let due = last_rtt_emit
                            .map(|t| t.elapsed() >= RTT_EMIT_MIN)
                            .unwrap_or(true);
                        if due {
                            last_rtt_emit = Some(Instant::now());
                            let _ = inner.events.send(PeerEvent::Rtt(peer.clone(), rtt));
                        }
                    }
                }
                Some(Ok(Frame::Bye { reason })) => break reason,
                // Hello/Welcome outside the handshake are protocol noise; drop them.
                Some(Ok(Frame::Hello(_) | Frame::Welcome(_))) => {}
                Some(Ok(f)) => {
                    let _ = inner.events.send(PeerEvent::Frame(peer.clone(), f));
                }
                Some(Err(e)) => break format!("read: {e}"),
                None => break "reader stopped".to_string(),
            },
            _ = tokio::time::sleep_until(next_ping.into()) => {
                let cad = cadence(&inner, &active);
                if let Some((_, _, sent)) = &outstanding {
                    if sent.elapsed() > cad.mul_f64(MISS_WINDOW) {
                        // Heartbeat loss never closes the socket: mark, keep pinging.
                        misses += 1;
                        outstanding = None;
                        if misses >= inner.opts.max_misses && !degraded {
                            degraded = true;
                            let _ = inner.events.send(PeerEvent::Degraded(peer.clone()));
                        }
                    }
                }
                if outstanding.is_none() {
                    nonce += 1;
                    let t_us = epoch.elapsed().as_micros() as u64;
                    let ping = Frame::Ping { nonce, t_us };
                    if let Err(e) =
                        write_frame_buffered(&mut wr, &ping, &mut write_buf).await
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
