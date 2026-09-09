//! `splice service`: the long-lived Linux process. Owns the engine bootstrap loop, the
//! tray, and the IPC socket (ipc.rs); spawns `splice window` processes on demand and
//! exits on Quit, SIGTERM or Ctrl-C after releasing captured input.

use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use parking_lot::{Mutex, RwLock};
use splice_core::{Command, EngineHandle, UiState};
use splice_proto::MachineId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::ipc::{self, ClientMessage, ServerMessage};
use crate::runtime::{self, BootStatus, RETRY_INTERVAL};
use crate::tray::{self, AppAction};

/// Two Open requests within this window spawn one window, not two.
const SPAWN_GRACE: Duration = Duration::from_secs(3);
/// Time given to the engine to release captured input before the process exits.
const RELEASE_GRACE: Duration = Duration::from_millis(150);
const CLIENT_DRAIN_GRACE: Duration = Duration::from_secs(1);

struct Shared {
    state: Arc<RwLock<UiState>>,
    status: Mutex<BootStatus>,
    tray: AtomicBool,
    /// Bumped on every state/status change; window clients send a snapshot per bump.
    version: watch::Sender<u64>,
    engine: Mutex<Option<EngineHandle>>,
}

impl Shared {
    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    fn snapshot(&self) -> ServerMessage {
        ServerMessage::Snapshot {
            status: self.status.lock().clone(),
            tray: self.tray.load(Ordering::Acquire),
            state: Box::new(self.state.read().clone()),
        }
    }
}

struct PendingSpawn {
    at: Instant,
    token: u64,
}

struct WindowRegistry {
    senders: Vec<mpsc::UnboundedSender<ServerMessage>>,
    pending_spawn: Option<PendingSpawn>,
    next_token: u64,
}

impl WindowRegistry {
    fn register(&mut self, sender: mpsc::UnboundedSender<ServerMessage>) {
        self.pending_spawn = None;
        self.senders.push(sender);
    }

    fn remove(&mut self, sender: &mpsc::UnboundedSender<ServerMessage>) {
        self.senders.retain(|candidate| !candidate.same_channel(sender));
    }

    fn focus_live(&mut self) -> bool {
        let mut focused = false;
        self.senders.retain(|sender| {
            if sender.is_closed() {
                return false;
            }
            if !focused {
                match sender.send(ServerMessage::Focus) {
                    Ok(()) => focused = true,
                    Err(_) => return false,
                }
            }
            true
        });
        focused
    }

    fn spawn_pending(&mut self) -> u64 {
        let token = self.next_token;
        self.next_token = self.next_token.checked_add(1).expect("window spawn token exhausted");
        self.pending_spawn = Some(PendingSpawn { at: Instant::now(), token });
        token
    }

    fn spawn_pending_recent(&self) -> bool {
        self.pending_spawn.as_ref().is_some_and(|pending| pending.at.elapsed() < SPAWN_GRACE)
    }

    fn clear_pending(&mut self, token: u64) {
        if self.pending_spawn.as_ref().is_some_and(|pending| pending.token == token) {
            self.pending_spawn = None;
        }
    }
}

type SharedWindowRegistry = Arc<Mutex<WindowRegistry>>;

