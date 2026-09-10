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
    fetch: Arc<dyn ClipFetch>,
}

pub struct WaylandClipboard {
    conn: zbus::Connection,
    session_rx: watch::Receiver<Option<ClipSession>>,
    observed_rx: watch::Receiver<Option<ClipSession>>,
    offer: Arc<Mutex<Option<OfferState>>>,
    publication: tokio::sync::Mutex<()>,
}

#[async_trait::async_trait]
impl Clipboard for WaylandClipboard {
    async fn set_remote_offer(&self, offer: ClipboardOffer, fetch: Arc<dyn ClipFetch>) -> Result<()> {
        let _publication = self.publication.lock().await;
        let session = available_session(self.session_rx.borrow().clone(), &fetch)?;
        if let Err(error) = wait_for_session(&self.session_rx, &self.observed_rx, &session).await {
            if let Some(guard) = fetch.publication_guard().filter(|guard| !guard.is_published()) {
                guard.cancel();
            }
            return Err(error);
        }
        let proxy = portal::proxy(&self.conn, IFACE).await?;
        let path = portal::object_path(&session.path)?;
        let mut opts = Options::new();
        opts.insert("mime_types", Value::new(zbus::zvariant::Array::from(offer.mimes)));
        publish_selection(&self.offer, fetch, async {
            current_session(&self.session_rx, &session)?;
            let result = proxy.call::<_, _, ()>("SetSelection", &(path, opts)).await.map_err(portal::err_ctx("SetSelection"));
            current_session(&self.session_rx, &session)?;
            result
        }).await
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

    let (observed_tx, observed_rx) = watch::channel(None);
    let observer = tokio::spawn(observe(shared, conn.clone(), session_rx.clone(), observed_tx, offer.clone()));
    let server = tokio::spawn(serve_transfers(conn.clone(), session_rx.clone(), offer.clone()));
    let stop = Stop::new({
        let observer = observer.abort_handle();
        let server = server.abort_handle();
        let offer = offer.clone();
        move || {
            observer.abort();
            server.abort();
            *offer.lock() = None;
            crate::native_clipboard_clock().invalidate();
        }
    });

    (Arc::new(WaylandClipboard { conn, session_rx, observed_rx, offer, publication: tokio::sync::Mutex::new(()) }), stop)
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

fn available_session(session: Option<ClipSession>, fetch: &Arc<dyn ClipFetch>) -> Result<ClipSession> {
    if let Some(session) = session.filter(|s| s.enabled) {
        return Ok(session);
    }
    if let Some(guard) = fetch.publication_guard() {
        if !guard.is_published() {
            guard.cancel();
        }
    }
    Err(PlatformError::Unavailable("no clipboard session; retry after the session is restored".into()))
}

fn current_session(session_rx: &watch::Receiver<Option<ClipSession>>, session: &ClipSession) -> Result<()> {
    let current = session_rx.borrow();
    if current.as_ref().is_none_or(|current| !current.enabled || current.path != session.path) {
        return Err(PlatformError::Unavailable("clipboard session changed during publication".into()));
    }
    Ok(())
}

async fn wait_for_session(
    session_rx: &watch::Receiver<Option<ClipSession>>,
    observed_rx: &watch::Receiver<Option<ClipSession>>,
    session: &ClipSession,
) -> Result<()> {
    let mut current = session_rx.clone();
    let mut observed = observed_rx.clone();
    loop {
        if current.has_changed().is_err() || observed.has_changed().is_err() {
            return Err(PlatformError::Unavailable("clipboard observer stopped".into()));
        }
        current_session(&current, session)?;
        if current_session(&observed, session).is_ok() {
            return Ok(());
        }
        let changed = tokio::select! {
            changed = current.changed() => changed,
            changed = observed.changed() => changed,
        };
        changed.map_err(|_| PlatformError::Unavailable("clipboard observer stopped".into()))?;
    }
}

async fn publish_selection(
    offer: &Mutex<Option<OfferState>>,
    fetch: Arc<dyn ClipFetch>,
    publish: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    let guard = fetch.publication_guard();
    if let Some(guard) = &guard {
        guard.check()?;
    }
    *offer.lock() = Some(OfferState { fetch: fetch.clone() });
    let result = publish.await;
    let cancelled = guard.as_ref().is_some_and(|guard| guard.is_cancelled());
    if result.is_err() || cancelled {
        let mut current = offer.lock();
        if current.as_ref().is_some_and(|state| Arc::ptr_eq(&state.fetch, &fetch)) {
            *current = None;
        }
    } else if let Some(guard) = guard {
        guard.published();
    }
    if cancelled {
        return Err(PlatformError::Unavailable("clipboard publication was cancelled".into()));
    }
    result
}

async fn selection_read(conn: &zbus::Connection, session_path: &str, mime: &str) -> Result<zvariant::OwnedFd> {
    let proxy = portal::proxy(conn, IFACE).await?;
    let fd: zvariant::OwnedFd = proxy
        .call("SelectionRead", &(portal::object_path(session_path)?, mime))
        .await
        .map_err(portal::err_ctx("SelectionRead"))?;
    Ok(fd)
}

async fn observe(
    shared: Arc<Shared>,
    conn: zbus::Connection,
    mut session_rx: watch::Receiver<Option<ClipSession>>,
    observed_tx: watch::Sender<Option<ClipSession>>,
    offer: Arc<Mutex<Option<OfferState>>>,
) {
    let mut reads = tokio::task::JoinSet::new();
    loop {
        let session = loop {
            if let Some(s) = session_rx.borrow().clone().filter(|s| s.enabled) {
                break s;
            }
            if session_rx.changed().await.is_err() {
                return;
            }
        };
        let proxy = match portal::proxy(&conn, IFACE).await {
            Ok(p) => p,
            Err(_) => return,
        };
        let mut changes = match proxy.receive_signal("SelectionOwnerChanged").await {
            Ok(s) => s,
            Err(_) => return,
        };
        observed_tx.send_replace(Some(session.clone()));
        loop {
            tokio::select! {
                changed = session_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if current_session(&session_rx, &session).is_ok() {
                        continue;
                    }
                    observed_tx.send_replace(None);
                    reads.abort_all();
                    *offer.lock() = None;
                    crate::native_clipboard_clock().invalidate();
                    break;
                }
                _ = reads.join_next(), if !reads.is_empty() => {}
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
                        reads.abort_all();
                        continue;
                    }
                    reads.abort_all();
                    let clock = crate::native_clipboard_clock();
                    let generation = clock.invalidate();
                    let mimes = normalize_mimes(
                        &portal::get::<Vec<String>>(&opts, "mime_types").unwrap_or_default(),
                    );
                    let shared = shared.clone();
                    let conn = conn.clone();
                    let path = session.path.clone();
                    reads.spawn(async move {
                        let inline_text = if mimes.iter().any(|m| m == "text/plain;charset=utf-8") {
                            read_inline_text(&conn, &path).await
                        } else {
                            None
                        };
                        if !clock.changed(generation, mimes.clone(), inline_text.clone()) {
                            shared.emit(PlatformEvent::ClipboardChanged { mimes, inline_text });
                        }
                    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClipboardClock, ClipboardGuard};

    struct Fetch(ClipboardGuard);

    #[async_trait::async_trait]
    impl ClipFetch for Fetch {
        fn publication_guard(&self) -> Option<ClipboardGuard> {
            Some(self.0.clone())
        }

        async fn fetch(&self, _mime: &str) -> Option<Vec<u8>> {
            if self.0.is_cancelled() { None } else { Some(b"offered".to_vec()) }
        }
    }

    fn provider() -> (Arc<ClipboardClock>, ClipboardGuard, Arc<dyn ClipFetch>) {
        let clock = Arc::new(ClipboardClock::default());
        let guard = ClipboardGuard::new(clock.clone(), clock.current(), None);
        let fetch: Arc<dyn ClipFetch> = Arc::new(Fetch(guard.clone()));
        (clock, guard, fetch)
    }

    #[tokio::test]
    async fn foreign_owner_before_set_selection_reply_preserves_the_submitted_provider() {
        let (clock, guard, fetch) = provider();
        let offer = Arc::new(Mutex::new(None));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let task = {
            let offer = offer.clone();
            let entered = entered.clone();
            let release = release.clone();
            tokio::spawn(async move {
                publish_selection(&offer, fetch, async {
                    entered.notify_one();
                    release.notified().await;
                    Ok(())
                }).await
            })
        };
        entered.notified().await;
        clock.invalidate();
        release.notify_one();
        task.await.unwrap().unwrap();
        assert!(guard.is_published());
        let retained = offer.lock().as_ref().unwrap().fetch.clone();
        assert_eq!(retained.fetch("text/plain").await.unwrap(), b"offered");
        clock.invalidate();
        assert_eq!(retained.fetch("text/plain").await.unwrap(), b"offered");
    }

    #[tokio::test]
    async fn successful_native_reply_never_revives_a_cancelled_provider() {
        let (_, guard, fetch) = provider();
        let offer = Mutex::new(None);
        assert!(publish_selection(&offer, fetch, async {
            guard.cancel();
            Ok(())
        }).await.is_err());
        assert!(offer.lock().is_none());
        assert!(guard.is_cancelled());
        assert!(!guard.is_published());
    }

    #[tokio::test]
    async fn cancelled_cached_offer_cannot_reach_set_selection_after_regrant() {
        let (_, guard, fetch) = provider();
        assert!(available_session(None, &fetch).is_err());
        assert!(guard.is_cancelled());
        assert!(!guard.is_published());
        let restored = Some(ClipSession { path: "/session/new".into(), enabled: true });
        assert!(available_session(restored, &fetch).is_ok());
        let offer = Mutex::new(None);
        let called = std::sync::atomic::AtomicBool::new(false);
        assert!(publish_selection(&offer, fetch, async {
            called.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        }).await.is_err());
        assert!(!called.load(std::sync::atomic::Ordering::Acquire));
        assert!(offer.lock().is_none());
    }

    #[tokio::test]
    async fn no_session_does_not_cancel_already_published_ordinary_bytes() {
        let (clock, guard, fetch) = provider();
        guard.published();
        clock.invalidate();
        assert!(available_session(None, &fetch).is_err());
        assert!(!guard.is_cancelled());
        assert_eq!(fetch.fetch("text/plain").await.unwrap(), b"offered");
    }

    #[tokio::test]
    async fn session_replacement_during_set_selection_cannot_ack_the_old_offer() {
        let (_, guard, fetch) = provider();
        let session = ClipSession { path: "/session/old".into(), enabled: true };
        let (tx, rx) = watch::channel(Some(session.clone()));
        let offer = Mutex::new(None);
        assert!(publish_selection(&offer, fetch, async {
            current_session(&rx, &session)?;
            tx.send_replace(Some(ClipSession { path: "/session/new".into(), enabled: true }));
            current_session(&rx, &session)
        }).await.is_err());
        assert!(!guard.is_published());
        assert!(offer.lock().is_none());
    }

    #[tokio::test]
    async fn publication_waits_for_observation_of_the_current_session() {
        use futures::FutureExt;
        let session = ClipSession { path: "/session/new".into(), enabled: true };
        let (_session_tx, session_rx) = watch::channel(Some(session.clone()));
        let (observed_tx, observed_rx) = watch::channel(Some(ClipSession { path: "/session/old".into(), enabled: true }));
        let ready = wait_for_session(&session_rx, &observed_rx, &session);
        tokio::pin!(ready);
        assert!(ready.as_mut().now_or_never().is_none());
        observed_tx.send_replace(Some(session.clone()));
        ready.await.unwrap();
        drop(observed_tx);
        assert!(wait_for_session(&session_rx, &observed_rx, &session).await.is_err());
    }

    #[tokio::test]
    async fn losing_session_while_waiting_for_observation_fails() {
        use futures::FutureExt;
        let session = ClipSession { path: "/session/current".into(), enabled: true };
        let (session_tx, session_rx) = watch::channel(Some(session.clone()));
        let (_observed_tx, observed_rx) = watch::channel(None);
        let ready = wait_for_session(&session_rx, &observed_rx, &session);
        tokio::pin!(ready);
        assert!(ready.as_mut().now_or_never().is_none());
        session_tx.send_replace(None);
        assert!(ready.await.is_err());
    }

}
