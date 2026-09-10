use parking_lot::Mutex;
use splice_files::ipc::{self, Connection, HelperToService, ServiceToHelper};
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub const MAX_CLIENTS: usize = 2;
pub const OUTBOUND_QUEUE: usize = 256;
pub const HELPER_MODE: &str = "file-shelf";
pub const CONNECT_GRACE: Duration = Duration::from_secs(15);

pub type ClientId = u64;

pub enum Inbound {
    Connected(ClientId, ClientSender),
    Message(ClientId, HelperToService, Vec<OwnedFd>),
    Disconnected(ClientId),
    HelperExited(Result<(), String>),
}

#[derive(Clone)]
pub struct ClientSender {
    outbound: std::sync::mpsc::SyncSender<ServiceToHelper>,
    connection: Arc<Connection>,
}

impl ClientSender {
    pub fn send(&self, message: ServiceToHelper) -> bool {
        match self.outbound.try_send(message) {
            Ok(()) => true,
            Err(_) => {
                self.disconnect();
                false
            }
        }
    }

    pub fn disconnect(&self) {
        let _ = self.connection.shutdown();
    }
}

pub struct Listener {
    path: PathBuf,
    listener: Arc<UnixListener>,
    closing: Arc<AtomicBool>,
}

impl Listener {
    pub fn bind(events: mpsc::Sender<Inbound>) -> std::io::Result<Listener> {
        let path = ipc::socket_path()?;
        let listener = Arc::new(ipc::listener(&path)?);
        let closing = Arc::new(AtomicBool::new(false));
        let accept_listener = listener.clone();
        let accept_closing = closing.clone();
        std::thread::Builder::new()
            .name("splice-files-accept".into())
            .spawn(move || accept_loop(accept_listener, accept_closing, events))?;
        Ok(Listener { path, listener, closing })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
        let _ = ipc::shutdown_listener(&self.listener);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn accept_loop(listener: Arc<UnixListener>, closing: Arc<AtomicBool>, events: mpsc::Sender<Inbound>) {
    let live = Arc::new(AtomicUsize::new(0));
    let next = AtomicU64::new(1);
    loop {
        let connection = match Connection::accept(&listener) {
            Ok(connection) => connection,
            Err(error) => {
                if closing.load(Ordering::Acquire) {
                    return;
                }
                if matches!(error.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) {
                    tracing::info!(%error, "rejected file shelf connection");
                    continue;
                }
                tracing::warn!(%error, "file shelf listener stopped");
                return;
            }
        };
        if live.load(Ordering::Acquire) >= MAX_CLIENTS {
            tracing::info!("refusing extra file shelf connection");
            continue;
        }
        let id = next.fetch_add(1, Ordering::Relaxed);
        if let Err(error) = serve_client(id, connection, live.clone(), events.clone()) {
            tracing::warn!(%error, "cannot serve file shelf connection");
        }
    }
}

fn serve_client(id: ClientId, connection: Connection, live: Arc<AtomicUsize>, events: mpsc::Sender<Inbound>) -> std::io::Result<()> {
    let (mut sender, mut receiver) = connection.split()?;
    let connection = Arc::new(connection);
    let (outbound_tx, outbound_rx) = std::sync::mpsc::sync_channel::<ServiceToHelper>(OUTBOUND_QUEUE);
    let writer_connection = connection.clone();
    std::thread::Builder::new().name(format!("splice-files-write-{id}")).spawn(move || {
        while let Ok(message) = outbound_rx.recv() {
            if let Err(error) = sender.send(&message, &[]) {
                tracing::info!(client = id, %error, "file shelf stopped reading");
                let _ = writer_connection.shutdown();
                return;
            }
        }
    })?;
    live.fetch_add(1, Ordering::AcqRel);
    let reader_connection = connection.clone();
    let reader_events = events.clone();
    let reader_live = live.clone();
    let client_sender = ClientSender { outbound: outbound_tx, connection };
    let reader = std::thread::Builder::new().name(format!("splice-files-read-{id}")).spawn(move || {
        if reader_events.blocking_send(Inbound::Connected(id, client_sender)).is_err() {
            let _ = reader_connection.shutdown();
            reader_live.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        loop {
            match receiver.recv::<HelperToService>() {
                Ok(Some((message, fds))) => {
                    if let Err(error) = ipc::validate_helper_fds(&message, fds.len()) {
                        tracing::warn!(client = id, %error, "file shelf sent an invalid descriptor count");
                        break;
                    }
                    if reader_events.blocking_send(Inbound::Message(id, message, fds)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::info!(client = id, %error, "file shelf connection ended");
                    break;
                }
            }
        }
        let _ = reader_connection.shutdown();
        reader_live.fetch_sub(1, Ordering::AcqRel);
        let _ = reader_events.blocking_send(Inbound::Disconnected(id));
    });
    if let Err(error) = reader {
        live.fetch_sub(1, Ordering::AcqRel);
        return Err(error);
    }
    Ok(())
}

pub struct HelperProcess {
    pid: u32,
    started: Instant,
    alive: Arc<AtomicBool>,
}

impl HelperProcess {
    pub fn spawn(log_dir: Option<PathBuf>, events: mpsc::Sender<Inbound>) -> std::io::Result<HelperProcess> {
        let stderr = log_dir
            .and_then(|dir| std::fs::OpenOptions::new().create(true).append(true).open(dir.join("splice-files.log")).ok())
            .map(Stdio::from)
            .unwrap_or_else(Stdio::null);
        let mut child = Process::new("/proc/self/exe").arg(HELPER_MODE).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn()?;
        let pid = child.id();
        let alive = Arc::new(AtomicBool::new(true));
        let wait_alive = alive.clone();
        std::thread::Builder::new().name("splice-files-wait".into()).spawn(move || {
            let result = match child.wait() {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(format!("the file shelf exited with {status}")),
                Err(error) => Err(format!("cannot wait for the file shelf: {error}")),
            };
            wait_alive.store(false, Ordering::Release);
            let _ = events.blocking_send(Inbound::HelperExited(result));
        })?;
        Ok(HelperProcess { pid, started: Instant::now(), alive })
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn age(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn terminate(&self) {
        if self.alive() {
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
}

pub struct HelperSlot {
    process: Mutex<Option<HelperProcess>>,
}

impl HelperSlot {
    pub fn new() -> Self {
        Self { process: Mutex::new(None) }
    }

    pub fn running(&self) -> Option<Duration> {
        self.process.lock().as_ref().filter(|process| process.alive()).map(HelperProcess::age)
    }

    pub fn replace(&self, process: HelperProcess) {
        if let Some(previous) = self.process.lock().replace(process) {
            previous.terminate();
        }
    }

    pub fn terminate(&self) {
        if let Some(process) = self.process.lock().take() {
            process.terminate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splice_platform::files::FileOfferId;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn listener_delivers_messages_with_descriptor_validation_and_bounded_clients() {
        let dir = tempfile::tempdir().unwrap();
        let runtime_dir = dir.path().join("run");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let path = runtime_dir.join("splice").join("files.sock");
        let (events_tx, mut events) = mpsc::channel(16);
        let listener = Arc::new(ipc::listener(&path).unwrap());
        let closing = Arc::new(AtomicBool::new(false));
        let accept_listener = listener.clone();
        let accept_closing = closing.clone();
        std::thread::spawn(move || accept_loop(accept_listener, accept_closing, events_tx));

        let mut first = Connection::connect(&path).unwrap();
        first.send(&HelperToService::Hello { version: ipc::PROTOCOL_VERSION }, &[]).unwrap();
        let Some(Inbound::Connected(first_id, first_sender)) = events.recv().await else { panic!("expected connection before immediate hello") };
        match events.recv().await {
            Some(Inbound::Message(id, HelperToService::Hello { version }, fds)) => {
                assert_eq!(id, first_id);
                assert_eq!(version, ipc::PROTOCOL_VERSION);
                assert!(fds.is_empty());
            }
            _ => panic!("expected hello"),
        }
        assert!(first_sender.send(ServiceToHelper::Present));
        let (reply, fds): (ServiceToHelper, Vec<OwnedFd>) = first.recv().unwrap().unwrap();
        assert!(matches!(reply, ServiceToHelper::Present));
        assert!(fds.is_empty());

        let file = tempfile::NamedTempFile::new().unwrap();
        let fd = OwnedFd::from(file.as_file().try_clone().unwrap());
        first.send(&HelperToService::SourceDrop { id: FileOfferId::new(), recipient: "peer".into(), paths: vec![PathBuf::from("/tmp/x")], portal_fds: 0 }, &[&fd]).unwrap();
        assert!(matches!(events.recv().await, Some(Inbound::Disconnected(id)) if id == first_id));

        let mut second = Connection::connect(&path).unwrap();
        let Some(Inbound::Connected(second_id, _)) = events.recv().await else { panic!("expected second connection") };
        let _third = Connection::connect(&path).unwrap();
        let Some(Inbound::Connected(_, _)) = events.recv().await else { panic!("expected third connection") };
        let mut refused = Connection::connect(&path).unwrap();
        let closed: std::io::Result<Option<(ServiceToHelper, Vec<OwnedFd>)>> = refused.recv();
        assert!(matches!(closed, Ok(None) | Err(_)));
        second.send(&HelperToService::Dismiss { offer: FileOfferId::new() }, &[]).unwrap();
        match events.recv().await {
            Some(Inbound::Message(id, HelperToService::Dismiss { .. }, _)) => assert_eq!(id, second_id),
            _ => panic!("expected dismiss from the second client"),
        }
        closing.store(true, Ordering::Release);
        ipc::shutdown_listener(&listener).unwrap();
    }
}
