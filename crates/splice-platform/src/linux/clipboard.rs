//! Clipboard portal attached to the RemoteDesktop session (GNOME 46+ / KDE 6.4+).
//!
//! The ashpd clipboard wrapper is feature-gated out of this workspace, so
//! org.freedesktop.portal.Clipboard is spoken directly over zbus. RequestClipboard is
//! issued by the RemoteDesktop session setup (emulate.rs) before Start; this module only
//! observes and serves an already-granted session.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use parking_lot::Mutex;
use splice_proto::{CLIP_INLINE_TEXT_MAX, CLIP_MAX_TOTAL};
use tokio::io::unix::AsyncFd;
use tokio::sync::watch;
use zbus::zvariant::{OwnedObjectPath, Value};

use super::portal::{self, Options};
use super::{Shared, Stop};
use crate::{ClipFetch, Clipboard, ClipboardOffer, PlatformError, PlatformEvent, Result};
use zbus::zvariant;

const IFACE: &str = "org.freedesktop.portal.Clipboard";
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Text mimes, in preference order for inline reads.
const TEXT_MIMES: &[&str] = &["text/plain;charset=utf-8", "text/plain"];
/// Aliases that mean "plain text" plus selection-manager noise that means nothing.
const TEXT_ALIASES: &[&str] = &["text/plain", "UTF8_STRING", "STRING", "TEXT"];
const NOISE_MIMES: &[&str] = &["TIMESTAMP", "TARGETS", "MULTIPLE", "SAVE_TARGETS"];

/// RemoteDesktop session the clipboard portal is attached to; `enabled` is the
/// `clipboard_enabled` grant from Start.
#[derive(Clone, Debug)]
pub struct ClipSession {
    pub path: String,
    pub enabled: bool,
}

struct OfferState {
    mimes: Vec<String>,
    fetch: Arc<dyn ClipFetch>,
}

pub struct WaylandClipboard {
    conn: zbus::Connection,
    session_rx: watch::Receiver<Option<ClipSession>>,
    offer: Arc<Mutex<Option<OfferState>>>,
}

#[async_trait::async_trait]
impl Clipboard for WaylandClipboard {
    async fn set_remote_offer(&self, offer: ClipboardOffer, fetch: Arc<dyn ClipFetch>) -> Result<()> {
        let mimes = offer.mimes.clone();
        *self.offer.lock() = Some(OfferState { mimes: mimes.clone(), fetch });
        // Bind first so the non-Send watch::Ref guard drops before the await.
        let session = self.session_rx.borrow().clone();
        if let Some(session) = session {
            if session.enabled {
                set_selection(&self.conn, &session.path, &mimes).await?;
            }
        }
        Ok(())
    }

