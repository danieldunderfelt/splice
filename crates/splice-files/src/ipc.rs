use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use splice_platform::files::{FileOfferId, ViewId};

pub const PROTOCOL_VERSION: u32 = 4;
pub const MAX_FDS: usize = 64;
pub const MAX_MESSAGE: usize = 64 * 1024;
pub const MAX_PACK: usize = 48 * 1024;
pub const SUMMARY_NAMES: usize = 3;
pub const SUMMARY_NAME_LEN: usize = 48;
pub const SUMMARY_ORIGIN_LEN: usize = 64;
pub const SUMMARY_ERROR_LEN: usize = 96;

pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_owned();
    out.push('…');
    out
}

pub fn summarize_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .take(SUMMARY_NAMES)
        .map(|name| truncate(name, SUMMARY_NAME_LEN))
        .collect()
}

pub fn summarize_origin(origin: &str) -> String {
    truncate(origin, SUMMARY_ORIGIN_LEN)
}

pub fn summarize_error(error: &str) -> String {
    truncate(error, SUMMARY_ERROR_LEN)
}

pub fn pack_by_size<T: serde::Serialize>(items: Vec<T>, max: usize) -> Vec<Vec<T>> {
    const ENVELOPE: usize = 128;
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut size = ENVELOPE;
    for item in items {
        let item_size = serde_json::to_vec(&item)
            .map(|bytes| bytes.len())
            .unwrap_or(max)
            + 1;
        if !current.is_empty() && size + item_size > max {
            chunks.push(std::mem::take(&mut current));
            size = ENVELOPE;
        }
        size += item_size;
        current.push(item);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

pub fn paged<T: serde::Serialize>(items: Vec<T>, max: usize) -> Vec<(Vec<T>, bool)> {
    let mut chunks = pack_by_size(items, max);
    if chunks.is_empty() {
        chunks.push(Vec::new());
    }
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| (chunk, index != last))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OfferStateDesc {
    Available,
    Revoked,
    Expired,
    Preparing,
    Receiving,
    Ready,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfferDesc {
    pub offer: FileOfferId,
    pub origin: String,
    pub names: Vec<String>,
    pub count: u32,
    pub total_size: Option<u64>,
    pub state: OfferStateDesc,
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerDesc {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptStateDesc {
    Retained,
    Unavailable,
    Clearing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiptDesc {
    pub receipt: String,
    pub offer: FileOfferId,
    pub origin: String,
    pub names: Vec<String>,
    pub count: u32,
    pub total_size: Option<u64>,
    pub state: ReceiptStateDesc,
    pub error: Option<String>,
    pub published: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HelperToService {
    Hello {
        version: u32,
    },
    SourceDrop {
        id: FileOfferId,
        recipient: String,
        paths: Vec<PathBuf>,
        portal_fds: u32,
    },
    CreateView {
        offer: FileOfferId,
    },
    CreateReceiptView {
        receipt: String,
    },
    DragStarted {
        view: ViewId,
    },
    DropPerformed {
        view: ViewId,
    },
    DragCancelled {
        view: ViewId,
    },
    DragFinished {
        view: ViewId,
    },
    ReleaseView {
        view: ViewId,
    },
    ReceiveToClipboard {
        offer: FileOfferId,
        generation: u64,
    },
    RepublishReceipt {
        receipt: String,
        generation: u64,
    },
    SaveTo {
        offer: FileOfferId,
        dest: PathBuf,
    },
    ClearReceipt {
        receipt: String,
    },
    CancelReceive {
        offer: FileOfferId,
    },
    RetryReceive {
        offer: FileOfferId,
    },
    ReceiptPaths {
        receipt: String,
    },
    Dismiss {
        offer: FileOfferId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServiceToHelper {
    Peers {
        peers: Vec<PeerDesc>,
    },
    Offers {
        offers: Vec<OfferDesc>,
        more: bool,
    },
    Receipts {
        receipts: Vec<ReceiptDesc>,
        more: bool,
    },
    ReceiptPaths {
        receipt: String,
        paths: Vec<PathBuf>,
        more: bool,
    },
    ViewReady {
        view: ViewId,
        offer: FileOfferId,
        uris: Vec<PathBuf>,
        portal_key: Option<String>,
    },
    ViewFailed {
        view: ViewId,
        offer: FileOfferId,
        error: String,
    },
    ReceiptViewReady {
        receipt: String,
        view: ViewId,
        uris: Vec<PathBuf>,
    },
    ReceiptViewFailed {
        receipt: String,
        error: String,
    },
    Progress {
        offer: FileOfferId,
        state: String,
        done: u64,
        total: Option<u64>,
    },
    PublishClipboard {
        offer: FileOfferId,
        generation: u64,
        uris: Vec<PathBuf>,
        portal_key: Option<String>,
    },
    RetireView {
        view: ViewId,
    },
    Present,
    Error {
        message: String,
    },
}

impl HelperToService {
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            HelperToService::Hello { .. }
                | HelperToService::DragStarted { .. }
                | HelperToService::DropPerformed { .. }
                | HelperToService::DragCancelled { .. }
                | HelperToService::DragFinished { .. }
                | HelperToService::ReleaseView { .. }
                | HelperToService::Dismiss { .. }
                | HelperToService::CancelReceive { .. }
                | HelperToService::ClearReceipt { .. }
                | HelperToService::ReceiptPaths { .. }
        )
    }
}

pub fn validate_helper_fds(msg: &HelperToService, fd_count: usize) -> io::Result<()> {
    let expected = match msg {
        HelperToService::SourceDrop { portal_fds, .. } => *portal_fds as usize,
        _ => 0,
    };
    if fd_count != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message declares {expected} fds but carried {fd_count}"),
        ));
    }
    Ok(())
}

pub fn validate_service_fds(fd_count: usize) -> io::Result<()> {
    if fd_count != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("service message carried {fd_count} unexpected fds"),
        ));
    }
    Ok(())
}

pub fn socket_path() -> io::Result<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir).join("splice/files.sock"));
        }
    }
    let uid = unsafe { libc::geteuid() };
    Ok(PathBuf::from(format!("/tmp/splice-{uid}/files.sock")))
}

