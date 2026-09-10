//! Service ⇄ window IPC on `$XDG_RUNTIME_DIR/splice.sock`, newline-delimited JSON.
//!
//! One long-lived `splice service` process owns the engine, the tray and this socket.
//! `splice window` processes render its snapshots and send commands, and exit when
//! closed; `splice` (open) and `splice quit` are one-shot clients. Wayland cannot hide a
//! window, so this split is what makes the X button work while Splice keeps running.

use std::io::{self, BufRead, Write};
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::{Command as Process, Stdio};
#[cfg(target_os = "linux")]
use std::time::Instant;
#[cfg(any(target_os = "linux", test))]
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use splice_core::Command;
#[cfg(target_os = "linux")]
use splice_core::UiState;

#[cfg(target_os = "linux")]
use crate::runtime::BootStatus;

pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

pub fn encode_message(message: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    if line.len() >= MAX_MESSAGE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Splice message exceeds size limit"));
    }
    line.push(b'\n');
    Ok(line)
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message on every connection; windows receive snapshots, others do not.
    Hello { window: bool },
    Command(Command),
    /// Retry engine bootstrap now instead of waiting out the interval.
    Retry,
    /// Show a window: focus the open one or spawn a new one.
    Open,
    OpenFiles,
    /// Stop the service (and every window attached to it).
    Quit,
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
pub fn socket_path() -> io::Result<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("splice.sock"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))
}

#[cfg(target_os = "linux")]
pub fn connect() -> io::Result<UnixStream> {
    UnixStream::connect(socket_path()?)
}

pub fn write_message(stream: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    stream.write_all(&encode_message(message)?)
}

/// `Ok(None)` at end of stream.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return finish_message(&line);
        }
        let count = available.iter().position(|b| *b == b'\n').map_or(available.len(), |i| i + 1);
        if line.len().saturating_add(count) > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Splice message exceeds size limit"));
        }
        let complete = available[count - 1] == b'\n';
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            return finish_message(&line);
        }
    }
}

fn finish_message<T: DeserializeOwned>(line: &[u8]) -> io::Result<Option<T>> {
    if line.is_empty() {
        Ok(None)
    } else if line.last() != Some(&b'\n') {
        Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Incomplete Splice message"))
    } else {
        serde_json::from_slice(line).map(Some).map_err(io::Error::other)
    }
}

pub struct AsyncMessages<R> {
    reader: tokio::io::BufReader<R>,
    pending: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> AsyncMessages<R> {
    pub fn new(reader: R) -> Self {
        Self { reader: tokio::io::BufReader::new(reader), pending: Vec::new() }
    }

    pub async fn next<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        use tokio::io::AsyncBufReadExt;
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return finish_message(&self.pending);
            }
            let count = available.iter().position(|b| *b == b'\n').map_or(available.len(), |i| i + 1);
            if self.pending.len().saturating_add(count) > MAX_MESSAGE_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Splice message exceeds size limit"));
            }
            let complete = available[count - 1] == b'\n';
            self.pending.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if complete {
                let result = finish_message(&self.pending);
                self.pending.clear();
                return result;
            }
        }
    }
}

/// Connect to the service, starting it detached first when none is running.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_writes_leave_the_stream_unchanged() {
        let mut stream = Vec::new();
        let error = write_message(&mut stream, &"x".repeat(MAX_MESSAGE_BYTES)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(stream.is_empty());
        write_message(&mut stream, &ClientMessage::OpenFiles).unwrap();
        let mut reader = io::BufReader::new(stream.as_slice());
        assert!(matches!(read_message(&mut reader).unwrap(), Some(ClientMessage::OpenFiles)));
    }

    #[test]
    fn oversized_and_truncated_messages_are_rejected() {
        let oversized = vec![b' '; MAX_MESSAGE_BYTES + 1];
        let mut reader = io::BufReader::new(oversized.as_slice());
        assert_eq!(read_message::<serde_json::Value>(&mut reader).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut reader = io::BufReader::new(b"\"Quit\"".as_slice());
        assert_eq!(read_message::<ClientMessage>(&mut reader).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn partial_message_survives_cancelled_read() {
        use tokio::io::AsyncWriteExt;
        let (reader, mut writer) = tokio::io::duplex(64);
        let mut messages = AsyncMessages::new(reader);
        writer.write_all(b"\"Qu").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(10), messages.next::<ClientMessage>()).await.is_err());
        writer.write_all(b"it\"\n\"Retry\"\n").await.unwrap();
        assert!(matches!(messages.next().await.unwrap(), Some(ClientMessage::Quit)));
        assert!(matches!(messages.next().await.unwrap(), Some(ClientMessage::Retry)));
    }

    #[tokio::test]
    async fn oversized_async_message_is_rejected_before_eof() {
        use tokio::io::AsyncWriteExt;
        let (reader, mut writer) = tokio::io::duplex(4096);
        let send = tokio::spawn(async move {
            writer.write_all(&vec![b' '; MAX_MESSAGE_BYTES + 1]).await.unwrap();
            writer
        });
        let mut messages = AsyncMessages::new(reader);
        let error = tokio::time::timeout(Duration::from_secs(2), messages.next::<ClientMessage>()).await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let _writer = send.await.unwrap();
    }
}