    async fn read_local(&self, mime: &str) -> Result<Vec<u8>> {
        let session = self
            .session_rx
            .borrow()
            .clone()
            .filter(|s| s.enabled)
            .ok_or_else(|| PlatformError::Unavailable("no clipboard session".into()))?;
        // The engine speaks normalized mimes; the portal may only have the bare alias.
        let mut candidates = vec![mime.to_string()];
        if mime == "text/plain;charset=utf-8" {
            candidates.push("text/plain".into());
        }
        let mut last_err = None;
        for mime in candidates {
            match selection_read(&self.conn, &session.path, &mime).await {
                Ok(fd) => {
                    return read_fd(fd.into(), CLIP_MAX_TOTAL, READ_TIMEOUT)
                        .await
                        .map_err(|e| PlatformError::Other(e.into()));
                }
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or_else(|| PlatformError::Unavailable("clipboard read failed".into())))
    }
}

pub fn create(
    shared: Arc<Shared>,
    conn: zbus::Connection,
    session_rx: watch::Receiver<Option<ClipSession>>,
) -> (Arc<WaylandClipboard>, Stop) {
    let offer: Arc<Mutex<Option<OfferState>>> = Arc::new(Mutex::new(None));

    let observer = tokio::spawn(observe(shared, conn.clone(), session_rx.clone(), offer.clone()));
    let server = tokio::spawn(serve_transfers(conn.clone(), session_rx.clone(), offer.clone()));
    let stop = Stop::new({
        let observer = observer.abort_handle();
        let server = server.abort_handle();
        let offer = offer.clone();
        move || {
            observer.abort();
            server.abort();
            *offer.lock() = None;
        }
    });

    (Arc::new(WaylandClipboard { conn, session_rx, offer }), stop)
}

fn normalize_mimes(mimes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for mime in mimes {
        let normalized = if mime == "text/plain;charset=utf-8" || TEXT_ALIASES.contains(&mime.as_str()) {
            "text/plain;charset=utf-8"
        } else if NOISE_MIMES.contains(&mime.as_str()) {
            continue;
        } else {
            mime.as_str()
        };
        let normalized = normalized.to_string();
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out
}

async fn set_selection(conn: &zbus::Connection, session_path: &str, mimes: &[String]) -> Result<()> {
    let proxy = portal::proxy(conn, IFACE).await?;
    let mut opts = Options::new();
    opts.insert("mime_types", Value::new(zbus::zvariant::Array::from(mimes.to_vec())));
    proxy
        .call::<_, _, ()>("SetSelection", &(portal::object_path(session_path)?, opts))
        .await
        .map_err(portal::err_ctx("SetSelection"))?;
    Ok(())
}

async fn selection_read(conn: &zbus::Connection, session_path: &str, mime: &str) -> Result<zvariant::OwnedFd> {
    let proxy = portal::proxy(conn, IFACE).await?;
    let fd: zvariant::OwnedFd = proxy
        .call("SelectionRead", &(portal::object_path(session_path)?, mime))
        .await
        .map_err(portal::err_ctx("SelectionRead"))?;
    Ok(fd)
}

/// Observes SelectionOwnerChanged; on a real (non-self) change, reads small text inline
/// and republishes the offer. Re-applies a pending remote offer on session (re)grant.
async fn observe(
    shared: Arc<Shared>,
    conn: zbus::Connection,
    mut session_rx: watch::Receiver<Option<ClipSession>>,
    offer: Arc<Mutex<Option<OfferState>>>,
) {
    loop {
        let session = loop {
            if let Some(s) = session_rx.borrow().clone().filter(|s| s.enabled) {
                break s;
            }
            if session_rx.changed().await.is_err() {
                return;
            }
        };
        // Extract before the await: the parking_lot guard is not Send.
        let pending = offer.lock().as_ref().map(|state| state.mimes.clone());
        if let Some(mimes) = pending {
            if let Err(err) = set_selection(&conn, &session.path, &mimes).await {
                tracing::warn!(error = %err, "re-applying clipboard offer failed");
            }
        }

        let proxy = match portal::proxy(&conn, IFACE).await {
            Ok(p) => p,
            Err(_) => return,
        };
        let mut changes = match proxy.receive_signal("SelectionOwnerChanged").await {
            Ok(s) => s,
            Err(_) => return,
        };
        loop {
            tokio::select! {
                changed = session_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                msg = changes.next() => {
                    let Some(msg) = msg else { return };
                    let Some((path, opts)) = portal::session_signal(&msg)
                    else {
                        continue;
                    };
                    if path != session.path {
                        continue;
                    }
                    // Loop guard: our own SetSelection also fires this signal.
                    if portal::get::<bool>(&opts, "session_is_owner") == Some(true) {
                        continue;
                    }
                    let mimes = normalize_mimes(
                        &portal::get::<Vec<String>>(&opts, "mime_types").unwrap_or_default(),
                    );
                    let inline_text = if mimes.iter().any(|m| m == "text/plain;charset=utf-8") {
                        read_inline_text(&conn, &session.path).await
                    } else {
                        None
                    };
                    shared.emit(PlatformEvent::ClipboardChanged { mimes, inline_text });
                }
            }
        }
    }
}

async fn read_inline_text(conn: &zbus::Connection, session_path: &str) -> Option<String> {
    for mime in TEXT_MIMES {
        let Ok(fd) = selection_read(conn, session_path, mime).await else { continue };
        // One extra byte distinguishes "empty inline" from "too large to inline".
        match read_fd(fd.into(), CLIP_INLINE_TEXT_MAX + 1, READ_TIMEOUT).await {
            Ok(bytes) if bytes.len() <= CLIP_INLINE_TEXT_MAX => {
                if let Ok(text) = String::from_utf8(bytes) {
                    return Some(text);
                }
            }
            _ => {}
        }
    }
    None
}

/// Serves SelectionTransfer: a local app is pasting content we advertised for a peer.
async fn serve_transfers(
    conn: zbus::Connection,
    mut session_rx: watch::Receiver<Option<ClipSession>>,
    offer: Arc<Mutex<Option<OfferState>>>,
) {
    let proxy = match portal::proxy(&conn, IFACE).await {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut transfers = match proxy.receive_signal("SelectionTransfer").await {
        Ok(s) => s,
        Err(_) => return,
    };
    loop {
        tokio::select! {
            changed = session_rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            msg = transfers.next() => {
                let Some(msg) = msg else { return };
                let Ok((path, mime, serial)) =
                    msg.body().deserialize::<(OwnedObjectPath, String, u32)>()
                else {
                    continue;
                };
                let Some(session) = session_rx.borrow().clone().filter(|s| s.enabled) else {
                    continue;
                };
                if path.as_str() != session.path {
                    continue;
                }
                let fetch = offer.lock().as_ref().map(|o| o.fetch.clone());
                let conn = conn.clone();
                tokio::spawn(async move {
                    serve_transfer(&conn, &session.path, &mime, serial, fetch).await;
                });
            }
        }
    }
}

async fn serve_transfer(
    conn: &zbus::Connection,
    session_path: &str,
    mime: &str,
    serial: u32,
    fetch: Option<Arc<dyn ClipFetch>>,
) {
    let proxy = match portal::proxy(conn, IFACE).await {
        Ok(p) => p,
        Err(_) => return,
    };
    let done = |success: bool| {
        let proxy = proxy.clone();
        let path = session_path.to_string();
        async move {
            if let Ok(opath) = portal::object_path(&path) {
                let _ = proxy
                    .call::<_, _, ()>("SelectionWriteDone", &(opath, serial, success))
                    .await;
            }
        }
    };
    let Ok(opath) = portal::object_path(session_path) else { return };
    let fd: zvariant::OwnedFd = match proxy.call("SelectionWrite", &(opath, serial)).await
    {
        Ok(fd) => fd,
        Err(err) => {
            tracing::warn!(error = %err, "SelectionWrite failed");
            return;
        }
    };
    let data = match fetch {
        Some(fetch) => match tokio::time::timeout(FETCH_TIMEOUT, fetch.fetch(mime)).await {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(%error, "clipboard fetch timed out");
                None
            },
        },
        None => None,
    };
    match data {
        Some(bytes) => {
            let ok = write_fd(fd.into(), &bytes, READ_TIMEOUT).await.is_ok();
            done(ok).await;
        }
        None => {
            drop(fd);
            done(false).await;
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Drains a portal pipe fd, capped at `cap` bytes (the rest is discarded).
async fn read_fd(fd: OwnedFd, cap: usize, timeout: Duration) -> io::Result<Vec<u8>> {
    tokio::time::timeout(timeout, read_fd_inner(fd, cap))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "clipboard read timed out"))?
}

async fn read_fd_inner(fd: OwnedFd, cap: usize) -> io::Result<Vec<u8>> {
    set_nonblocking(&fd)?;
    let fd = AsyncFd::new(std::fs::File::from(fd))?;
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let mut guard = fd.readable().await?;
        match guard.try_io(|inner| {
            let n = unsafe {
                libc::read(inner.get_ref().as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let remaining = cap.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(remaining)]);
                if out.len() >= cap {
                    break;
                }
            }
            Ok(Err(err)) => return Err(err),
            Err(_would_block) => continue,
        }
    }
    Ok(out)
}

async fn write_fd(fd: OwnedFd, data: &[u8], timeout: Duration) -> io::Result<()> {
    tokio::time::timeout(timeout, write_fd_inner(fd, data))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "clipboard write timed out"))?
}

async fn write_fd_inner(fd: OwnedFd, data: &[u8]) -> io::Result<()> {
    set_nonblocking(&fd)?;
    let fd = AsyncFd::new(std::fs::File::from(fd))?;
    let mut written = 0;
    while written < data.len() {
        let mut guard = fd.writable().await?;
        match guard.try_io(|inner| {
            let n = unsafe {
                libc::write(
                    inner.get_ref().as_raw_fd(),
                    data[written..].as_ptr().cast(),
                    data.len() - written,
                )
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => written += n,
            Ok(Err(err)) => return Err(err),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}
