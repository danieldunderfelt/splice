//! Linux window side: IPC client to `splice service` (ipc.rs). Mirrors the service's
//! snapshots into the UI state and forwards commands. A lost connection reads as
//! "engine offline" with the usual retry, and Retry also starts the service if needed.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use parking_lot::{Mutex, RwLock};
use splice_core::UiState;

use crate::ipc::{self, ClientMessage, ServerMessage};
use crate::runtime::{BootStatus, RETRY_INTERVAL};

const NO_TRAY_HINT: &str = "No system tray on this desktop: launch Splice again to reopen this \
                            window. GNOME can show a tray icon with the AppIndicator extension.";

pub struct Remote {
    writer: Mutex<Option<UnixStream>>,
    retry: mpsc::Sender<()>,
}

impl Remote {
    pub fn send(&self, message: ClientMessage) {
        let mut writer = self.writer.lock();
        match writer.as_mut() {
            Some(stream) => {
                if let Err(err) = ipc::write_message(stream, &message) {
                    tracing::warn!(error = %err, "service connection lost");
                    *writer = None;
                }
            }
            None => tracing::debug!(?message, "service offline; dropping message"),
        }
    }

    pub fn retry(&self) {
        let _ = self.retry.send(());
    }
}

pub struct Mirror {
    pub state: Arc<RwLock<UiState>>,
    pub status: Arc<Mutex<BootStatus>>,
    pub tray_hint: Arc<Mutex<Option<String>>>,
    pub focus_request: Arc<AtomicBool>,
    pub quit_request: Arc<AtomicBool>,
}

pub fn start(ctx: egui::Context, mirror: Mirror) -> Arc<Remote> {
    let (retry_tx, retry_rx) = mpsc::channel();
    let remote = Arc::new(Remote { writer: Mutex::new(None), retry: retry_tx });
    let worker = remote.clone();
    let spawned = std::thread::Builder::new()
        .name("splice-ipc".into())
        .spawn(move || run(worker, ctx, mirror, retry_rx));
    if let Err(err) = spawned {
        tracing::error!(error = %err, "cannot start service connection thread");
    }
    remote
}

fn run(remote: Arc<Remote>, ctx: egui::Context, mirror: Mirror, retry_rx: mpsc::Receiver<()>) {
    loop {
        let stream = match ipc::ensure_service() {
            Ok(stream) => stream,
            Err(err) => {
                *mirror.status.lock() = BootStatus::Offline(format!("cannot start the Splice service: {err}"));
                ctx.request_repaint();
                let _ = retry_rx.recv_timeout(RETRY_INTERVAL);
                continue;
            }
        };
        let hello = stream
            .try_clone()
            .and_then(|mut writer| ipc::write_message(&mut writer, &ClientMessage::Hello { window: true }).map(|()| writer));
        match hello {
            Ok(writer) => *remote.writer.lock() = Some(writer),
            Err(err) => {
                *mirror.status.lock() = BootStatus::Offline(format!("cannot talk to the Splice service: {err}"));
                ctx.request_repaint();
                let _ = retry_rx.recv_timeout(RETRY_INTERVAL);
                continue;
            }
        }
        let mut reader = BufReader::new(stream);
        loop {
            match ipc::read_message::<ServerMessage>(&mut reader) {
                Ok(Some(ServerMessage::Snapshot { status, tray, state })) => {
                    *mirror.state.write() = state;
                    *mirror.status.lock() = status;
                    *mirror.tray_hint.lock() = (!tray).then(|| NO_TRAY_HINT.into());
                }
                Ok(Some(ServerMessage::Focus)) => mirror.focus_request.store(true, Ordering::Release),
                Ok(Some(ServerMessage::Quit)) => mirror.quit_request.store(true, Ordering::Release),
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "service connection failed");
                    break;
                }
            }
            ctx.request_repaint();
        }
        *remote.writer.lock() = None;
        *mirror.status.lock() = BootStatus::Offline("Splice service stopped".into());
        ctx.request_repaint();
        let _ = retry_rx.recv_timeout(RETRY_INTERVAL);
    }
}
