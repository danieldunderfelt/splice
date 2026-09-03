//! org.freedesktop.ScreenSaver keep-awake + lock detection, shared by both injection
//! backends. GNOME exposes the interface at /org/freedesktop/ScreenSaver, KDE at
//! /ScreenSaver; the first working path wins.
//!
//! Inhibit/UnInhibit are reconciled against a desired-state flag under one async
//! mutex, so an enter immediately followed by a leave cannot leave a cookie behind.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::Mutex;

use super::Shared;

const LOCKED_NOTE: &str = "screen locked: remote input paused";

pub struct ScreenSaver {
    conn: Option<zbus::Connection>,
    locked: AtomicBool,
    wanted: AtomicBool,
    cookie: Mutex<Option<u32>>,
}

impl ScreenSaver {
    pub fn new(conn: Option<zbus::Connection>) -> Self {
        Self {
            conn,
            locked: AtomicBool::new(false),
            wanted: AtomicBool::new(false),
            cookie: Mutex::new(None),
        }
    }

    async fn proxy(&self) -> Option<zbus::Proxy<'static>> {
        let conn = self.conn.as_ref()?;
        for path in ["/org/freedesktop/ScreenSaver", "/ScreenSaver"] {
            let Ok(proxy) = zbus::Proxy::new(
                conn,
                "org.freedesktop.ScreenSaver",
                path,
                "org.freedesktop.ScreenSaver",
            )
            .await
            else {
                continue;
            };
            if proxy.call::<_, _, bool>("GetActive", &()).await.is_ok() {
                return Some(proxy);
            }
        }
        None
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    pub async fn inhibit(&self) {
        self.wanted.store(true, Ordering::Release);
        self.reconcile().await;
    }

    pub async fn uninhibit(&self) {
        self.wanted.store(false, Ordering::Release);
        self.reconcile().await;
    }

    /// Brings the D-Bus inhibitor in line with `wanted`; loops because the flag can
    /// flip while a round trip is in flight.
    async fn reconcile(&self) {
        let mut cookie = self.cookie.lock().await;
        loop {
            let wanted = self.wanted.load(Ordering::Acquire);
            match (*cookie, wanted) {
                (None, true) => {
                    let Some(proxy) = self.proxy().await else { return };
                    match proxy
                        .call::<_, _, u32>("Inhibit", &("splice", "Remote input active"))
                        .await
                    {
                        Ok(c) => *cookie = Some(c),
                        Err(err) => {
                            tracing::warn!(error = %err, "ScreenSaver.Inhibit failed");
                            return;
                        }
                    }
                }
                (Some(c), false) => {
                    let Some(proxy) = self.proxy().await else {
                        *cookie = None;
                        return;
                    };
                    if let Err(err) = proxy.call::<_, _, ()>("UnInhibit", &(c,)).await {
                        tracing::warn!(error = %err, "ScreenSaver.UnInhibit failed");
                    }
                    *cookie = None;
                }
                _ => return,
            }
        }
    }

    /// Tracks GetActive + ActiveChanged. `note_health` drives the "screen locked"
    /// health note for the portal backend (which cannot inject while locked); the note
    /// is only ever cleared when it is the current message, so real errors survive.
    pub async fn monitor(self: Arc<Self>, shared: Arc<Shared>, note_health: Arc<AtomicBool>) {
        let Some(proxy) = self.proxy().await else { return };
        let active = proxy.call::<_, _, bool>("GetActive", &()).await.unwrap_or(false);
        self.locked.store(active, Ordering::Relaxed);
        self.note(&shared, &note_health, active);
        let mut changes = match proxy.receive_signal("ActiveChanged").await {
            Ok(s) => s,
            Err(_) => return,
        };
        while let Some(msg) = changes.next().await {
            let Ok((active,)) = msg.body().deserialize::<(bool,)>() else { continue };
            self.locked.store(active, Ordering::Relaxed);
            self.note(&shared, &note_health, active);
        }
    }

    fn note(&self, shared: &Shared, note_health: &AtomicBool, active: bool) {
        if !note_health.load(Ordering::Acquire) {
            return;
        }
        shared.set_health(|h| {
            if active {
                if h.emulate.is_none() {
                    h.emulate = Some(LOCKED_NOTE.to_string());
                }
            } else if h.emulate.as_deref() == Some(LOCKED_NOTE) {
                h.emulate = None;
            }
        });
    }
}
