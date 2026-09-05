//! Service ⇄ window IPC on `$XDG_RUNTIME_DIR/splice.sock`, newline-delimited JSON.
//!
//! One long-lived `splice service` process owns the engine, the tray and this socket.
//! `splice window` processes render its snapshots and send commands, and exit when
//! closed; `splice` (open) and `splice quit` are one-shot clients. Wayland cannot hide a
//! window, so this split is what makes the X button work while Splice keeps running.

use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use splice_core::{Command, UiState};

use crate::runtime::BootStatus;

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message on every connection; windows receive snapshots, others do not.
    Hello { window: bool },
    Command(Command),
    /// Retry engine bootstrap now instead of waiting out the interval.
    Retry,
    /// Show a window: focus the open one or spawn a new one.
    Open,
    /// Stop the service (and every window attached to it).
    Quit,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Snapshot {
        status: BootStatus,
        /// True when a status notifier host accepted the tray icon.
        tray: bool,
        state: Box<UiState>,
    },
    Focus,
    Quit,
}

pub fn socket_path() -> io::Result<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("splice.sock"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))
}

pub fn connect() -> io::Result<UnixStream> {
    UnixStream::connect(socket_path()?)
}

pub fn write_message(stream: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)
}

/// `Ok(None)` at end of stream.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line).map(Some).map_err(io::Error::other)
}

/// Connect to the service, starting it detached first when none is running.
pub fn ensure_service() -> io::Result<UnixStream> {
    if let Ok(stream) = connect() {
        return Ok(stream);
    }
    spawn_service()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match connect() {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() >= deadline => return Err(err),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Start `splice service` in its own session so it outlives this process and the
/// terminal it may have been started from. Its log goes to splice.log because stderr
/// is not a terminal.
fn spawn_service() -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = Process::new(std::env::current_exe()?);
    command
        .arg("service")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
