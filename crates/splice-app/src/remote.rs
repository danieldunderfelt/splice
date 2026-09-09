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
    run_with_connector(
        remote,
        ctx,
        mirror,
        retry_rx,
        RETRY_INTERVAL,
        |start_service| {
            if start_service {
                ipc::ensure_service()
            } else {
                ipc::connect()
            }
        },
    );
}

fn run_with_connector<C>(
    remote: Arc<Remote>,
    ctx: egui::Context,
    mirror: Mirror,
    retry_rx: mpsc::Receiver<()>,
    retry_interval: std::time::Duration,
    mut connect: C,
) where
    C: FnMut(bool) -> std::io::Result<UnixStream>,
{
    let mut start_service = true;
    loop {
        let may_start = std::mem::replace(&mut start_service, false);
        let stream = match connect(may_start) {
            Ok(stream) => stream,
            Err(err) => {
                let action = if may_start { "start" } else { "connect to" };
                *mirror.status.lock() = BootStatus::Offline(format!("cannot {action} the Splice service: {err}"));
                ctx.request_repaint();
                if retry_rx.recv_timeout(retry_interval).is_ok() {
                    start_service = true;
                }
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
                if retry_rx.recv_timeout(retry_interval).is_ok() {
                    start_service = true;
                }
                continue;
            }
        }
        let mut reader = BufReader::new(stream);
        loop {
            match ipc::read_message::<ServerMessage>(&mut reader) {
                Ok(Some(ServerMessage::Snapshot { status, tray, state })) => {
                    *mirror.state.write() = *state;
                    *mirror.status.lock() = status;
                    *mirror.tray_hint.lock() = (!tray).then(|| NO_TRAY_HINT.into());
                }
                Ok(Some(ServerMessage::Focus)) => mirror.focus_request.store(true, Ordering::Release),
                Ok(Some(ServerMessage::Quit)) => {
                    mirror.quit_request.store(true, Ordering::Release);
                    ctx.request_repaint();
                    *remote.writer.lock() = None;
                    return;
                }
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
        let _ = retry_rx.recv_timeout(retry_interval);
        start_service = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;

    fn mirror() -> Mirror {
        Mirror {
            state: Arc::new(RwLock::new(UiState::initial(splice_proto::MachineId("self".into())))),
            status: Arc::new(Mutex::new(BootStatus::Starting)),
            tray_hint: Arc::new(Mutex::new(None)),
            focus_request: Arc::new(AtomicBool::new(false)),
            quit_request: Arc::new(AtomicBool::new(false)),
        }
    }

    fn read_hello(stream: &UnixStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        assert!(matches!(
            ipc::read_message::<ClientMessage>(&mut reader).unwrap(),
            Some(ClientMessage::Hello { window: true })
        ));
    }

    #[test]
    fn reconnect_restarts_service_after_unexpected_disconnect() {
        let (client1, server1) = UnixStream::pair().unwrap();
        let (client2, mut server2) = UnixStream::pair().unwrap();
        let (retry_tx, retry_rx) = mpsc::channel();
        let modes = Arc::new(Mutex::new(Vec::new()));
        let observed_modes = modes.clone();
        let mut clients = VecDeque::from([client1, client2]);
        let server = std::thread::spawn(move || {
            read_hello(&server1);
            drop(server1);
            read_hello(&server2);
            ipc::write_message(&mut server2, &ServerMessage::Quit).unwrap();
        });
        let remote = Arc::new(Remote { writer: Mutex::new(None), retry: retry_tx });
        let mirror = mirror();
        let quit = mirror.quit_request.clone();
        run_with_connector(
            remote,
            egui::Context::default(),
            mirror,
            retry_rx,
            std::time::Duration::from_millis(10),
            move |start_service| {
                observed_modes.lock().push(start_service);
                clients.pop_front().ok_or_else(|| std::io::Error::other("no test connection"))
            },
        );
        server.join().unwrap();
        assert_eq!(*modes.lock(), vec![true, true]);
        assert!(quit.load(Ordering::Acquire));
    }

    #[test]
    fn failed_initial_start_does_not_authorize_automatic_restart() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (retry_tx, retry_rx) = mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            read_hello(&server);
            ipc::write_message(&mut server, &ServerMessage::Quit).unwrap();
        });
        let remote = Arc::new(Remote { writer: Mutex::new(None), retry: retry_tx });
        let mut connections = VecDeque::from([
            Err(std::io::Error::other("initial start failed")),
            Ok(client),
        ]);
        let mut modes = Vec::new();
        run_with_connector(
            remote,
            egui::Context::default(),
            mirror(),
            retry_rx,
            std::time::Duration::from_millis(10),
            |start_service| {
                modes.push(start_service);
                connections.pop_front().expect("unexpected reconnect")
            },
        );
        server_thread.join().unwrap();
        assert_eq!(modes, vec![true, false]);
    }

    #[test]
    fn explicit_retry_can_start_service_after_initial_failure() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (retry_tx, retry_rx) = mpsc::channel();
        let retry = retry_tx.clone();
        let server_thread = std::thread::spawn(move || {
            read_hello(&server);
            ipc::write_message(&mut server, &ServerMessage::Quit).unwrap();
        });
        let remote = Arc::new(Remote { writer: Mutex::new(None), retry: retry_tx });
        let mut client = Some(client);
        let mut modes = Vec::new();
        run_with_connector(
            remote,
            egui::Context::default(),
            mirror(),
            retry_rx,
            std::time::Duration::from_secs(1),
            |start_service| {
                modes.push(start_service);
                if modes.len() == 1 {
                    retry.send(()).unwrap();
                    Err(std::io::Error::other("initial start failed"))
                } else {
                    Ok(client.take().expect("unexpected reconnect"))
                }
            },
        );
        server_thread.join().unwrap();
        assert_eq!(modes, vec![true, true]);
    }

    #[test]
    fn quit_message_stops_remote_worker_without_reconnect() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (retry_tx, retry_rx) = mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            read_hello(&server);
            ipc::write_message(&mut server, &ServerMessage::Quit).unwrap();
        });
        let remote = Arc::new(Remote { writer: Mutex::new(None), retry: retry_tx });
        let mirror = mirror();
        let quit = mirror.quit_request.clone();
        let connections = Arc::new(Mutex::new(0));
        let observed = connections.clone();
        let mut client = Some(client);
        run_with_connector(
            remote,
            egui::Context::default(),
            mirror,
            retry_rx,
            std::time::Duration::from_millis(10),
            move |_| {
                *observed.lock() += 1;
                client.take().ok_or_else(|| std::io::Error::other("unexpected reconnect"))
            },
        );
        server_thread.join().unwrap();
        assert!(quit.load(Ordering::Acquire));
        assert_eq!(*connections.lock(), 1);
    }
}
