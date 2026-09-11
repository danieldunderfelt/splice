use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request, FUSE_ROOT_ID,
};
use parking_lot::{Condvar, Mutex, RwLock};
use splice_platform::files::{
    EntryId, EntryKind, FileContentSource, FileEntry, FileManifest, ViewId, ViewState, ViewStats,
};

use crate::gate::{CancelOutcome, Gate, ReadDecision};

const TTL: Duration = Duration::from_secs(1);
const MAX_READ: usize = 1024 * 1024;
const DROP_SETTLE: Duration = Duration::from_millis(250);

pub const DEFAULT_WORKERS: usize = 4;
pub const DEFAULT_QUEUE: usize = 64;

#[derive(Clone, Copy)]
enum Node {
    ViewDir { view: ViewId },
    Entry { view: ViewId, entry: EntryId },
}

struct ReadJob {
    view: Arc<View>,
    entry: EntryId,
    offset: u64,
    size: u32,
    reply: ReplyData,
    await_drop: bool,
}

struct View {
    id: ViewId,
    manifest: FileManifest,
    dir_ino: u64,
    entry_inos: HashMap<EntryId, u64>,
    gate: Mutex<Gate>,
    drop_signal: Condvar,
    lifecycle: Mutex<()>,
    cache: Mutex<HashMap<EntryId, PathBuf>>,
    materializing: Mutex<HashSet<EntryId>>,
    materialized: Condvar,
    bytes_served: AtomicU64,
    reads: AtomicU64,
    denied_reads: AtomicU64,
    commits: AtomicU64,
    opens: AtomicUsize,
}

struct MountInner {
    views: RwLock<HashMap<ViewId, Arc<View>>>,
    inos: RwLock<HashMap<u64, Node>>,
    next_ino: AtomicU64,
    content: Arc<dyn FileContentSource>,
    queue: std::sync::mpsc::SyncSender<ReadJob>,
    shutdown: AtomicBool,
}

pub struct MountConfig {
    pub mountpoint: PathBuf,
    pub content: Arc<dyn FileContentSource>,
    pub workers: usize,
    pub queue: usize,
}

pub struct Mount {
    inner: Arc<MountInner>,
    _session: BackgroundSession,
    mountpoint: PathBuf,
}