fn seqpacket_socket() -> io::Result<RawFd> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

pub fn listener(path: &std::path::Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let fd = seqpacket_socket()?;
    let addr = socket_addr(path)?;
    let (raw, len) = addr_parts(&addr);
    let bound = unsafe { libc::bind(fd, &raw as *const _ as *const libc::sockaddr, len) };
    if bound < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    let listened = unsafe { libc::listen(fd, 16) };
    if listened < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(unsafe { UnixListener::from_raw_fd(fd) })
}

fn socket_addr(path: &std::path::Path) -> io::Result<libc::sockaddr_un> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= 108 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    addr.sun_path[..bytes.len()].copy_from_slice(unsafe {
        std::slice::from_raw_parts(bytes.as_ptr() as *const libc::c_char, bytes.len())
    });
    Ok(addr)
}

fn addr_parts(addr: &libc::sockaddr_un) -> (libc::sockaddr_un, libc::socklen_t) {
    (
        *addr,
        std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
    )
}

pub struct Connection {
    stream: UnixStream,
}

pub fn shutdown_listener(listener: &UnixListener) -> io::Result<()> {
    shutdown_fd(listener.as_raw_fd())
}

fn shutdown_fd(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::shutdown(fd, libc::SHUT_RDWR) } < 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::NotConnected {
            return Err(err);
        }
    }
    Ok(())
}

pub struct SendHalf {
    stream: UnixStream,
}

pub struct RecvHalf {
    stream: UnixStream,
}

impl SendHalf {
    pub fn send<T: Serialize>(&mut self, msg: &T, fds: &[&OwnedFd]) -> io::Result<()> {
        let result = send_message(&self.stream, msg, fds);
        if result.is_err() {
            let _ = shutdown_fd(self.stream.as_raw_fd());
        }
        result
    }
}

impl RecvHalf {
    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
        recv_message(&self.stream)
    }

    pub fn shutdown(&self) -> io::Result<()> {
        shutdown_fd(self.stream.as_raw_fd())
    }
}

