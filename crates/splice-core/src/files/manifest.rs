#![allow(clippy::unnecessary_cast)]

use super::{
    Cancellation, EntryId, EntryKind, FileTimestamp, LocalSelection, Manifest, ManifestEntry,
    SelectedRoot,
};
use anyhow::{Context, Result};
use splice_proto::files::{MAX_DEPTH, MAX_ENTRIES};
use std::{
    collections::HashSet,
    ffi::CString,
    fs::{File, Metadata},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

pub(super) struct Directory(pub File);

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        anyhow::ensure!(
            fd >= 0,
            "open directory: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }

    pub fn file(&self, path: &Path, create: bool, directory: bool) -> Result<File> {
        let mut parent = self.0.try_clone()?;
        let components: Vec<_> = path.components().collect();
        if components.is_empty() {
            anyhow::ensure!(!create, "empty creation path");
            return Ok(parent);
        }
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                anyhow::bail!("path leaves selected directory");
            };
            let name = CString::new(name.as_bytes())?;
            let last = index + 1 == components.len();
            let mut flags = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
            if !last || directory {
                flags |= libc::O_DIRECTORY;
            }
            flags |= if last && create {
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error()).context("open selected entry");
            }
            parent = unsafe { File::from_raw_fd(fd) };
        }
        Ok(parent)
    }

    pub fn parent(&self, path: &Path) -> Result<(File, CString)> {
        let name = path.file_name().context("missing capability basename")?;
        let parent = self.file(
            path.parent().context("missing capability parent")?,
            false,
            true,
        )?;
        Ok((parent, CString::new(name.as_bytes())?))
    }

    pub fn stat(&self, path: &Path) -> Result<libc::stat> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = if path.as_os_str().is_empty() {
            unsafe { libc::fstat(self.0.as_raw_fd(), &mut stat) }
        } else {
            let (parent, name) = self.parent(path)?;
            unsafe {
                libc::fstatat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            }
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("inspect selected entry");
        }
        Ok(stat)
    }

    pub fn read_link(&self, path: &Path) -> Result<String> {
        let (parent, name) = self.parent(path)?;
        let mut bytes = vec![0; 4097];
        let n = unsafe {
            libc::readlinkat(
                parent.as_raw_fd(),
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        anyhow::ensure!(
            n > 0 && n <= 4096,
            "unsupported symlink target length or changed link"
        );
        bytes.truncate(n as usize);
        String::from_utf8(bytes).context("symlink target is not UTF-8")
    }

    pub fn symlink(&self, path: &Path, target: &str) -> Result<()> {
        let (parent, name) = self.parent(path)?;
        let target = CString::new(target)?;
        anyhow::ensure!(
            unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) } == 0,
            "create selected symlink: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    pub fn mkdir(&self, name: &str) -> Result<Self> {
        let name_c = CString::new(name)?;
        let rc = unsafe { libc::mkdirat(self.0.as_raw_fd(), name_c.as_ptr(), 0o700) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("create private directory");
        }
        Ok(Self(self.file(Path::new(name), false, true)?))
    }

    pub fn names(
        file: &File,
        limit: usize,
        cancel: &Cancellation,
    ) -> Result<Vec<std::ffi::OsString>> {
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let fd = unsafe {
            libc::openat(
                file.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        anyhow::ensure!(
            fd >= 0,
            "open enumeration descriptor: {}",
            std::io::Error::last_os_error()
        );
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(std::io::Error::last_os_error()).context("enumerate selected directory");
        }
        let stream = Stream(stream);
        let mut names = Vec::new();
        loop {
            cancel.check()?;
            #[cfg(target_os = "macos")]
            let errno = unsafe { libc::__error() };
            #[cfg(target_os = "linux")]
            let errno = unsafe { libc::__errno_location() };
            unsafe {
                *errno = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                anyhow::ensure!(
                    unsafe { *errno } == 0,
                    "directory enumeration failed: {}",
                    std::io::Error::last_os_error()
                );
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            anyhow::ensure!(names.len() < limit, "selection exceeds entry limit");
            use std::os::unix::ffi::OsStringExt;
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
        }
        names.sort();
        Ok(names)
    }

    pub fn sync(&self) -> Result<()> {
        Ok(self.0.sync_all()?)
    }

    pub fn publish(&self, name: &str, destination: &Directory) -> Result<()> {
        let name = CString::new(name)?;
        #[cfg(target_os = "linux")]
        let rc = unsafe {
            libc::renameat2(
                self.0.as_raw_fd(),
                name.as_ptr(),
                destination.0.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let rc = unsafe {
            libc::renameatx_np(
                self.0.as_raw_fd(),
                name.as_ptr(),
                destination.0.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("publish without overwriting");
        }
        destination.sync()?;
        self.sync()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    dev: u64,
    ino: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    mode: u32,
}

impl Version {
    fn stat(m: &libc::stat) -> Self {
        Self {
            dev: m.st_dev as u64,
            ino: m.st_ino,
            len: m.st_size as u64,
            modified: (m.st_mtime, m.st_mtime_nsec),
            changed: (m.st_ctime, m.st_ctime_nsec),
            mode: m.st_mode as u32,
        }
    }

    fn of(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
            mode: m.mode(),
        }
    }
}

struct SourceEntry {
    root: usize,
    path: PathBuf,
    version: Version,
}
static SOURCE_HANDLES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_SOURCE_HANDLES: usize = 64;

struct SourceHandle;
impl SourceHandle {
    fn acquire() -> Result<Self> {
        use std::sync::atomic::Ordering;
        SOURCE_HANDLES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_SOURCE_HANDLES).then_some(n + 1)
            })
            .map_err(|_| anyhow::anyhow!("source directory handle budget exceeded"))?;
        Ok(Self)
    }
}
impl Drop for SourceHandle {
    fn drop(&mut self) {
        SOURCE_HANDLES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

pub(super) struct Selection {
    pub manifest: Manifest,
    roots: Vec<Directory>,
    handles: Vec<SourceHandle>,
    entries: Vec<SourceEntry>,
    _access: Option<std::sync::Arc<dyn Send + Sync>>,
}

impl Selection {
    #[cfg(test)]
    pub fn build(paths: Vec<PathBuf>, cancel: &Cancellation) -> Result<Self> {
        Self::build_local(LocalSelection::paths(paths), cancel)
    }

    pub fn build_local(selection: LocalSelection, cancel: &Cancellation) -> Result<Self> {
        selection.validate()?;
        let mut result = Self {
            manifest: Manifest {
                generation: super::random()?,
                entries: vec![],
                total_bytes: 0,
            },
            roots: vec![],
            handles: vec![],
            entries: vec![],
            _access: selection.access,
        };
        let mut names = HashSet::new();
        let mut anchors = std::collections::HashMap::new();
        for selected in selection.roots {
            cancel.check()?;
            let path = match selected {
                SelectedRoot::Path(path) => path,
                SelectedRoot::Open { name, file } => {
                    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
                    anyhow::ensure!(
                        flags >= 0 && flags & libc::O_ACCMODE != libc::O_WRONLY,
                        "selected descriptor is not readable"
                    );
                    #[cfg(target_os = "linux")]
                    anyhow::ensure!(
                        !file.metadata()?.is_file() || flags & libc::O_PATH == 0,
                        "selected file descriptor has no read access"
                    );
                    anyhow::ensure!(
                        names.insert(name.nfc().collect::<String>().to_lowercase()),
                        "colliding source names"
                    );
                    let permit = SourceHandle::acquire()?;
                    let root = result.roots.len();
                    result.roots.push(Directory(file.try_clone()?));
                    result.handles.push(permit);
                    result.walk(root, PathBuf::new(), None, name, cancel, 1)?;
                    continue;
                }
            };
            anyhow::ensure!(path.is_absolute(), "source selection must be absolute");
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .context("source name is not UTF-8")?
                .to_string();
            anyhow::ensure!(
                names.insert(name.nfc().collect::<String>().to_lowercase()),
                "colliding source names"
            );
            let metadata = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "select a symlink's target explicitly"
            );
            let canonical = path.canonicalize()?;
            let parent = canonical
                .parent()
                .context("filesystem root cannot be selected")?;
            let root = if let Some(root) = anchors.get(parent) {
                *root
            } else {
                let permit = SourceHandle::acquire()?;
                let root = result.roots.len();
                result.roots.push(Directory::open(parent)?);
                result.handles.push(permit);
                anchors.insert(parent.to_path_buf(), root);
                root
            };
            let relative = PathBuf::from(canonical.file_name().context("missing source basename")?);
            let first_entry = result.entries.len();
            result.walk(root, relative, None, name, cancel, 1)?;
            anyhow::ensure!(
                result.entries[first_entry].version == Version::of(&metadata),
                "selected root changed during capture"
            );
        }
        result.manifest.validate()?;
        Ok(result)
    }

    fn walk(
        &mut self,
        root: usize,
        path: PathBuf,
        parent: Option<EntryId>,
        name: String,
        cancel: &Cancellation,
        depth: usize,
    ) -> Result<()> {
        cancel.check()?;
        anyhow::ensure!(
            depth <= MAX_DEPTH && self.entries.len() < MAX_ENTRIES,
            "selection exceeds manifest limits"
        );
        anyhow::ensure!(
            splice_proto::files::valid_name(&name),
            "unsupported file name"
        );
        let stat = self.roots[root].stat(&path)?;
        let version = Version::stat(&stat);
        let file_type = stat.st_mode & libc::S_IFMT;
        anyhow::ensure!(
            matches!(file_type, libc::S_IFREG | libc::S_IFDIR | libc::S_IFLNK),
            "special files cannot be offered"
        );
        anyhow::ensure!(
            parent.is_some() || file_type != libc::S_IFLNK,
            "symlink roots cannot be offered"
        );
        let id = EntryId(self.entries.len() as u32);
        let kind = if file_type == libc::S_IFDIR {
            EntryKind::Directory
        } else if file_type == libc::S_IFLNK {
            EntryKind::Symlink {
                target: self.roots[root].read_link(&path)?,
            }
        } else {
            self.manifest.total_bytes = self
                .manifest
                .total_bytes
                .checked_add(version.len)
                .context("selection size overflow")?;
            anyhow::ensure!(
                self.manifest.total_bytes <= splice_proto::files::MAX_TOTAL_BYTES,
                "selection size limit exceeded"
            );
            EntryKind::File { size: version.len }
        };
        self.manifest.entries.push(ManifestEntry {
            id,
            parent,
            name,
            kind,
            mode: if file_type == libc::S_IFLNK {
                0o777
            } else {
                version.mode & 0o777
            },
            modified: FileTimestamp {
                seconds: version.modified.0,
                nanos: u32::try_from(version.modified.1)
                    .context("invalid source modification time")?,
            },
        });
        self.entries.push(SourceEntry {
            root,
            path: path.clone(),
            version: version.clone(),
        });
        if file_type == libc::S_IFDIR {
            let file = self.roots[root].file(&path, false, true)?;
            anyhow::ensure!(
                Version::of(&file.metadata()?) == version,
                "source directory changed during enumeration"
            );
            let children = Directory::names(&file, MAX_ENTRIES - self.entries.len(), cancel)?;
            let mut names = HashSet::new();
            for child in children {
                cancel.check()?;
                let name = child
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("source name is not UTF-8"))?;
                anyhow::ensure!(
                    names.insert(name.nfc().collect::<String>().to_lowercase()),
                    "normalization or case collision"
                );
                let child_path = path.join(&name);
                self.walk(root, child_path, Some(id), name, cancel, depth + 1)?;
            }
        }
        anyhow::ensure!(
            Version::stat(&self.roots[root].stat(&path)?) == version,
            "source tree or link changed during enumeration"
        );
        Ok(())
    }

    pub fn open(&self, id: EntryId) -> Result<File> {
        let entry = self
            .entries
            .get(id.0 as usize)
            .context("unknown source entry")?;
        let file = self.roots[entry.root].file(&entry.path, false, false)?;
        self.verify(id, &file)?;
        Ok(file)
    }

    pub fn verify(&self, id: EntryId, file: &File) -> Result<()> {
        let entry = self
            .entries
            .get(id.0 as usize)
            .context("unknown source entry")?;
        anyhow::ensure!(
            Version::of(&file.metadata()?) == entry.version,
            "source version changed"
        );
        anyhow::ensure!(
            Version::stat(&self.roots[entry.root].stat(&entry.path)?) == entry.version,
            "source path changed"
        );
        Ok(())
    }

    pub fn validate(&self, manifest: &Manifest, cancel: &Cancellation) -> Result<()> {
        for entry in &manifest.entries {
            cancel.check()?;
            if let EntryKind::Symlink { target } = &entry.kind {
                let source = self
                    .entries
                    .get(entry.id.0 as usize)
                    .context("unknown symlink source")?;
                let root = &self.roots[source.root];
                anyhow::ensure!(
                    Version::stat(&root.stat(&source.path)?) == source.version
                        && root.read_link(&source.path)? == *target
                        && Version::stat(&root.stat(&source.path)?) == source.version,
                    "source symlink changed"
                );
            } else {
                self.open(entry.id)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaced_symlink_and_metadata_mutation_invalidate_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"same").unwrap();
        std::fs::write(root.path().join("b"), b"same").unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink("a", &link).unwrap();
        let selection =
            Selection::build(vec![root.path().into()], &Cancellation::default()).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("b", &link).unwrap();
        assert!(selection
            .validate(&selection.manifest, &Cancellation::default())
            .is_err());
        let selection =
            Selection::build(vec![root.path().into()], &Cancellation::default()).unwrap();
        std::fs::set_permissions(
            root.path().join("a"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        assert!(selection
            .validate(&selection.manifest, &Cancellation::default())
            .is_err());
    }

    #[test]
    fn descriptor_directory_survives_portal_path_removal_and_confines_links() {
        use std::os::unix::fs::FileExt;
        use std::sync::Arc;
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("portal");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("data"), b"capability").unwrap();
        std::os::unix::fs::symlink("data", path.join("alias")).unwrap();
        let directory = Arc::new(File::open(&path).unwrap());
        let local = LocalSelection {
            roots: vec![SelectedRoot::Open {
                name: "folder".into(),
                file: directory,
            }],
            access: Some(Arc::new(())),
        };
        let moved = parent.path().join("moved");
        std::fs::rename(&path, &moved).unwrap();
        let selection = Selection::build_local(local.clone(), &Cancellation::default()).unwrap();
        let second = Selection::build_local(local.clone(), &Cancellation::default()).unwrap();
        assert_eq!(selection.manifest.entries.len(), 3);
        assert_eq!(second.manifest.entries.len(), 3);
        let mut bytes = [0; 10];
        assert!(
            matches!(&selection.manifest.entries[1].kind, EntryKind::Symlink { target } if target == "data")
        );
        assert!(selection.open(EntryId(1)).is_err());
        selection
            .open(EntryId(2))
            .unwrap()
            .read_exact_at(&mut bytes, 0)
            .unwrap();
        assert_eq!(&bytes, b"capability");
        std::fs::write(moved.join("data"), b"mutation!!").unwrap();
        assert!(selection
            .validate(&selection.manifest, &Cancellation::default())
            .is_err());
        std::os::unix::fs::symlink("../outside", moved.join("escape")).unwrap();
        assert!(Selection::build_local(local, &Cancellation::default()).is_err());
    }

    #[test]
    fn cancelled_enumeration_releases_native_access() {
        use std::sync::Arc;
        let parent = tempfile::tempdir().unwrap();
        let access = Arc::new(());
        let weak = Arc::downgrade(&access);
        let selection = LocalSelection {
            roots: vec![SelectedRoot::Open {
                name: "folder".into(),
                file: Arc::new(File::open(parent.path()).unwrap()),
            }],
            access: Some(access),
        };
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(Selection::build_local(selection, &cancel).is_err());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn confines_symlinks_and_rejects_special_files_and_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("data"), b"payload").unwrap();
        std::os::unix::fs::symlink("data", root.join("inside")).unwrap();
        let selection = Selection::build(vec![root.clone()], &Cancellation::default()).unwrap();
        assert_eq!(selection.manifest.total_bytes, 7);
        std::os::unix::fs::symlink("..", root.join("outside")).unwrap();
        assert!(Selection::build(vec![root.clone()], &Cancellation::default()).is_err());
        std::fs::remove_file(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(".", root.join("cycle")).unwrap();
        assert!(Selection::build(vec![root.clone()], &Cancellation::default()).is_err());
        std::fs::remove_file(root.join("cycle")).unwrap();
        let fifo = CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(Selection::build(vec![root], &Cancellation::default()).is_err());
    }

    #[test]
    fn source_replacement_and_same_length_mutation_fail_the_version_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        let selection = Selection::build(vec![path.clone()], &Cancellation::default()).unwrap();
        let descriptor = selection.open(EntryId(0)).unwrap();
        std::fs::write(&path, b"modified").unwrap();
        assert!(selection.verify(EntryId(0), &descriptor).is_err());
        let selection = Selection::build(vec![path.clone()], &Cancellation::default()).unwrap();
        let descriptor = selection.open(EntryId(0)).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"modified").unwrap();
        assert!(selection.verify(EntryId(0), &descriptor).is_err());
    }

    #[test]
    fn cancellation_stops_enumeration_without_reading_payload() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        File::create(&file)
            .unwrap()
            .set_len(5 * 1024 * 1024 * 1024)
            .unwrap();
        let selection = Selection::build(vec![file.clone()], &Cancellation::default()).unwrap();
        assert_eq!(selection.manifest.total_bytes, 5 * 1024 * 1024 * 1024);
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(Selection::build(vec![file], &cancellation).is_err());
    }

    #[test]
    fn directory_capabilities_never_follow_replaced_parents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file"), b"original").unwrap();
        let selection = Selection::build(vec![root.clone()], &Cancellation::default()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("file"), b"outside!").unwrap();
        std::fs::rename(&root, dir.path().join("old")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        assert!(selection.open(EntryId(1)).is_err());
    }
}