impl Mount {
    pub fn spawn(config: MountConfig) -> io::Result<Mount> {
        unmount_stale(&config.mountpoint);
        std::fs::create_dir_all(&config.mountpoint)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config.mountpoint, std::fs::Permissions::from_mode(0o700))?;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<ReadJob>(config.queue.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let inner = Arc::new(MountInner {
            views: RwLock::new(HashMap::new()),
            inos: RwLock::new(HashMap::new()),
            next_ino: AtomicU64::new(FUSE_ROOT_ID + 1),
            content: config.content,
            queue: tx,
            shutdown: AtomicBool::new(false),
        });
        for _ in 0..config.workers.max(1) {
            let inner = Arc::clone(&inner);
            let rx = Arc::clone(&rx);
            std::thread::Builder::new()
                .name("splice-fuse-read".into())
                .spawn(move || read_worker(inner, rx))?;
        }
        let options = vec![
            MountOption::FSName("splice-files".into()),
            MountOption::RO,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoExec,
            MountOption::DefaultPermissions,
        ];
        let session = match fuser::spawn_mount2(ViewFs { inner: Arc::clone(&inner) }, &config.mountpoint, &options) {
            Ok(session) => session,
            Err(error) => {
                inner.shutdown.store(true, Ordering::Relaxed);
                return Err(error);
            }
        };
        Ok(Mount {
            inner,
            _session: session,
            mountpoint: config.mountpoint,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn create_view(&self, manifest: FileManifest) -> ViewId {
        let id = ViewId::new();
        self.insert_view(id, manifest, Gate::new());
        id
    }

    pub fn restore_view(&self, id: ViewId, manifest: FileManifest) -> bool {
        if self.inner.views.read().contains_key(&id) {
            return false;
        }
        self.insert_view(id, manifest, Gate::committed());
        true
    }

    fn insert_view(&self, id: ViewId, manifest: FileManifest, gate: Gate) {
        let mut inos = self.inner.inos.write();
        let dir_ino = self.inner.next_ino.fetch_add(1, Ordering::Relaxed);
        inos.insert(dir_ino, Node::ViewDir { view: id });
        let mut entry_inos = HashMap::with_capacity(manifest.entries.len());
        for entry in &manifest.entries {
            let ino = self.inner.next_ino.fetch_add(1, Ordering::Relaxed);
            entry_inos.insert(entry.id, ino);
            inos.insert(ino, Node::Entry { view: id, entry: entry.id });
        }
        drop(inos);
        let view = Arc::new(View {
            id,
            manifest,
            dir_ino,
            entry_inos,
            gate: Mutex::new(gate),
            drop_signal: Condvar::new(),
            lifecycle: Mutex::new(()),
            cache: Mutex::new(HashMap::new()),
            materializing: Mutex::new(HashSet::new()),
            materialized: Condvar::new(),
            bytes_served: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            denied_reads: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            opens: AtomicUsize::new(0),
        });
        self.inner.views.write().insert(id, view);
    }

    pub fn view_dir(&self, view: ViewId) -> PathBuf {
        self.mountpoint.join(format!("v{view}"))
    }

    pub fn view_uris(&self, view: ViewId) -> Vec<PathBuf> {
        let views = self.inner.views.read();
        let Some(view) = views.get(&view) else {
            return Vec::new();
        };
        let dir = self.view_dir(view.id);
        view.manifest
            .roots()
            .map(|root| dir.join(&root.name))
            .collect()
    }

    pub fn view_state(&self, view: ViewId) -> Option<ViewState> {
        Some(self.inner.views.read().get(&view)?.gate.lock().state())
    }

    pub fn drag_started(&self, view: ViewId) {
        if let Some(view) = self.inner.views.read().get(&view).cloned() {
            view.gate.lock().drag_started();
        }
    }

    pub fn drop_performed(&self, view: ViewId) -> bool {
        let views = self.inner.views.read();
        let Some(view) = views.get(&view).cloned() else {
            return false;
        };
        drop(views);
        let performed = view.gate.lock().drop_performed();
        view.drop_signal.notify_all();
        performed
    }

    pub fn cancel_view(&self, view: ViewId) -> CancelOutcome {
        let views = self.inner.views.read();
        let Some(view) = views.get(&view).cloned() else {
            return CancelOutcome::AlreadyRetired;
        };
        let outcome = {
            let _lifecycle = view.lifecycle.lock();
            let outcome = view.gate.lock().cancel();
            match outcome {
                CancelOutcome::ZeroPayload => self.inner.content.on_cancel(view.id, false),
                CancelOutcome::StopTransfer => self.inner.content.on_cancel(view.id, true),
                CancelOutcome::AlreadyRetired => {}
            }
            outcome
        };
        view.drop_signal.notify_all();
        drop(views);
        if outcome != CancelOutcome::AlreadyRetired {
            self.remove_view(view.id);
        }
        outcome
    }

    pub fn retire_view(&self, view: ViewId) {
        if let Some(view) = self.inner.views.read().get(&view).cloned() {
            view.gate.lock().retire();
            view.drop_signal.notify_all();
        }
        self.remove_view(view);
    }

    pub fn retire_if_unused(&self, ids: &[ViewId]) -> Result<(), ViewId> {
        let held: Vec<Arc<View>> = {
            let views = self.inner.views.read();
            let mut seen = HashSet::new();
            ids.iter().filter(|id| seen.insert(**id)).filter_map(|id| views.get(id).cloned()).collect()
        };
        let guards: Vec<_> = held.iter().map(|view| view.lifecycle.lock()).collect();
        if let Some(busy) = held.iter().find(|view| view.opens.load(Ordering::SeqCst) > 0) {
            return Err(busy.id);
        }
        for view in &held {
            view.gate.lock().retire();
            view.drop_signal.notify_all();
        }
        drop(guards);
        for view in &held {
            self.remove_view(view.id);
        }
        Ok(())
    }

    pub fn stats(&self, view: ViewId) -> Option<ViewStats> {
        let views = self.inner.views.read();
        let view = views.get(&view)?;
        Some(ViewStats {
            bytes_served: view.bytes_served.load(Ordering::Relaxed),
            reads: view.reads.load(Ordering::Relaxed),
            denied_reads: view.denied_reads.load(Ordering::Relaxed),
            commits: view.commits.load(Ordering::Relaxed),
        })
    }

    pub fn open_readers(&self, view: ViewId) -> usize {
        self.inner.views.read().get(&view).map(|view| view.opens.load(Ordering::SeqCst)).unwrap_or(0)
    }

    fn remove_view(&self, id: ViewId) {
        let Some(view) = self.inner.views.write().remove(&id) else {
            return;
        };
        let mut inos = self.inner.inos.write();
        inos.remove(&view.dir_ino);
        for ino in view.entry_inos.values() {
            inos.remove(ino);
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
    }
}

fn unmount_stale(mountpoint: &Path) {
    let _ = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg(mountpoint)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn read_worker(inner: Arc<MountInner>, rx: Arc<Mutex<std::sync::mpsc::Receiver<ReadJob>>>) {
    loop {
        let next = {
            let rx = rx.lock();
            rx.recv_timeout(Duration::from_millis(100))
        };
        match next {
            Ok(job) => {
                if inner.shutdown.load(Ordering::Relaxed) {
                    job.reply.error(libc::EIO);
                    drain_queue(&rx);
                    return;
                }
                serve_read(&inner, job)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if inner.shutdown.load(Ordering::Relaxed) {
                    drain_queue(&rx);
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn drain_queue(rx: &Mutex<std::sync::mpsc::Receiver<ReadJob>>) {
    loop {
        let job = {
            let rx = rx.lock();
            rx.try_recv()
        };
        match job {
            Ok(job) => job.reply.error(libc::EIO),
            Err(_) => return,
        }
    }
}

fn decide(inner: &MountInner, view: &Arc<View>) -> ReadDecision {
    let _lifecycle = view.lifecycle.lock();
    let decision = view.gate.lock().on_read();
    if decision == ReadDecision::Commit {
        view.commits.fetch_add(1, Ordering::Relaxed);
        inner.content.on_commit(view.id);
    }
    decision
}

fn await_drop(view: &View) {
    let mut gate = view.gate.lock();
    let deadline = Instant::now() + DROP_SETTLE;
    while gate.state() == ViewState::Offered && gate.is_dragging() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || view.drop_signal.wait_for(&mut gate, remaining).timed_out() {
            break;
        }
    }
}

fn serve_read(inner: &MountInner, job: ReadJob) {
    let ReadJob { view, entry, offset, size, reply, await_drop: wait } = job;
    if wait {
        await_drop(&view);
        match decide(inner, &view) {
            ReadDecision::Deny => {
                view.denied_reads.fetch_add(1, Ordering::Relaxed);
                reply.error(libc::EIO);
                return;
            }
            ReadDecision::Commit | ReadDecision::Serve => {
                let Some(meta) = view.manifest.entry(entry) else {
                    reply.error(libc::EIO);
                    return;
                };
                if meta.kind != EntryKind::File || meta.size == 0 {
                    reply.data(&[]);
                    return;
                }
            }
        }
    }
    let path = match ensure_materialized(inner, &view, entry) {
        Ok(path) => path,
        Err(()) => {
            reply.error(libc::EIO);
            return;
        }
    };
    if view.gate.lock().state() == ViewState::Retired {
        reply.error(libc::EIO);
        return;
    }
    let result = (|| -> io::Result<Vec<u8>> {
        let file = std::fs::File::open(&path)?;
        let len = file.metadata()?.len();
        if offset >= len {
            return Ok(Vec::new());
        }
        let want = std::cmp::min(size as usize, (len - offset) as usize);
        let mut buf = vec![0u8; want];
        file.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    })();
    match result {
        Ok(buf) => {
            view.bytes_served.fetch_add(buf.len() as u64, Ordering::Relaxed);
            reply.data(&buf);
        }
        Err(_) => reply.error(libc::EIO),
    }
}

fn ensure_materialized(inner: &MountInner, view: &Arc<View>, entry: EntryId) -> Result<PathBuf, ()> {
    if let Some(path) = view.cache.lock().get(&entry) {
        return Ok(path.clone());
    }
    let mut in_flight = view.materializing.lock();
    loop {
        if let Some(path) = view.cache.lock().get(&entry) {
            return Ok(path.clone());
        }
        if in_flight.contains(&entry) {
            view.materialized.wait(&mut in_flight);
            if view.gate.lock().state() == ViewState::Retired {
                return Err(());
            }
            continue;
        }
        in_flight.insert(entry);
        break;
    }
    drop(in_flight);
    if view.gate.lock().state() == ViewState::Retired {
        let mut in_flight = view.materializing.lock();
        in_flight.remove(&entry);
        view.materialized.notify_all();
        return Err(());
    }
    let result = inner.content.materialize(view.id, entry);
    let mut in_flight = view.materializing.lock();
    in_flight.remove(&entry);
    if view.gate.lock().state() == ViewState::Retired {
        view.materialized.notify_all();
        return Err(());
    }
    match result {
        Ok(path) => {
            view.cache.lock().insert(entry, path.clone());
            view.materialized.notify_all();
            Ok(path)
        }
        Err(err) => {
            tracing::warn!(view = %view.id, entry = %entry, "materialize failed: {err}");
            view.materialized.notify_all();
            Err(())
        }
    }
}

fn admit_open(view: &View) -> bool {
    let _lifecycle = view.lifecycle.lock();
    if view.gate.lock().state() == ViewState::Retired {
        return false;
    }
    view.opens.fetch_add(1, Ordering::SeqCst);
    true
}

fn system_time(secs: i64, nanos: u32) -> SystemTime {
    if secs < 0 {
        SystemTime::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()) + Duration::from_nanos(nanos as u64)
    } else {
        SystemTime::UNIX_EPOCH + Duration::new(secs as u64, nanos)
    }
}

fn dir_attr(ino: u64) -> FileAttr {
    FileAttr {
        ino,
        size: 0,
        blocks: 0,
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o500,
        nlink: 2,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn entry_attr(ino: u64, entry: &FileEntry) -> FileAttr {
    let kind = match entry.kind {
        EntryKind::File => FileType::RegularFile,
        EntryKind::Dir => FileType::Directory,
        EntryKind::Symlink => FileType::Symlink,
    };
    let perm: u16 = match entry.kind {
        EntryKind::Dir => ((entry.mode & 0o777) | 0o500) as u16,
        EntryKind::File => ((entry.mode & 0o777) | 0o400) as u16,
        EntryKind::Symlink => 0o777,
    };
    let modified = system_time(entry.mtime, entry.mtime_nanos);
    FileAttr {
        ino,
        size: entry.size,
        blocks: entry.size.div_ceil(512),
        atime: modified,
        mtime: modified,
        ctime: modified,
        crtime: modified,
        kind,
        perm,
        nlink: 1,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

struct ViewFs {
    inner: Arc<MountInner>,
}

impl ViewFs {
    fn node(&self, ino: u64) -> Option<Node> {
        self.inner.inos.read().get(&ino).copied()
    }

    fn view(&self, id: ViewId) -> Option<Arc<View>> {
        self.inner.views.read().get(&id).cloned()
    }

    fn enqueue(&self, job: ReadJob) {
        match self.inner.queue.try_send(job) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(job)) => job.reply.error(libc::EAGAIN),
            Err(std::sync::mpsc::TrySendError::Disconnected(job)) => job.reply.error(libc::EIO),
        }
    }

    fn entry_node<'a>(&self, view: &'a View, entry: EntryId) -> Option<(u64, &'a FileEntry)> {
        let ino = *view.entry_inos.get(&entry)?;
        let meta = view.manifest.entry(entry)?;
        Some((ino, meta))
    }

    fn children_of(&self, view: &View, node: Node) -> Vec<(u64, FileType, String)> {
        let parent = match node {
            Node::ViewDir { .. } => None,
            Node::Entry { entry, .. } => Some(entry),
        };
        view.manifest
            .entries
            .iter()
            .filter(|e| e.parent == parent)
            .filter_map(|e| {
                let (ino, meta) = self.entry_node(view, e.id)?;
                let kind = match meta.kind {
                    EntryKind::File => FileType::RegularFile,
                    EntryKind::Dir => FileType::Directory,
                    EntryKind::Symlink => FileType::Symlink,
                };
                Some((ino, kind, meta.name.clone()))
            })
            .collect()
    }
}

impl Filesystem for ViewFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        if parent == FUSE_ROOT_ID {
            let Some(id_str) = name.strip_prefix('v') else {
                reply.error(libc::ENOENT);
                return;
            };
            let Ok(id) = id_str.parse::<ViewId>() else {
                reply.error(libc::ENOENT);
                return;
            };
            let Some(view) = self.view(id) else {
                reply.error(libc::ENOENT);
                return;
            };
            reply.entry(&TTL, &dir_attr(view.dir_ino), 0);
            return;
        }
        let Some(node) = self.node(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let (view_id, parent_entry) = match node {
            Node::ViewDir { view } => (view, None),
            Node::Entry { view, entry } => (view, Some(entry)),
        };
        let Some(view) = self.view(view_id) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(child) = view
            .manifest
            .entries
            .iter()
            .find(|e| e.parent == parent_entry && e.name == name)
        else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(ino) = view.entry_inos.get(&child.id).copied() else {
            reply.error(libc::ENOENT);
            return;
        };
        reply.entry(&TTL, &entry_attr(ino, child), 0);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == FUSE_ROOT_ID {
            reply.attr(&TTL, &dir_attr(FUSE_ROOT_ID));
            return;
        }
        let Some(node) = self.node(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match node {
            Node::ViewDir { view } => {
                if self.view(view).is_some() {
                    reply.attr(&TTL, &dir_attr(ino));
                } else {
                    reply.error(libc::ENOENT);
                }
            }
            Node::Entry { view, entry } => {
                let Some(view) = self.view(view) else {
                    reply.error(libc::ENOENT);
                    return;
                };
                let Some(meta) = view.manifest.entry(entry) else {
                    reply.error(libc::ENOENT);
                    return;
                };
                reply.attr(&TTL, &entry_attr(ino, meta));
            }
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let Some(Node::Entry { view, entry }) = self.node(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(view) = self.view(view) else {
            reply.error(libc::EIO);
            return;
        };
        let Some(meta) = view.manifest.entry(entry) else {
            reply.error(libc::EIO);
            return;
        };
        match &meta.link_target {
            Some(target) => reply.data(target.as_bytes()),
            None => reply.error(libc::EINVAL),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.node(ino) {
            Some(Node::Entry { view, .. }) => {
                let Some(view) = self.view(view) else {
                    reply.error(libc::ENOENT);
                    return;
                };
                if admit_open(&view) {
                    reply.opened(ino, 0);
                } else {
                    reply.error(libc::ENOENT);
                }
            }
            _ => reply.error(libc::ENOENT),
        }
    }

    fn release(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, _flags: i32, _lock_owner: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        if let Some(Node::Entry { view, .. }) = self.node(ino) {
            if let Some(view) = self.view(view) {
                let _ = view.opens.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |opens| Some(opens.saturating_sub(1)));
            }
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(Node::Entry { view, entry }) = self.node(ino) else {
            reply.error(libc::EIO);
            return;
        };
        let Some(view) = self.view(view) else {
            reply.error(libc::EIO);
            return;
        };
        view.reads.fetch_add(1, Ordering::Relaxed);
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let awaiting_drop = {
            let _lifecycle = view.lifecycle.lock();
            let gate = view.gate.lock();
            gate.state() == ViewState::Offered && gate.is_dragging()
        };
        if awaiting_drop {
            self.enqueue(ReadJob {
                view: Arc::clone(&view),
                entry,
                offset: offset as u64,
                size: (size as usize).min(MAX_READ) as u32,
                reply,
                await_drop: true,
            });
            return;
        }
        match decide(&self.inner, &view) {
            ReadDecision::Deny => {
                view.denied_reads.fetch_add(1, Ordering::Relaxed);
                reply.error(libc::EIO);
            }
            ReadDecision::Commit | ReadDecision::Serve => {
                let Some(meta) = view.manifest.entry(entry) else {
                    reply.error(libc::EIO);
                    return;
                };
                if meta.kind != EntryKind::File || meta.size == 0 {
                    reply.data(&[]);
                    return;
                }
                self.enqueue(ReadJob {
                    view: Arc::clone(&view),
                    entry,
                    offset: offset as u64,
                    size: (size as usize).min(MAX_READ) as u32,
                    reply,
                    await_drop: false,
                });
            }
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        if ino == FUSE_ROOT_ID {
            reply.opened(ino, 0);
            return;
        }
        let view = match self.node(ino) {
            Some(Node::ViewDir { view }) => view,
            Some(Node::Entry { view, .. }) => view,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let Some(view) = self.view(view) else {
            reply.error(libc::ENOENT);
            return;
        };
        if admit_open(&view) {
            reply.opened(ino, 0);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn releasedir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, _flags: i32, reply: ReplyEmpty) {
        if ino != FUSE_ROOT_ID {
            let view = match self.node(ino) {
                Some(Node::ViewDir { view }) => Some(view),
                Some(Node::Entry { view, .. }) => Some(view),
                None => None,
            };
            if let Some(view) = view.and_then(|view| self.view(view)) {
                let _ = view.opens.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |opens| Some(opens.saturating_sub(1)));
            }
        }
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".into()),
            (FUSE_ROOT_ID, FileType::Directory, "..".into()),
        ];
        if ino == FUSE_ROOT_ID {
            for view in self.inner.views.read().values() {
                entries.push((view.dir_ino, FileType::Directory, format!("v{}", view.id)));
            }
        } else {
            let Some(node) = self.node(ino) else {
                reply.error(libc::ENOENT);
                return;
            };
            let view_id = match node {
                Node::ViewDir { view } => view,
                Node::Entry { view, entry } => {
                    let Some(view) = self.view(view) else {
                        reply.error(libc::ENOENT);
                        return;
                    };
                    match view.manifest.entry(entry).map(|e| e.kind) {
                        Some(EntryKind::Dir) => view.id,
                        _ => {
                            reply.error(libc::ENOTDIR);
                            return;
                        }
                    }
                }
            };
            let Some(view) = self.view(view_id) else {
                reply.error(libc::ENOENT);
                return;
            };
            entries.extend(self.children_of(&view, node));
        }
        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(child_ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamps_preserve_fractional_seconds_before_and_at_epoch() {
        assert_eq!(super::system_time(-1, 500_000_000), std::time::UNIX_EPOCH - std::time::Duration::from_millis(500));
        assert_eq!(super::system_time(0, 123_456_789), std::time::UNIX_EPOCH + std::time::Duration::from_nanos(123_456_789));
    }

    use super::*;
    use crate::local::LocalDirSource;
    use crate::manifest;

    static FUSE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fuse_available() -> bool {
        Path::new("/dev/fuse").exists()
            && std::process::Command::new("fusermount3")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
        mount: Mount,
        source: Arc<LocalDirSource>,
        tree: manifest::SourceTree,
        file_name: String,
        content: Vec<u8>,
    }

    fn fixture() -> Option<Fixture> {
        if !fuse_available() {
            eprintln!("skipping: /dev/fuse or fusermount3 unavailable");
            return None;
        }
        let guard = FUSE_TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src/root");
        std::fs::create_dir_all(&src).unwrap();
        let content = b"splice pilot gate test payload\n".to_vec();
        std::fs::write(src.join("hello.txt"), &content).unwrap();
        let tree = manifest::from_paths(
            splice_platform::files::FileOfferId::new(),
            "test",
            &[src],
        )
        .unwrap();
        let source = Arc::new(
            LocalDirSource::new(tmp.path().join("cache"), Box::new(|_| {})).unwrap(),
        );
        let mount = Mount::spawn(MountConfig {
            mountpoint: tmp.path().join("mnt"),
            content: Arc::clone(&source) as Arc<dyn FileContentSource>,
            workers: 2,
            queue: 8,
        })
        .unwrap();
        Some(Fixture {
            _tmp: tmp,
            _guard: guard,
            mount,
            source,
            tree,
            file_name: "hello.txt".into(),
            content,
        })
    }

    impl Fixture {
        fn new_view(&self) -> (ViewId, PathBuf) {
            let view = self.mount.create_view(self.tree.manifest.clone());
            self.source.register_view(view, &self.tree);
            let path = self
                .mount
                .view_dir(view)
                .join("root")
                .join(&self.file_name);
            (view, path)
        }
    }

    #[test]
    fn pre_drop_metadata_ok_content_eio_then_commit_on_first_read() {
        let Some(fx) = fixture() else { return };
        let (view, path) = fx.new_view();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), fx.content.len() as u64);
        let err = std::fs::read(&path).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
        let err = std::fs::read(&path).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 0);
        assert_eq!(fx.source.bytes_materialized.load(Ordering::Relaxed), 0);
        assert!(fx.mount.drop_performed(view));
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data, fx.content);
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 1);
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data, fx.content);
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 1);
        let stats = fx.mount.stats(view).unwrap();
        assert_eq!(stats.commits, 1);
        assert!(stats.denied_reads >= 2);
        assert!(stats.bytes_served >= fx.content.len() as u64);
    }

    #[test]
    fn read_racing_ahead_of_a_started_drag_waits_for_the_drop_then_commits() {
        let Some(fx) = fixture() else { return };
        let (view, path) = fx.new_view();
        fx.mount.drag_started(view);
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || std::fs::read(&reader_path));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 0);
        assert!(fx.mount.drop_performed(view));
        let data = reader.join().unwrap().unwrap();
        assert_eq!(data, fx.content);
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn read_during_a_drag_that_never_drops_times_out_and_denies() {
        let Some(fx) = fixture() else { return };
        let (view, path) = fx.new_view();
        fx.mount.drag_started(view);
        let err = std::fs::read(&path).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 0);
        assert_eq!(fx.mount.view_state(view), Some(ViewState::Offered));
    }

    #[test]
    fn cancel_after_drop_before_read_sends_zero_payload() {
        let Some(fx) = fixture() else { return };
        let (view, path) = fx.new_view();
        assert!(fx.mount.drop_performed(view));
        assert_eq!(fx.mount.cancel_view(view), CancelOutcome::ZeroPayload);
        let err = std::fs::read(&path).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 0);
        assert_eq!(fx.source.cancels.load(Ordering::Relaxed), 1);
        assert_eq!(fx.source.bytes_materialized.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cancel_while_offered_is_zero_payload() {
        let Some(fx) = fixture() else { return };
        let (view, path) = fx.new_view();
        assert_eq!(fx.mount.cancel_view(view), CancelOutcome::ZeroPayload);
        let err = std::fs::read(&path).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cancel_after_commit_reports_stop_transfer() {
        let Some(fx) = fixture() else { return };
        let (view, path) = fx.new_view();
        assert!(fx.mount.drop_performed(view));
        let _ = std::fs::read(&path).unwrap();
        assert_eq!(fx.mount.cancel_view(view), CancelOutcome::StopTransfer);
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn restored_view_keeps_its_id_and_serves_without_a_drop() {
        let Some(fx) = fixture() else { return };
        let id = ViewId::new();
        assert!(fx.mount.restore_view(id, fx.tree.manifest.clone()));
        assert!(!fx.mount.restore_view(id, fx.tree.manifest.clone()));
        fx.source.register_view(id, &fx.tree);
        assert_eq!(fx.mount.view_state(id), Some(ViewState::Committed));
        let path = fx.mount.view_dir(id).join("root").join(&fx.file_name);
        assert_eq!(std::fs::read(&path).unwrap(), fx.content);
        assert_eq!(fx.source.commits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn open_readers_tracks_file_and_dir_handles() {
        let Some(fx) = fixture() else { return };
        let (view, path) = fx.new_view();
        assert!(fx.mount.drop_performed(view));
        assert_eq!(fx.mount.open_readers(view), 0);
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(fx.mount.open_readers(view), 1);
        let dir = std::fs::File::open(fx.mount.view_dir(view)).unwrap();
        assert_eq!(fx.mount.open_readers(view), 2);
        drop(file);
        drop(dir);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(fx.mount.open_readers(view), 0);
    }

    #[test]
    fn retire_if_unused_refuses_open_readers_and_blocks_later_opens() {
        let Some(fx) = fixture() else { return };
        let (view, path) = fx.new_view();
        let (other, _) = fx.new_view();
        assert!(fx.mount.drop_performed(view));
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(fx.mount.retire_if_unused(&[other, view]), Err(view));
        assert_eq!(fx.mount.view_state(other), Some(ViewState::Offered));
        assert_eq!(fx.mount.view_state(view), Some(ViewState::DroppedAwaitingRead));
        drop(file);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(fx.mount.retire_if_unused(&[other, view, ViewId::new()]), Ok(()));
        assert_eq!(fx.mount.view_state(view), None);
        assert_eq!(fx.mount.view_state(other), None);
        let err = std::fs::File::open(&path).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
        assert_eq!(fx.mount.open_readers(view), 0);
    }

    #[test]
    fn root_lists_view_dirs_and_views_list_roots() {
        let Some(fx) = fixture() else { return };
        let (view, _path) = fx.new_view();
        let entries: Vec<String> = std::fs::read_dir(fx.mount.mountpoint())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![format!("v{view}")]);
        let roots: Vec<String> = std::fs::read_dir(fx.mount.view_dir(view))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(roots, vec!["root".to_string()]);
    }
}