impl Connection {
    pub fn split(&self) -> io::Result<(SendHalf, RecvHalf)> {
        Ok((
            SendHalf {
                stream: self.stream.try_clone()?,
            },
            RecvHalf {
                stream: self.stream.try_clone()?,
            },
        ))
    }
    pub fn connect(path: &std::path::Path) -> io::Result<Connection> {
        let fd = seqpacket_socket()?;
        let addr = socket_addr(path)?;
        let (raw, len) = addr_parts(&addr);
        let connected =
            unsafe { libc::connect(fd, &raw as *const _ as *const libc::sockaddr, len) };
        if connected < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        let conn = Connection {
            stream: unsafe { UnixStream::from_raw_fd(fd) },
        };
        conn.check_peer()?;
        Ok(conn)
    }

    pub fn accept(listener: &UnixListener) -> io::Result<Connection> {
        let (stream, _) = listener.accept()?;
        let conn = Connection { stream };
        conn.check_peer()?;
        Ok(conn)
    }

    fn check_peer(&self) -> io::Result<()> {
        let uid = self.peer_uid()?;
        let own = unsafe { libc::geteuid() };
        if uid != own {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("peer uid {uid} does not match {own}"),
            ));
        }
        Ok(())
    }

    pub fn shutdown(&self) -> io::Result<()> {
        shutdown_fd(self.stream.as_raw_fd())
    }

    pub fn peer_uid(&self) -> io::Result<u32> {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                self.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }

    pub fn send<T: Serialize>(&mut self, msg: &T, fds: &[&OwnedFd]) -> io::Result<()> {
        send_message(&self.stream, msg, fds)
    }

    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
        recv_message(&self.stream)
    }
}

pub fn validate_outgoing<T: Serialize>(msg: &T, fd_count: usize) -> io::Result<()> {
    encode_message(msg, fd_count).map(|_| ())
}

fn encode_message<T: Serialize>(msg: &T, fd_count: usize) -> io::Result<Vec<u8>> {
    if fd_count > MAX_FDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many fds"));
    }
    let mut bytes =
        serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message too large",
        ));
    }
    Ok(bytes)
}

fn send_message<T: Serialize>(stream: &UnixStream, msg: &T, fds: &[&OwnedFd]) -> io::Result<()> {
    let bytes = encode_message(msg, fds.len())?;
    send_record(stream.as_raw_fd(), &bytes, fds)
}

fn recv_message<T: DeserializeOwned>(stream: &UnixStream) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let mut buf = vec![0u8; MAX_MESSAGE];
    let Some((n, fds)) = recv_record(stream.as_raw_fd(), &mut buf)? else {
        return Ok(None);
    };
    let line = &buf[..n];
    let msg =
        serde_json::from_slice(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some((msg, fds)))
}

fn send_record(fd: RawFd, bytes: &[u8], fds: &[&OwnedFd]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let raw_fds: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let cmsg_space =
        unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; cmsg_space];
    if !raw_fds.is_empty() {
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space;
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN((raw_fds.len() * std::mem::size_of::<RawFd>()) as u32) as usize;
            std::ptr::copy_nonoverlapping(
                raw_fds.as_ptr(),
                libc::CMSG_DATA(cmsg) as *mut RawFd,
                raw_fds.len(),
            );
        }
        msg.msg_controllen =
            unsafe { libc::CMSG_SPACE((raw_fds.len() * std::mem::size_of::<RawFd>()) as u32) }
                as usize;
    }
    let sent = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short seqpacket send",
        ));
    }
    Ok(())
}

