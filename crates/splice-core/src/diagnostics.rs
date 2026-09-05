use serde::{Deserialize, Serialize};
use splice_proto::{BuildInfo, MachineId};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionPhase {
    #[default]
    Discovered,
    Connecting,
    SendingHello,
    AwaitingWelcome,
    AwaitingHello,
    AwaitingReady,
    Connected,
    Degraded,
    Disconnected,
    Rejected,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficSnapshot {
    pub frames_sent: u64,
    pub input_frames_sent: u64,
    pub bytes_sent: u64,
    pub max_queue_depth: u64,
    pub max_input_queue_us: u64,
    pub max_write_us: u64,
}

#[derive(Default)]
pub(crate) struct Traffic {
    frames: AtomicU64,
    inputs: AtomicU64,
    bytes: AtomicU64,
    queue_depth: AtomicU64,
    input_queue_us: AtomicU64,
    write_us: AtomicU64,
}

impl Traffic {
    pub fn queued(&self, depth: usize) {
        self.queue_depth.fetch_max(depth as u64, Ordering::Relaxed);
    }

    pub fn sent(&self, input: bool, bytes: usize, queued: Duration, written: Duration) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.write_us
            .fetch_max(written.as_micros() as u64, Ordering::Relaxed);
        if input {
            self.inputs.fetch_add(1, Ordering::Relaxed);
            self.input_queue_us
                .fetch_max(queued.as_micros() as u64, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> TrafficSnapshot {
        TrafficSnapshot {
            frames_sent: self.frames.load(Ordering::Relaxed),
            input_frames_sent: self.inputs.load(Ordering::Relaxed),
            bytes_sent: self.bytes.load(Ordering::Relaxed),
            max_queue_depth: self.queue_depth.load(Ordering::Relaxed),
            max_input_queue_us: self.input_queue_us.load(Ordering::Relaxed),
            max_write_us: self.write_us.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDiagnostics {
    pub phase: ConnectionPhase,
    pub address: Option<SocketAddr>,
    pub phase_changed_ms: u64,
    pub last_connected_ms: Option<u64>,
    pub last_heartbeat_ms: Option<u64>,
    pub attempts: u64,
    pub disconnects: u64,
    pub last_error: Option<String>,
    pub build: Option<BuildInfo>,
    pub traffic: TrafficSnapshot,
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis() as u64
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostics {
    pub peers: BTreeMap<MachineId, PeerDiagnostics>,
    pub export_path: Option<String>,
    pub export_error: Option<String>,
}

pub fn export(
    directory: &std::path::Path,
    state: &crate::UiState,
) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    #[derive(Serialize)]
    struct Report<'a> {
        schema: u16,
        created_ms: u64,
        state: &'a crate::UiState,
    }
    let created_ms = unix_ms();
    let directory = directory.join("diagnostics");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("splice-{created_ms}.json"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let bytes = serde_json::to_vec_pretty(&Report {
        schema: 1,
        created_ms,
        state,
    })?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(path)
}