pub fn run() -> anyhow::Result<()> {
    let path = ipc::socket_path()?;
    let Some(_lock) = acquire_service_lock(&path.with_extension("lock"))? else {
        tracing::info!("splice service already running");
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing stale service socket"),
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("splice-tokio")
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    let result = runtime.block_on(serve(&path));
    let _ = std::fs::remove_file(&path);
    result
}

fn acquire_service_lock(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file =
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

async fn serve(path: &PathBuf) -> anyhow::Result<()> {
    let listener = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    let shared = Arc::new(Shared {
        state: Arc::new(RwLock::new(UiState::initial(MachineId("self".into())))),
        status: Mutex::new(BootStatus::Starting),
        tray: AtomicBool::new(false),
        version: watch::channel(0).0,
        engine: Mutex::new(None),
    });
    let windows: SharedWindowRegistry = Arc::new(Mutex::new(WindowRegistry {
        senders: Vec::new(),
        pending_spawn: None,
        next_token: 0,
    }));
    let (actions_tx, mut actions_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let (retry_tx, retry_rx) = mpsc::unbounded_channel::<()>();
    tokio::spawn(engine_loop(shared.clone(), retry_rx));
    let mut clients = JoinSet::new();

    let (tray_tx, tray_rx) = mpsc::unbounded_channel::<AppAction>();
    let tray = tray::linux::spawn(shared.state.clone(), tray_tx, tokio::runtime::Handle::current());
    tokio::spawn(bridge_tray(tray_rx, shared.clone(), actions_tx.clone()));
    tokio::spawn(sync_tray(tray, shared.clone()));

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;
    tracing::info!(socket = %path.display(), "splice service running");
    let mut state_version = shared.version.subscribe();
    loop {
        tokio::select! {
            _ = state_version.changed() => {
                if shared.state.read().restart_requested { break; }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        clients.spawn(client(stream, shared.clone(), actions_tx.clone(), windows.clone()));
                    }
                    Err(err) => tracing::warn!(error = %err, "accept failed"),
                }
            }
            result = clients.join_next(), if !clients.is_empty() => {
                if let Some(Err(err)) = result {
                    tracing::warn!(error = %err, "client task failed");
                }
            }
            action = actions_rx.recv() => {
                let Some(action) = action else { break };
                match action {
                    ClientMessage::Open => open_window(&windows),
                    ClientMessage::Quit => break,
                    ClientMessage::Command(cmd) => {
                        match shared.engine.lock().as_ref() {
                            Some(engine) => engine.send(cmd),
                            None => tracing::debug!(?cmd, "engine offline; dropping command"),
                        }
                    }
                    ClientMessage::Retry => {
                        let _ = retry_tx.send(());
                    }
                    ClientMessage::Hello { .. } => {}
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        }
    }
    tracing::info!("splice service shutting down");
    drop(listener);
    for window in windows.lock().senders.iter() {
        let _ = window.send(ServerMessage::Quit);
    }
    let engine = shared.engine.lock().clone();
    if let Some(engine) = engine {
        engine.send(Command::Panic);
        tokio::time::sleep(RELEASE_GRACE).await;
    }
    drain_clients(&mut clients).await;
    Ok(())
}

async fn drain_clients(clients: &mut JoinSet<()>) {
    let drain = async {
        while let Some(result) = clients.join_next().await {
            if let Err(err) = result {
                tracing::warn!(error = %err, "client task failed");
            }
        }
    };
    if tokio::time::timeout(CLIENT_DRAIN_GRACE, drain).await.is_err() {
        clients.abort_all();
    }
}

/// Bootstrap the engine, forward its state, and re-bootstrap after failures. A panic
/// inside bootstrap surfaces as an ordinary offline status via the task's JoinError.
async fn engine_loop(shared: Arc<Shared>, mut retry_rx: mpsc::UnboundedReceiver<()>) {
    loop {
        let handle = match tokio::spawn(runtime::bootstrap()).await {
            Ok(Ok(handle)) => handle,
            Ok(Err(err)) => {
                tracing::warn!("engine bootstrap failed: {err:#}");
                *shared.status.lock() = BootStatus::Offline(format!("{err:#}"));
                shared.bump();
                wait_for_retry(&mut retry_rx).await;
                continue;
            }
            Err(join) => {
                let msg = join
                    .try_into_panic()
                    .map(|payload| runtime::panic_message(&*payload))
                    .unwrap_or_else(|err| err.to_string());
                tracing::warn!("engine bootstrap panicked: {msg}");
                *shared.status.lock() = BootStatus::Offline(format!("engine crashed during startup: {msg}"));
                shared.bump();
                wait_for_retry(&mut retry_rx).await;
                continue;
            }
        };
        let mut watch = handle.state();
        *shared.state.write() = watch.borrow_and_update().clone();
        *shared.engine.lock() = Some(handle);
        *shared.status.lock() = BootStatus::Online;
        shared.bump();
        while watch.changed().await.is_ok() {
            *shared.state.write() = watch.borrow_and_update().clone();
            shared.bump();
            if shared.state.read().restart_requested { return; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        *shared.engine.lock() = None;
        *shared.status.lock() = BootStatus::Offline("engine stopped".into());
        shared.bump();
        wait_for_retry(&mut retry_rx).await;
    }
}

async fn wait_for_retry(retry_rx: &mut mpsc::UnboundedReceiver<()>) {
    tokio::select! {
        _ = tokio::time::sleep(RETRY_INTERVAL) => {}
        _ = retry_rx.recv() => {}
    }
}

async fn bridge_tray(
    mut tray_rx: mpsc::UnboundedReceiver<AppAction>,
    shared: Arc<Shared>,
    actions: mpsc::UnboundedSender<ClientMessage>,
) {
    while let Some(action) = tray_rx.recv().await {
        let message = match action {
            AppAction::Open => ClientMessage::Open,
            AppAction::Quit => ClientMessage::Quit,
            AppAction::DisconnectAll => ClientMessage::Command(Command::Panic),
            AppAction::ToggleMachine(id) => {
                let enabled = shared.state.read().machines.iter().find(|m| m.id == id).map(|m| m.enabled);
                match enabled {
                    Some(enabled) => ClientMessage::Command(Command::SetMachineEnabled(id, !enabled)),
                    None => continue,
                }
            }
        };
        let _ = actions.send(message);
    }
}

async fn sync_tray(tray: tray::linux::LinuxTray, shared: Arc<Shared>) {
    let mut version = shared.version.subscribe();
    loop {
        let available = tray.available();
        if shared.tray.swap(available, Ordering::AcqRel) != available {
            shared.bump();
        }
        tray.sync(&shared.state.read());
        if version.changed().await.is_err() {
            return;
        }
    }
}

fn open_window(windows: &SharedWindowRegistry) {
    let mut registry = windows.lock();
    if registry.focus_live() {
        return;
    }
    if registry.spawn_pending_recent() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            tracing::warn!(error = %err, "cannot locate own executable to open a window");
            return;
        }
    };
    match Process::new(exe)
        .arg("window")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let token = registry.spawn_pending();
            let windows = windows.clone();
            std::thread::spawn(move || {
                let _ = child.wait();
                windows.lock().clear_pending(token);
            });
        }
        Err(err) => tracing::warn!(error = %err, "cannot spawn splice window"),
    }
}

async fn client(
    stream: UnixStream,
    shared: Arc<Shared>,
    actions: mpsc::UnboundedSender<ClientMessage>,
    windows: SharedWindowRegistry,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut version = shared.version.subscribe();
    let mut is_window = false;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                match serde_json::from_str::<ClientMessage>(&line) {
                    Ok(ClientMessage::Hello { window }) => {
                        if window && !is_window {
                            is_window = true;
                            windows.lock().register(out_tx.clone());
                            if write(&mut writer, &shared.snapshot()).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(message) => {
                        let _ = actions.send(message);
                    }
                    Err(err) => tracing::warn!(error = %err, "bad client message"),
                }
            }
            changed = version.changed(), if is_window => {
                if changed.is_err() || write(&mut writer, &shared.snapshot()).await.is_err() {
                    break;
                }
            }
            message = out_rx.recv() => {
                let Some(message) = message else { break };
                let quit = matches!(message, ServerMessage::Quit);
                if write(&mut writer, &message).await.is_err() {
                    break;
                }
                if quit {
                    break;
                }
            }
        }
    }
    if is_window {
        windows.lock().remove(&out_tx);
    }
}