fn recv_record(fd: RawFd, buf: &mut [u8]) -> io::Result<Option<(usize, Vec<OwnedFd>)>> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let cmsg_space =
        unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; cmsg_space];
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space;
    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let truncated = msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC);
    let mut fds = Vec::new();
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        let hdr = unsafe { &*cmsg };
        if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == libc::SCM_RIGHTS {
            let count = (hdr.cmsg_len - unsafe { libc::CMSG_LEN(0) } as usize)
                / std::mem::size_of::<RawFd>();
            if count > MAX_FDS {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "too many fds"));
            }
            let data = unsafe { libc::CMSG_DATA(cmsg) as *const RawFd };
            for i in 0..count {
                let raw = unsafe { *data.add(i) };
                fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
            }
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
    }
    if truncated & libc::MSG_TRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated record",
        ));
    }
    if truncated & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated fd list",
        ));
    }
    if n == 0 {
        return Ok(None);
    }
    Ok(Some((n as usize, fds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Connection, Connection) {
        let mut fds = [0; 2];
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        let a = Connection {
            stream: unsafe { UnixStream::from_raw_fd(fds[0]) },
        };
        let b = Connection {
            stream: unsafe { UnixStream::from_raw_fd(fds[1]) },
        };
        (a, b)
    }

    #[test]
    fn empty_record_closes_its_received_descriptors() {
        use std::io::Read;
        let (a, b) = pair();
        let (left, mut right) = UnixStream::pair().unwrap();
        let descriptor = OwnedFd::from(left);
        send_record(a.stream.as_raw_fd(), &[], &[&descriptor]).unwrap();
        drop(descriptor);
        let mut bytes = [0; 8];
        assert!(recv_record(b.stream.as_raw_fd(), &mut bytes)
            .unwrap()
            .is_none());
        right
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        assert_eq!(right.read(&mut bytes).unwrap(), 0);
    }

    #[test]
    fn roundtrip_message_without_fds() {
        let (mut a, mut b) = pair();
        let msg = HelperToService::CreateView {
            offer: FileOfferId::new(),
        };
        a.send(&msg, &[]).unwrap();
        let (got, fds): (HelperToService, Vec<OwnedFd>) = b.recv().unwrap().unwrap();
        assert!(fds.is_empty());
        match got {
            HelperToService::CreateView { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_with_fds() {
        let (mut a, mut b) = pair();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp.as_file().try_clone().unwrap(), b"fd payload").unwrap();
        let fd: OwnedFd = std::fs::File::open(tmp.path()).unwrap().into();
        let msg = HelperToService::SourceDrop {
            id: FileOfferId::new(),
            recipient: "peer-a".into(),
            paths: vec![PathBuf::from("/tmp/x")],
            portal_fds: 1,
        };
        a.send(&msg, &[&fd]).unwrap();
        let (got, mut fds): (HelperToService, Vec<OwnedFd>) = b.recv().unwrap().unwrap();
        validate_helper_fds(&got, fds.len()).unwrap();
        match got {
            HelperToService::SourceDrop {
                portal_fds,
                recipient,
                ..
            } => {
                assert_eq!(portal_fds, 1);
                assert_eq!(recipient, "peer-a");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(fds.len(), 1);
        let mut file = std::fs::File::from(fds.remove(0));
        let mut content = String::new();
        use std::io::Read;
        file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "fd payload");
    }

    #[test]
    fn fd_count_mismatch_is_rejected() {
        let msg = HelperToService::SourceDrop {
            id: FileOfferId::new(),
            recipient: "peer-a".into(),
            paths: vec![PathBuf::from("/tmp/x")],
            portal_fds: 2,
        };
        assert!(validate_helper_fds(&msg, 1).is_err());
        assert!(validate_helper_fds(&msg, 3).is_err());
        assert!(validate_helper_fds(&msg, 2).is_ok());
        let plain = HelperToService::Dismiss {
            offer: FileOfferId::new(),
        };
        assert!(validate_helper_fds(&plain, 1).is_err());
        assert!(validate_helper_fds(&plain, 0).is_ok());
        assert!(validate_service_fds(1).is_err());
        assert!(validate_service_fds(0).is_ok());
    }

    #[test]
    fn clipboard_generation_round_trip() {
        let (mut a, mut b) = pair();
        let offer = FileOfferId::new();
        a.send(
            &HelperToService::ReceiveToClipboard {
                offer,
                generation: 7,
            },
            &[],
        )
        .unwrap();
        let (got, _): (HelperToService, Vec<OwnedFd>) = b.recv().unwrap().unwrap();
        match got {
            HelperToService::ReceiveToClipboard {
                offer: got_offer,
                generation,
            } => {
                assert_eq!(got_offer, offer);
                assert_eq!(generation, 7);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn control_messages_are_classified() {
        let offer = FileOfferId::new();
        let view = ViewId::new();
        for message in [
            HelperToService::Hello {
                version: PROTOCOL_VERSION,
            },
            HelperToService::DragStarted { view },
            HelperToService::DropPerformed { view },
            HelperToService::DragCancelled { view },
            HelperToService::DragFinished { view },
            HelperToService::ReleaseView { view },
            HelperToService::Dismiss { offer },
            HelperToService::CancelReceive { offer },
            HelperToService::ClearReceipt {
                receipt: "ab".repeat(16),
            },
            HelperToService::ReceiptPaths {
                receipt: "ab".repeat(16),
            },
        ] {
            assert!(message.is_control(), "{message:?}");
        }
        for message in [
            HelperToService::SourceDrop {
                id: offer,
                recipient: "peer".into(),
                paths: vec![],
                portal_fds: 0,
            },
            HelperToService::CreateView { offer },
            HelperToService::CreateReceiptView {
                receipt: "ab".repeat(16),
            },
            HelperToService::ReceiveToClipboard {
                offer,
                generation: 1,
            },
            HelperToService::RepublishReceipt {
                receipt: "ab".repeat(16),
                generation: 1,
            },
            HelperToService::SaveTo {
                offer,
                dest: PathBuf::from("/tmp"),
            },
            HelperToService::RetryReceive { offer },
        ] {
            assert!(!message.is_control(), "{message:?}");
        }
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("short", 48), "short");
        let long = "é".repeat(100);
        let out = truncate(&long, 48);
        assert!(out.ends_with('…'));
        assert!(out.len() <= 51);
        let mixed = format!("{}{}", "a".repeat(47), "é".repeat(10));
        let out = truncate(&mixed, 48);
        assert_eq!(out, format!("{}…", "a".repeat(47)));
    }

    #[test]
    fn max_size_receipt_snapshot_stays_under_the_record_cap() {
        let name = "n".repeat(255);
        let origin = "o".repeat(256);
        let descs: Vec<ReceiptDesc> = (0..MAX_RECORDS_TEST)
            .map(|i| ReceiptDesc {
                receipt: format!("{i:032x}"),
                offer: FileOfferId::new(),
                origin: summarize_origin(&origin),
                names: summarize_names(&[name.clone(), name.clone(), name.clone(), name.clone()]),
                count: 128,
                total_size: Some(u64::MAX),
                state: ReceiptStateDesc::Unavailable,
                error: Some(summarize_error(&"e".repeat(1024))),
                published: true,
            })
            .collect();
        let chunks = pack_by_size(descs, MAX_PACK);
        let mut total = 0;
        for (index, chunk) in chunks.iter().enumerate() {
            total += chunk.len();
            let bytes = serde_json::to_vec(&ServiceToHelper::Receipts {
                receipts: chunk.clone(),
                more: index + 1 < chunks.len(),
            })
            .unwrap();
            assert!(
                bytes.len() <= MAX_MESSAGE,
                "receipts chunk {index} is {} bytes",
                bytes.len()
            );
        }
        assert_eq!(total, MAX_RECORDS_TEST);
    }

    #[test]
    fn max_size_offer_snapshot_stays_under_the_record_cap() {
        let name = "n".repeat(255);
        let origin = "o".repeat(256);
        let descs: Vec<OfferDesc> = (0..MAX_RECORDS_TEST)
            .map(|_| OfferDesc {
                offer: FileOfferId::new(),
                origin: summarize_origin(&origin),
                names: summarize_names(&[name.clone(), name.clone(), name.clone(), name.clone()]),
                count: 128,
                total_size: Some(u64::MAX),
                state: OfferStateDesc::Available,
                expires_at: Some(i64::MAX),
            })
            .collect();
        let chunks = pack_by_size(descs, MAX_PACK);
        let mut total = 0;
        for (index, chunk) in chunks.iter().enumerate() {
            total += chunk.len();
            let bytes = serde_json::to_vec(&ServiceToHelper::Offers {
                offers: chunk.clone(),
                more: index + 1 < chunks.len(),
            })
            .unwrap();
            assert!(
                bytes.len() <= MAX_MESSAGE,
                "offers chunk {index} is {} bytes",
                bytes.len()
            );
        }
        assert_eq!(total, MAX_RECORDS_TEST);
    }

    #[test]
    fn max_size_receipt_paths_stay_under_the_record_cap() {
        let paths: Vec<PathBuf> = (0..128)
            .map(|i| {
                let mut path = PathBuf::from("/");
                let mut len = 1;
                while len < 4090 {
                    let part = format!("p{i:03}{}", "x".repeat(240.min(4090 - len - 4)));
                    len += part.len() + 1;
                    path.push(part);
                }
                path
            })
            .collect();
        assert!(paths.iter().all(|path| path.as_os_str().len() <= 4096));
        let chunks = pack_by_size(paths, MAX_PACK);
        let mut total = 0;
        for (index, chunk) in chunks.iter().enumerate() {
            total += chunk.len();
            let bytes = serde_json::to_vec(&ServiceToHelper::ReceiptPaths {
                receipt: "ab".repeat(16),
                paths: chunk.clone(),
                more: index + 1 < chunks.len(),
            })
            .unwrap();
            assert!(
                bytes.len() <= MAX_MESSAGE,
                "paths chunk {index} is {} bytes",
                bytes.len()
            );
        }
        assert_eq!(total, 128);
    }

    const MAX_RECORDS_TEST: usize = 128;

    #[test]
    fn peer_uid_matches_own() {
        let (a, _b) = pair();
        let own = unsafe { libc::geteuid() };
        assert_eq!(a.peer_uid().unwrap(), own);
    }

    #[test]
    fn listener_accept_flow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = listener(&path).unwrap();
        let mut client = Connection::connect(&path).unwrap();
        let mut server = Connection::accept(&listener).unwrap();
        client
            .send(
                &ServiceToHelper::Error {
                    message: "hi".into(),
                },
                &[],
            )
            .unwrap();
        let (got, _): (ServiceToHelper, Vec<OwnedFd>) = server.recv().unwrap().unwrap();
        match got {
            ServiceToHelper::Error { message } => assert_eq!(message, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn shutdown_unblocks_listener_and_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shutdown.sock");
        let listener = std::sync::Arc::new(listener(&path).unwrap());
        let accepting = {
            let listener = std::sync::Arc::clone(&listener);
            std::thread::spawn(move || Connection::accept(&listener).map(|_| ()))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        shutdown_listener(&listener).unwrap();
        assert!(accepting.join().unwrap().is_err());
        let (a, _b) = pair();
        let (_send, mut recv) = a.split().unwrap();
        let reading = std::thread::spawn(move || recv.recv::<ServiceToHelper>());
        std::thread::sleep(std::time::Duration::from_millis(50));
        a.shutdown().unwrap();
        assert!(matches!(reading.join().unwrap(), Ok(None) | Err(_)));
    }

    #[test]
    fn oversized_source_drop_is_rejected_before_queueing() {
        let mut message = HelperToService::SourceDrop {
            id: FileOfferId::new(),
            recipient: "peer-a".into(),
            paths: (0..100)
                .map(|index| {
                    PathBuf::from(format!("/home/daniel/{}/file-{index}", "x".repeat(700)))
                })
                .collect(),
            portal_fds: 0,
        };
        assert_eq!(
            validate_outgoing(&message, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        if let HelperToService::SourceDrop { paths, .. } = &mut message {
            paths.truncate(1);
        }
        assert!(validate_outgoing(&message, 0).is_ok());
        assert_eq!(
            validate_outgoing(&message, MAX_FDS + 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn failed_writer_shuts_down_all_socket_handles() {
        let (a, mut b) = pair();
        b.stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let (mut send, _reader_still_alive) = a.split().unwrap();
        let message = ServiceToHelper::Error {
            message: "x".repeat(MAX_MESSAGE),
        };
        assert_eq!(
            send.send(&message, &[]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(b.recv::<ServiceToHelper>().unwrap().is_none());
    }

    #[test]
    fn eof_returns_none() {
        let (mut a, b) = pair();
        drop(b);
        let got: io::Result<Option<(ServiceToHelper, Vec<OwnedFd>)>> = a.recv();
        assert!(matches!(got, Ok(None)));
    }
}
