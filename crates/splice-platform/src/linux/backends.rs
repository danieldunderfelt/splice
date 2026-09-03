//! Backend supervisor: resolves the engine's preferences against what the session
//! offers, runs one implementation per concern, and hot-swaps them when preferences
//! change. The engine only ever sees the three switch objects below, which replay
//! state (armed edges, entered position, remote clipboard offer) into a new
//! implementation so a swap is invisible to the focus state machine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use splice_proto::{InputEvent, Vec2};
use tokio::sync::watch;

use super::clipboard::ClipSession;
use super::probe::{self, Availability};
use super::tokens::TokenStore;
use super::{capture, clipboard, datacontrol, emulate, overlay, uinput};
use super::{PanicRelease, Shared, Stop};
use crate::{
    BackendPrefs, BackendStatus, Capture, CaptureEvent, CapturePref, ClipFetch, Clipboard,
    ClipboardOffer, EdgeSpec, Emulate, InjectPref, PlatformError, PlatformEvent, Result,
};

/// After a driven session ends, injected motion can still hit our own edge strips;
/// the overlay ignores edge entries for this long (mirrors the engine's own grace).
const DRIVEN_GRACE: Duration = Duration::from_secs(1);
/// Availability is re-probed on this cadence so installing the udev rule or a portal
/// backend is picked up without restarting the service.
const REPROBE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureKind {
    Portal,
    Overlay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InjectKind {
    Portal,
    Uinput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipKind {
    Portal,
    DataControl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    capture: Option<CaptureKind>,
    inject: Option<InjectKind>,
    clipboard: Option<ClipKind>,
}

fn resolve(prefs: BackendPrefs, avail: &Availability) -> Plan {
    let capture = match prefs.capture {
        CapturePref::Overlay if avail.overlay => Some(CaptureKind::Overlay),
        CapturePref::Portal if avail.portal_capture => Some(CaptureKind::Portal),
        _ if avail.overlay => Some(CaptureKind::Overlay),
        _ if avail.portal_capture => Some(CaptureKind::Portal),
        _ => None,
    };
    let inject = match prefs.inject {
        InjectPref::Uinput if avail.uinput => Some(InjectKind::Uinput),
        InjectPref::Portal if avail.portal_inject => Some(InjectKind::Portal),
        _ if avail.uinput => Some(InjectKind::Uinput),
        _ if avail.portal_inject => Some(InjectKind::Portal),
        _ => None,
    };
    let clipboard = if avail.data_control {
        Some(ClipKind::DataControl)
    } else if avail.portal_inject {
        Some(ClipKind::Portal)
    } else {
        None
    };
    Plan { capture, inject, clipboard }
}

fn status(prefs: BackendPrefs, avail: &Availability, plan: &Plan, inject_note: Option<&str>) -> BackendStatus {
    let capture = match plan.capture {
        Some(CaptureKind::Overlay) => "Wayland overlay (cursor hidden while away)".to_string(),
        Some(CaptureKind::Portal) => "Input Capture portal".to_string(),
        None => "unavailable: this session has neither the InputCapture portal nor layer-shell \
                 support"
            .to_string(),
    };
    let capture = match prefs.capture {
        CapturePref::Overlay if !avail.overlay => {
            format!("{capture}; the Wayland overlay is not supported by this compositor")
        }
        CapturePref::Portal if !avail.portal_capture => {
            format!("{capture}; no InputCapture portal backend is installed")
        }
        _ => capture,
    };
    let inject = match plan.inject {
        Some(InjectKind::Uinput) => "virtual device (uinput)".to_string(),
        Some(InjectKind::Portal) => "Remote Desktop portal".to_string(),
        None => "unavailable: /dev/uinput is not accessible and there is no RemoteDesktop \
                 portal"
            .to_string(),
    };
    let inject = match prefs.inject {
        InjectPref::Uinput if !avail.uinput => {
            format!("{inject}; /dev/uinput is not accessible")
        }
        InjectPref::Portal if !avail.portal_inject => {
            format!("{inject}; no RemoteDesktop portal backend is installed")
        }
        _ => inject,
    };
    let inject = match inject_note {
        Some(note) => format!("{inject}; {note}"),
        None => inject,
    };
    let inject = if plan.inject == Some(InjectKind::Uinput) && cosmic_multi_output(avail) {
        format!("{inject}; COSMIC maps absolute pointers to one output, so remote input reaches only the active monitor")
    } else {
        inject
    };
    let clipboard = match plan.clipboard {
        Some(ClipKind::DataControl) => "Wayland data-control".to_string(),
        Some(ClipKind::Portal) => "Clipboard portal on the Remote Desktop session".to_string(),
        None => "unavailable: no data-control protocol and no RemoteDesktop portal".to_string(),
    };
    BackendStatus {
        prefs,
        capture,
        inject,
        clipboard,
        overlay_available: avail.overlay,
        uinput_available: avail.uinput,
        portal_capture_available: avail.portal_capture,
        portal_inject_available: avail.portal_inject,
    }
}

fn cosmic_multi_output(avail: &Availability) -> bool {
    avail.outputs > 1
        && std::env::var("XDG_CURRENT_DESKTOP")
            .map(|d| d.to_ascii_uppercase().contains("COSMIC"))
            .unwrap_or(false)
}

/// Whether a remote machine is currently driving this one (or just stopped). The
/// overlay consults this so injected motion reaching our own screen edge is not
/// mistaken for a local crossing.
#[derive(Default)]
pub struct Driven {
    active: AtomicBool,
    grace_until: Mutex<Option<Instant>>,
}

impl Driven {
    pub fn suppressed(&self) -> bool {
        if self.active.load(Ordering::Acquire) {
            return true;
        }
        self.grace_until.lock().is_some_and(|until| Instant::now() < until)
    }

    fn set(&self, active: bool) {
        let was = self.active.swap(active, Ordering::AcqRel);
        if was && !active {
            *self.grace_until.lock() = Some(Instant::now() + DRIVEN_GRACE);
        }
    }
}

struct SwitchCapture {
    inner: RwLock<Option<(Arc<dyn Capture>, PanicRelease)>>,
    edges: Mutex<(u64, Vec<EdgeSpec>)>,
}

impl SwitchCapture {
    fn current(&self) -> Option<Arc<dyn Capture>> {
        self.inner.read().as_ref().map(|(c, _)| c.clone())
    }

    fn panic(&self) {
        if let Some((_, panic)) = self.inner.read().as_ref() {
            panic.trigger();
        }
    }
}

#[async_trait::async_trait]
impl Capture for SwitchCapture {
    async fn set_edges(&self, edges: Vec<EdgeSpec>) -> Result<()> {
        {
            let mut cached = self.edges.lock();
            cached.0 += 1;
            cached.1 = edges.clone();
        }
        match self.current() {
            Some(inner) => inner.set_edges(edges).await,
            None => Ok(()),
        }
    }

    async fn begin_capture(&self) -> Result<()> {
        match self.current() {
            Some(inner) => inner.begin_capture().await,
            None => Err(PlatformError::Unavailable("no capture backend".into())),
        }
    }

    async fn end_capture(&self, warp_to: Option<Vec2>) -> Result<()> {
        match self.current() {
            Some(inner) => inner.end_capture(warp_to).await,
            None => Ok(()),
        }
    }
}

/// Session state and backend identity change together under one async lock, so a
/// swap cannot interleave with an in-flight enter or leave.
struct SwitchEmulate {
    inner: RwLock<Option<Arc<dyn Emulate>>>,
    session: tokio::sync::Mutex<Option<Vec2>>,
    driven: Arc<Driven>,
}

impl SwitchEmulate {
    fn current(&self) -> Option<Arc<dyn Emulate>> {
        self.inner.read().clone()
    }
}

#[async_trait::async_trait]
impl Emulate for SwitchEmulate {
    async fn enter(&self, pos: Vec2) -> Result<()> {
        let mut session = self.session.lock().await;
        let Some(inner) = self.current() else {
            return Err(PlatformError::Unavailable("no injection backend".into()));
        };
        inner.enter(pos).await?;
        *session = Some(pos);
        self.driven.set(true);
        Ok(())
    }

    async fn inject(&self, ev: InputEvent) -> Result<()> {
        let Some(inner) = self.current() else {
            return Err(PlatformError::Unavailable("no injection backend".into()));
        };
        inner.inject(ev).await
    }

    async fn leave(&self) -> Result<()> {
        let mut session = self.session.lock().await;
        *session = None;
        self.driven.set(false);
        match self.current() {
            Some(inner) => inner.leave().await,
            None => Ok(()),
        }
    }

    async fn release_all(&self) -> Result<()> {
        match self.current() {
            Some(inner) => inner.release_all().await,
            None => Ok(()),
        }
    }
}

type CachedOffer = (u64, Option<(ClipboardOffer, Arc<dyn ClipFetch>)>);

struct SwitchClipboard {
    inner: RwLock<Option<Arc<dyn Clipboard>>>,
    offer: Mutex<CachedOffer>,
}

impl SwitchClipboard {
    fn current(&self) -> Option<Arc<dyn Clipboard>> {
        self.inner.read().clone()
    }
}

#[async_trait::async_trait]
impl Clipboard for SwitchClipboard {
    async fn set_remote_offer(&self, offer: ClipboardOffer, fetch: Arc<dyn ClipFetch>) -> Result<()> {
        {
            let mut cached = self.offer.lock();
            cached.0 += 1;
            cached.1 = Some((offer.clone(), fetch.clone()));
        }
        match self.current() {
            Some(inner) => inner.set_remote_offer(offer, fetch).await,
            None => Ok(()),
        }
    }

    async fn read_local(&self, mime: &str) -> Result<Vec<u8>> {
        match self.current() {
            Some(inner) => inner.read_local(mime).await,
            None => Err(PlatformError::Unavailable("no clipboard backend".into())),
        }
    }
}

pub struct Handles {
    pub capture: Arc<dyn Capture>,
    pub emulate: Arc<dyn Emulate>,
    pub clipboard: Arc<dyn Clipboard>,
    pub panic: PanicRelease,
    pub capture_unavailable: bool,
    pub inject_unavailable: bool,
}

struct Running {
    capture: Option<(CaptureKind, Stop)>,
    portal_rd: Option<(Arc<emulate::WaylandEmulate>, watch::Receiver<Option<ClipSession>>, Stop)>,
    /// Whether the RemoteDesktop session is the active injector (it may exist only
    /// for the Clipboard portal), which decides who owns the `emulate` health field.
    portal_injects: Arc<AtomicBool>,
    uinput: Option<(Arc<dyn Emulate>, Stop)>,
    inject: Option<InjectKind>,
    /// Why a requested injection backend could not start, shown next to the fallback.
    inject_note: Option<String>,
    clipboard: Option<(ClipKind, Stop)>,
}

impl Running {
    fn stop_all(&mut self) {
        if let Some((_, stop)) = self.capture.take() {
            stop.stop();
        }
        if let Some((_, stop)) = self.clipboard.take() {
            stop.stop();
        }
        if let Some((_, stop)) = self.uinput.take() {
            stop.stop();
        }
        if let Some((_, _, stop)) = self.portal_rd.take() {
            stop.stop();
        }
    }
}

struct Supervisor {
    shared: Arc<Shared>,
    tokens: Arc<TokenStore>,
    conn: Option<zbus::Connection>,
    panic_chord: Vec<u32>,
    capture: Arc<SwitchCapture>,
    emulate: Arc<SwitchEmulate>,
    clipboard: Arc<SwitchClipboard>,
    driven: Arc<Driven>,
    running: Running,
}

impl Supervisor {
    async fn apply(&mut self, prefs: BackendPrefs, avail: &Availability) -> Plan {
        let mut plan = resolve(prefs, avail);
        self.apply_capture(plan.capture).await;
        self.apply_inject(&mut plan, avail).await;
        self.apply_clipboard(&mut plan).await;
        let status = status(prefs, avail, &plan, self.running.inject_note.as_deref());
        tracing::info!(?plan, "linux backends resolved");
        self.shared.emit(PlatformEvent::Backends(status));
        plan
    }

    async fn apply_capture(&mut self, wanted: Option<CaptureKind>) {
        if self.running.capture.as_ref().map(|(k, _)| *k) == wanted {
            return;
        }
        if let Some((kind, stop)) = self.running.capture.take() {
            tracing::info!(?kind, "stopping capture backend");
            *self.capture.inner.write() = None;
            stop.stop();
            self.shared.set_health(|h| h.capture = None);
            self.shared.emit(PlatformEvent::Capture(CaptureEvent::Broken {
                reason: "capture backend swapped".into(),
            }));
        }
        let Some(kind) = wanted else { return };
        let created: Result<(Arc<dyn Capture>, PanicRelease, Stop)> = match kind {
            CaptureKind::Portal => match &self.conn {
                Some(conn) => {
                    let (c, p, s) = capture::create(
                        self.shared.clone(),
                        self.tokens.clone(),
                        conn.clone(),
                        self.panic_chord.clone(),
                    );
                    Ok((c, p, s))
                }
                None => Err(PlatformError::Unavailable("no session bus".into())),
            },
            CaptureKind::Overlay => overlay::create(
                self.shared.clone(),
                self.panic_chord.clone(),
                self.driven.clone(),
            )
            .await
            .map(|(c, p, s)| (c as Arc<dyn Capture>, p, s)),
        };
        match created {
            Ok((backend, panic, stop)) => {
                loop {
                    let (generation, edges) = self.capture.edges.lock().clone();
                    if !edges.is_empty() {
                        let _ = backend.set_edges(edges).await;
                    }
                    let mut inner = self.capture.inner.write();
                    if self.capture.edges.lock().0 == generation {
                        *inner = Some((backend.clone(), panic.clone()));
                        break;
                    }
                }
                self.running.capture = Some((kind, stop));
                tracing::info!(?kind, "capture backend started");
            }
            Err(err) => {
                tracing::warn!(?kind, error = %err, "capture backend failed to start");
                self.shared.set_health(|h| h.capture = Some(format!("{err}")));
            }
        }
    }

    fn start_portal_rd(&mut self) {
        if self.running.portal_rd.is_some() {
            return;
        }
        if let Some(conn) = &self.conn {
            let (e, rx, stop) = emulate::create(
                self.shared.clone(),
                self.tokens.clone(),
                conn.clone(),
                self.running.portal_injects.clone(),
            );
            self.running.portal_rd = Some((e, rx, stop));
            tracing::info!("remote desktop portal session started");
        }
    }

    async fn apply_inject(&mut self, plan: &mut Plan, avail: &Availability) {
        if plan.inject == Some(InjectKind::Uinput) && self.running.uinput.is_none() {
            match uinput::create(self.shared.clone(), self.conn.clone()).await {
                Ok((e, stop)) => {
                    self.running.uinput = Some((e, stop));
                    self.running.inject_note = None;
                    tracing::info!("uinput injection started");
                }
                Err(err) => {
                    tracing::warn!(error = %err, "uinput injection failed to start");
                    self.running.inject_note = Some(format!("uinput failed to start: {err}"));
                    plan.inject = avail.portal_inject.then_some(InjectKind::Portal);
                }
            }
        } else if plan.inject != Some(InjectKind::Uinput) {
            self.running.inject_note = None;
        }
        if plan.inject != Some(InjectKind::Uinput) {
            if let Some((_, stop)) = self.running.uinput.take() {
                stop.stop();
            }
        }
        let needs_portal_rd = plan.inject == Some(InjectKind::Portal)
            || plan.clipboard == Some(ClipKind::Portal);
        self.running
            .portal_injects
            .store(plan.inject == Some(InjectKind::Portal), Ordering::Release);
        if needs_portal_rd {
            self.start_portal_rd();
        } else if let Some((_, _, stop)) = self.running.portal_rd.take() {
            stop.stop();
            tracing::info!("remote desktop portal session stopped");
        }
        if self.running.inject != plan.inject {
            let mut session = self.emulate.session.lock().await;
            if let Some(old) = self.emulate.current() {
                if session.is_some() {
                    let _ = old.leave().await;
                }
            }
            let new: Option<Arc<dyn Emulate>> = match plan.inject {
                Some(InjectKind::Uinput) => self.running.uinput.as_ref().map(|(e, _)| e.clone()),
                Some(InjectKind::Portal) => self
                    .running
                    .portal_rd
                    .as_ref()
                    .map(|(e, _, _)| e.clone() as Arc<dyn Emulate>),
                None => None,
            };
            *self.emulate.inner.write() = new.clone();
            self.running.inject = plan.inject;
            self.shared.set_health(|h| h.emulate = None);
            match (new, *session) {
                (Some(new), Some(pos)) => {
                    if let Err(err) = new.enter(pos).await {
                        tracing::warn!(error = %err, "re-entering after injection swap failed");
                        *session = None;
                        self.emulate.driven.set(false);
                    }
                }
                (None, Some(_)) => {
                    *session = None;
                    self.emulate.driven.set(false);
                }
                _ => {}
            }
        }
    }

    async fn apply_clipboard(&mut self, plan: &mut Plan) {
        if self.running.clipboard.as_ref().map(|(k, _)| *k) == plan.clipboard {
            return;
        }
        if let Some((_, stop)) = self.running.clipboard.take() {
            *self.clipboard.inner.write() = None;
            stop.stop();
            self.shared.set_health(|h| h.clipboard = None);
        }
        let created: Option<(Arc<dyn Clipboard>, Stop)> = match plan.clipboard {
            Some(ClipKind::DataControl) => match datacontrol::create(self.shared.clone()) {
                Ok((c, stop)) => Some((c, stop)),
                Err(err) => {
                    tracing::warn!(error = %err, "data-control clipboard failed to start");
                    self.shared.set_health(|h| h.clipboard = Some(format!("{err}")));
                    plan.clipboard = None;
                    None
                }
            },
            Some(ClipKind::Portal) => match (&self.conn, &self.running.portal_rd) {
                (Some(conn), Some((_, rx, _))) => {
                    let (c, stop) = clipboard::create(self.shared.clone(), conn.clone(), rx.clone());
                    Some((c as Arc<dyn Clipboard>, stop))
                }
                _ => {
                    plan.clipboard = None;
                    None
                }
            },
            None => None,
        };
        if let Some((backend, stop)) = created {
            loop {
                let (generation, pending) = self.clipboard.offer.lock().clone();
                if let Some((offer, fetch)) = pending {
                    let _ = backend.set_remote_offer(offer, fetch).await;
                }
                let mut inner = self.clipboard.inner.write();
                if self.clipboard.offer.lock().0 == generation {
                    *inner = Some(backend.clone());
                    break;
                }
            }
            if let Some(kind) = plan.clipboard {
                self.running.clipboard = Some((kind, stop));
            } else {
                stop.stop();
            }
        }
    }
}

pub async fn spawn(
    shared: Arc<Shared>,
    tokens: Arc<TokenStore>,
    conn: Option<zbus::Connection>,
    panic_chord: Vec<u32>,
    mut prefs_rx: watch::Receiver<BackendPrefs>,
) -> Handles {
    let driven = Arc::new(Driven::default());
    let capture = Arc::new(SwitchCapture { inner: RwLock::new(None), edges: Mutex::new((0, Vec::new())) });
    let emulate = Arc::new(SwitchEmulate {
        inner: RwLock::new(None),
        session: tokio::sync::Mutex::new(None),
        driven: driven.clone(),
    });
    let clipboard = Arc::new(SwitchClipboard { inner: RwLock::new(None), offer: Mutex::new((0, None)) });
    let mut supervisor = Supervisor {
        shared,
        tokens,
        conn,
        panic_chord,
        capture: capture.clone(),
        emulate: emulate.clone(),
        clipboard: clipboard.clone(),
        driven,
        running: Running {
            capture: None,
            portal_rd: None,
            portal_injects: Arc::new(AtomicBool::new(false)),
            uinput: None,
            inject: None,
            inject_note: None,
            clipboard: None,
        },
    };
    let avail = probe::run(supervisor.conn.as_ref()).await;
    tracing::info!(?avail, "linux session capabilities");
    let prefs = *prefs_rx.borrow_and_update();
    let plan = supervisor.apply(prefs, &avail).await;
    let panic = PanicRelease::new({
        let capture = capture.clone();
        move || capture.panic()
    });
    let handles = Handles {
        capture,
        emulate,
        clipboard,
        panic,
        capture_unavailable: plan.capture.is_none(),
        inject_unavailable: plan.inject.is_none(),
    };
    let mut last = (prefs, avail);
    let _ = tokio::spawn(async move {
        let mut reprobe = tokio::time::interval(REPROBE_INTERVAL);
        reprobe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                changed = prefs_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = reprobe.tick() => {}
            }
            let prefs = *prefs_rx.borrow_and_update();
            let avail = probe::run(supervisor.conn.as_ref()).await;
            if (prefs, avail.clone()) == last {
                continue;
            }
            last = (prefs, avail.clone());
            supervisor.apply(prefs, &avail).await;
        }
        supervisor.running.stop_all();
    });
    handles
}