async fn write(writer: &mut tokio::net::unix::OwnedWriteHalf, message: &ServerMessage) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn window_client_receives_quit_and_finishes_before_socket_closes() {
        let shared = Arc::new(Shared {
            state: Arc::new(RwLock::new(UiState::initial(MachineId("self".into())))),
            status: Mutex::new(BootStatus::Starting),
            tray: AtomicBool::new(false),
            version: watch::channel(0).0,
            engine: Mutex::new(None),
        });
        assert!(shared.engine.lock().is_none());
        let windows: SharedWindowRegistry = Arc::new(Mutex::new(WindowRegistry {
            senders: Vec::new(),
            pending_spawn: None,
            next_token: 0,
        }));
        let (actions_tx, _actions_rx) = mpsc::unbounded_channel();
        let (stream, remote) = UnixStream::pair().unwrap();
        let (remote_reader, mut remote_writer) = remote.into_split();
        let mut lines = BufReader::new(remote_reader).lines();
        let mut clients = JoinSet::new();
        clients.spawn(client(stream, shared, actions_tx, windows.clone()));

        let mut hello = serde_json::to_vec(&ClientMessage::Hello { window: true }).unwrap();
        hello.push(b'\n');
        remote_writer.write_all(&hello).await.unwrap();

        let initial = tokio::time::timeout(CLIENT_DRAIN_GRACE, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(serde_json::from_str::<ServerMessage>(&initial).unwrap(), ServerMessage::Snapshot { .. }));

        for sender in windows.lock().senders.iter() {
            sender.send(ServerMessage::Quit).unwrap();
        }
        drain_clients(&mut clients).await;
        assert!(clients.is_empty());
        assert!(windows.lock().senders.is_empty());

        let quit = tokio::time::timeout(CLIENT_DRAIN_GRACE, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(serde_json::from_str::<ServerMessage>(&quit).unwrap(), ServerMessage::Quit));
    }

    #[test]
    fn only_one_service_can_own_the_socket_at_a_time() {
        let path = std::env::temp_dir().join(format!("splice-service-lock-{}", std::process::id()));
        let first = acquire_service_lock(&path).unwrap().unwrap();
        assert!(acquire_service_lock(&path).unwrap().is_none());
        drop(first);
        let next = acquire_service_lock(&path).unwrap().unwrap();
        drop(next);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn closed_window_can_be_reopened_before_spawn_grace_expires() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut registry = WindowRegistry {
            senders: Vec::new(),
            pending_spawn: None,
            next_token: 0,
        };
        registry.spawn_pending();
        registry.register(sender);
        assert!(registry.pending_spawn.is_none());
        drop(receiver);
        assert!(!registry.focus_live());
        assert!(registry.senders.is_empty());
        assert!(!registry.spawn_pending_recent());
    }

    #[test]
    fn simultaneous_opens_deduplicate_pending_spawn() {
        let mut registry = WindowRegistry {
            senders: Vec::new(),
            pending_spawn: None,
            next_token: 0,
        };
        registry.spawn_pending();
        assert!(registry.spawn_pending_recent());
    }

    #[test]
    fn dead_sender_does_not_swallow_open_request() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut registry = WindowRegistry {
            senders: vec![sender],
            pending_spawn: None,
            next_token: 0,
        };
        drop(receiver);
        assert!(!registry.focus_live());
        assert!(!registry.spawn_pending_recent());
    }

    #[test]
    fn old_child_exit_cannot_clear_new_pending_spawn() {
        let mut registry = WindowRegistry {
            senders: Vec::new(),
            pending_spawn: None,
            next_token: 0,
        };
        let old = registry.spawn_pending();
        let new = registry.spawn_pending();
        registry.clear_pending(old);
        assert_eq!(registry.pending_spawn.as_ref().map(|pending| pending.token), Some(new));
    }
}
