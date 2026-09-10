#![allow(clippy::unnecessary_cast)]

use super::{
    manifest::Directory, Cancellation, EntryId, EntryKind, Manifest, ReceiveDestination,
    TransferRecord, TransferState,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) const CACHE_QUOTA: u64 = 64 * 1024 * 1024 * 1024;
pub(super) const MAX_RECORDS: usize = 256;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Journal {
    pub record: TransferRecord,
    pub destination: ReceiveDestination,
    pub manifest: Manifest,
    pub directory: PathBuf,
    pub staging: String,
    roots: Vec<PublishedRoot>,
    #[serde(default)]
    stage_identity: Option<(u64, u64)>,
    #[serde(default)]
    clearing: bool,
    #[serde(default)]
    claim: Option<Box<Claim>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Claim {
    original: PathBuf,
    holder: PathBuf,
    name: String,
    dev: u64,
    ino: u64,
}

impl Claim {
    fn location(&self) -> PathBuf {
        self.holder.join(&self.name)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PublishedRoot {
    name: String,
    dev: u64,
    ino: u64,
}

pub(super) struct Recovery {
    pub journals: Vec<Journal>,
    pub errors: Vec<super::RecoveryIssue>,
    pub error_count: usize,
    pub untracked_cache_bytes: u64,
    pub cleanup_failures: std::collections::HashSet<super::TransferId>,
}

impl Recovery {
    fn report(&mut self, transfer: Option<super::TransferId>, message: String) {
        self.error_count += 1;
        if self.errors.len() < 32 {
            self.errors.push(super::RecoveryIssue {
                transfer,
                message: super::bounded_error(&message, 1024),
            });
        }
    }
}

fn receipt_id(path: &Path) -> Option<super::TransferId> {
    let name = path.file_name()?.to_str()?.split('.').next()?;
    if name.len() != 32 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut id = [0; 16];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&name[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(super::TransferId(id))
}

fn load_journal(base: &Path, path: &Path) -> Result<Journal> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    anyhow::ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 2 * 1024 * 1024,
        "invalid or oversized file recovery journal"
    );
    let mut bytes = Vec::new();
    file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let journal: Journal =
        serde_json::from_slice(&bytes).context("corrupt or unsupported file recovery journal")?;
    journal.manifest.validate()?;
    anyhow::ensure!(
        path.file_name().and_then(|n| n.to_str())
            == Some(&format!("{}.json", super::hex(&journal.record.id.0))),
        "file journal identity mismatch"
    );
    anyhow::ensure!(
        journal.directory.is_absolute()
            && journal.staging == format!(".splice-{}.partial", super::hex(&journal.record.id.0)),
        "invalid receipt storage location"
    );
    if let Some(claim) = &journal.claim {
        anyhow::ensure!(
            claim.original.is_absolute()
                && claim.holder.is_absolute()
                && claim.holder.parent() == claim.original.parent()
                && claim.holder.file_name().and_then(|n| n.to_str())
                    == Some(&holder_name(journal.record.id))
                && claim.name.len() == 32
                && claim.name.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid claimed cleanup record"
        );
    }
    if journal.destination == ReceiveDestination::Cache {
        let expected = base.join("received").join(super::hex(&journal.record.id.0));
        anyhow::ensure!(
            journal.directory == expected
                || journal.claim.as_ref().is_some_and(
                    |claim| claim.original == expected && claim.location() == journal.directory
                ),
            "invalid cache receipt location"
        );
    }
    anyhow::ensure!(
        journal.record.direction == super::TransferDirection::Receive
            && journal.record.total_bytes == journal.manifest.total_bytes
            && journal.record.bytes <= journal.record.total_bytes,
        "invalid receive journal progress"
    );
    anyhow::ensure!(
        journal.roots.len() <= splice_proto::files::MAX_ROOTS
            && journal
                .roots
                .iter()
                .map(|root| &root.name)
                .collect::<std::collections::HashSet<_>>()
                .len()
                == journal.roots.len()
            && journal.roots.iter().all(|root| journal
                .manifest
                .entries
                .iter()
                .any(|entry| entry.parent.is_none() && entry.name == root.name)),
        "invalid published root identity"
    );
    anyhow::ensure!(
        journal.record.paths.len() <= journal.roots.len()
            && journal
                .record
                .paths
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == journal.record.paths.len()
            && journal.record.paths.iter().all(|path| journal
                .roots
                .iter()
                .any(|root| *path == journal.directory.join(&root.name))),
        "invalid published receipt path"
    );
    Ok(journal)
}

fn recover_journal(base: &Path, journal: &mut Journal) -> Result<bool> {
    if journal.clearing {
        clear_loaded(base, journal, &journal_path(base, journal.record.id))?;
        return Ok(false);
    }
    if !journal.record.state.terminal() {
        let journaled: std::collections::HashSet<PathBuf> =
            journal.record.paths.drain(..).collect();
        let mut incomplete = None;
        if let Ok(destination) = Directory::open(&journal.directory) {
            for root in &journal.roots {
                if let Ok(m) = destination.stat(Path::new(&root.name)) {
                    if m.st_dev as u64 == root.dev && m.st_ino == root.ino {
                        let path = journal.directory.join(&root.name);
                        if !journaled.contains(&path) {
                            if let Err(error) =
                                complete_root_metadata(&destination, &journal.manifest, root)
                            {
                                incomplete = Some(error);
                            }
                        }
                        journal.record.paths.push(path);
                    }
                }
            }
        }
        if !journal.roots.is_empty()
            && journal.record.paths.len() == journal.roots.len()
            && incomplete.is_none()
        {
            journal.record.state = TransferState::Ready;
            journal.record.bytes = journal.record.total_bytes;
            journal.record.error = None;
        } else {
            journal.record.state = TransferState::Failed;
            journal.record.error = Some(match incomplete {
                Some(error) => format!(
                    "service stopped before published directory metadata was recorded: {error:#}; explicit Retry required"
                ),
                None => "service stopped before receive completed; explicit Retry required".into(),
            });
        }
        save(base, journal)?;
    }
    cleanup_staging(base, journal)?;
    Ok(true)
}

fn complete_root_metadata(
    destination: &Directory,
    manifest: &Manifest,
    root: &PublishedRoot,
) -> Result<()> {
    let entry = manifest
        .entries
        .iter()
        .find(|entry| entry.parent.is_none() && entry.name == root.name)
        .context("missing published root entry")?;
    if entry.kind != EntryKind::Directory {
        return Ok(());
    }
    let file = destination.file(Path::new(&root.name), false, true)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.dev() == root.dev && metadata.ino() == root.ino,
        "published root replaced"
    );
    set_metadata(&file, entry.mode, &entry.modified)
}

pub(super) fn initialize(base: &Path) -> Result<Recovery> {
    std::fs::create_dir_all(base)?;
    let metadata = std::fs::symlink_metadata(base)?;
    anyhow::ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() },
        "file cache is not an owned directory"
    );
    std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700))?;
    let dir = Directory::open(base)?;
    for name in ["records", "received"] {
        if !base.join(name).try_exists()? {
            dir.mkdir(name)?;
        }
        let child = dir.file(Path::new(name), false, true)?;
        anyhow::ensure!(
            child.metadata()?.uid() == unsafe { libc::geteuid() },
            "cache directory owner mismatch"
        );
        child.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    let mut recovery = Recovery {
        journals: vec![],
        errors: vec![],
        error_count: 0,
        untracked_cache_bytes: 0,
        cleanup_failures: std::collections::HashSet::new(),
    };
    let (records, truncated) = bounded_entries(&base.join("records"))?;
    for entry in records {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                recovery.report(None, format!("read receipt directory: {error}"));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "bad") {
            recovery.report(
                receipt_id(&path),
                format!(
                    "Quarantined receipt journal {}; data retained",
                    entry.file_name().to_string_lossy()
                ),
            );
            continue;
        }
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let mut journal = match load_journal(base, &path) {
            Ok(journal) => journal,
            Err(error) => {
                let quarantine = path
                    .with_extension(format!("json.{}.bad", super::hex(&super::random::<16>()?)));
                let result = std::fs::rename(&path, &quarantine)
                    .and_then(|_| std::fs::File::open(base.join("records"))?.sync_all());
                recovery.report(
                    receipt_id(&path),
                    match result {
                        Ok(()) => format!(
                            "Quarantined receipt {}: {error:#}; data retained",
                            quarantine.file_name().unwrap().to_string_lossy()
                        ),
                        Err(rename) => format!(
                            "Cannot quarantine receipt {}: {error:#}; {rename}; data retained",
                            entry.file_name().to_string_lossy()
                        ),
                    },
                );
                continue;
            }
        };
        let result = match recover_journal(base, &mut journal) {
            Ok(true) if recovery.journals.len() >= 128 && journal.record.paths.is_empty() => {
                clear_loaded(base, &mut journal, &path).map(|()| false)
            }
            result => result,
        };
        match result {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => {
                let message = super::bounded_error(&format!("receipt recovery: {error:#}"), 1024);
                journal.clearing = false;
                recovery.cleanup_failures.insert(journal.record.id);
                if !journal.record.state.terminal() {
                    journal.record.state = TransferState::Failed;
                }
                journal.record.error = Some(message.clone());
                recovery.report(Some(journal.record.id), message);
                if let Err(error) = save(base, &journal) {
                    recovery.report(
                        Some(journal.record.id),
                        format!("cannot persist receipt diagnostic: {error:#}"),
                    );
                }
            }
        }
        if recovery.journals.len() >= MAX_RECORDS {
            recovery.report(
                Some(journal.record.id),
                "retained receipt view limit reached; journal and received data remain on disk"
                    .into(),
            );
            continue;
        }
        recovery.journals.push(journal);
    }
    if truncated {
        recovery.report(
            None,
            "receipt scan limit reached; remaining journals retained on disk".into(),
        );
    }
    let tracked: std::collections::HashSet<_> = recovery
        .journals
        .iter()
        .filter(|journal| journal.destination == ReceiveDestination::Cache)
        .flat_map(|journal| {
            std::iter::once(journal.directory.clone())
                .chain(journal.claim.iter().map(|claim| claim.holder.clone()))
        })
        .collect();
    let mut remaining = MAX_RECORDS * splice_proto::files::MAX_ENTRIES;
    let received = Directory::open(&base.join("received"))?;
    let (entries, truncated) = bounded_entries(&base.join("received"))?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                recovery.untracked_cache_bytes = CACHE_QUOTA;
                recovery.report(None, format!("inspect cache directory: {error}"));
                continue;
            }
        };
        if tracked.contains(&entry.path()) {
            continue;
        }
        let id = receipt_id(&entry.path());
        let result = (|| -> Result<u64> {
            let name = CString::new(entry.file_name().as_bytes())?;
            let directory = owned_directory(&received.0, &name, None)?;
            let (bytes, empty) = inventory_cache(&directory, &mut remaining, 0)?;
            if empty {
                remove_at(&received.0, &name, true)?;
                received.sync()?;
            } else {
                recovery.report(
                    id,
                    "untracked cache data retained; storage reserved until manual recovery".into(),
                );
            }
            Ok(bytes)
        })();
        match result {
            Ok(bytes) => {
                recovery.untracked_cache_bytes =
                    recovery.untracked_cache_bytes.saturating_add(bytes)
            }
            Err(error) => {
                recovery.untracked_cache_bytes = CACHE_QUOTA;
                recovery.report(
                    id,
                    format!(
                        "cannot inventory retained cache data: {error:#}; Save remains available"
                    ),
                );
            }
        }
    }
    if truncated {
        recovery.untracked_cache_bytes = CACHE_QUOTA;
        recovery.report(None, "cache inventory limit exceeded; Save remains available, cache receives require cleanup".into());
    }
    Ok(recovery)
}

type BoundedEntries = (Vec<std::io::Result<std::fs::DirEntry>>, bool);

fn bounded_entries(path: &Path) -> Result<BoundedEntries> {
    let mut entries: Vec<_> = std::fs::read_dir(path)?.take(MAX_RECORDS * 4 + 1).collect();
    let truncated = entries.len() > MAX_RECORDS * 4;
    entries.truncate(MAX_RECORDS * 4);
    Ok((entries, truncated))
}

fn stat_at(parent: &File, name: &CStr) -> std::io::Result<libc::stat> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat)
}

fn remove_at(parent: &File, name: &CStr, directory: bool) -> Result<()> {
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    anyhow::ensure!(
        unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } == 0,
        "remove {}: {}",
        name.to_string_lossy(),
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn rename_noreplace(from: &File, name: &CStr, to: &File, target: &CStr) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            name.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            name.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn verify_owned(file: &File, expected: Option<(u64, u64)>) -> Result<std::fs::Metadata> {
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
        "not an owned directory"
    );
    if let Some((dev, ino)) = expected {
        anyhow::ensure!(
            metadata.dev() == dev && metadata.ino() == ino,
            "owned directory identity changed"
        );
    }
    Ok(metadata)
}

fn restore_access(file: &File, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.mode() & 0o700 != 0o700 {
        file.set_permissions(std::fs::Permissions::from_mode(
            (metadata.mode() & 0o777) | 0o700,
        ))
        .context("restore owned directory access")?;
    }
    Ok(())
}

fn open_directory_at(parent: &File, name: &CStr, flags: libc::c_int) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn owned_directory(parent: &File, name: &CStr, expected: Option<(u64, u64)>) -> Result<File> {
    match open_directory_at(parent, name, libc::O_RDONLY) {
        Ok(file) => {
            let metadata = verify_owned(&file, expected)
                .with_context(|| format!("{}", name.to_string_lossy()))?;
            restore_access(&file, &metadata)?;
            Ok(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            unreadable_owned_directory(parent, name, expected)
        }
        Err(error) => Err(error).with_context(|| format!("open {}", name.to_string_lossy())),
    }
}

#[cfg(target_os = "linux")]
fn unreadable_owned_directory(
    parent: &File,
    name: &CStr,
    expected: Option<(u64, u64)>,
) -> Result<File> {
    let handle = open_directory_at(parent, name, libc::O_PATH)
        .with_context(|| format!("open {}", name.to_string_lossy()))?;
    let metadata =
        verify_owned(&handle, expected).with_context(|| format!("{}", name.to_string_lossy()))?;
    let proc_path = CString::new(format!("/proc/self/fd/{}", handle.as_raw_fd()))?;
    anyhow::ensure!(
        unsafe {
            libc::chmod(
                proc_path.as_ptr(),
                ((metadata.mode() & 0o777) | 0o700) as libc::mode_t,
            )
        } == 0,
        "restore owned directory access: {}",
        std::io::Error::last_os_error()
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(Path::new(proc_path.to_str()?))?;
    verify_owned(&file, Some((metadata.dev(), metadata.ino())))?;
    Ok(file)
}

#[cfg(target_os = "macos")]
fn unreadable_owned_directory(
    _parent: &File,
    name: &CStr,
    _expected: Option<(u64, u64)>,
) -> Result<File> {
    anyhow::bail!(
        "owned directory {} is not readable; restore owner read permission manually before retrying",
        name.to_string_lossy()
    )
}

fn cleanup_budget() -> usize {
    splice_proto::files::MAX_ENTRIES + splice_proto::files::MAX_ROOTS + 1
}

fn remove_contents(directory: &File, remaining: &mut usize, depth: usize) -> Result<()> {
    anyhow::ensure!(
        depth <= splice_proto::files::MAX_DEPTH + 2,
        "cleanup depth limit reached"
    );
    for child in Directory::names(directory, *remaining, &Cancellation::default())? {
        anyhow::ensure!(*remaining > 0, "cleanup entry limit reached");
        *remaining -= 1;
        let child = CString::new(child.as_bytes())?;
        let stat = stat_at(directory, &child).context("inspect cleanup entry")?;
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let nested = owned_directory(directory, &child, None)?;
            remove_contents(&nested, remaining, depth + 1)?;
            remove_at(directory, &child, true)?;
        } else {
            remove_at(directory, &child, false)?;
        }
    }
    Ok(())
}

fn holder_name(id: super::TransferId) -> String {
    format!(".splice-{}.cleanup", super::hex(&id.0))
}

fn claim_and_remove(
    base: &Path,
    journal: &mut Journal,
    parent: &File,
    parent_path: &Path,
    name: &CStr,
    directory: &File,
) -> Result<()> {
    let metadata = verify_owned(directory, None)?;
    restore_access(directory, &metadata)?;
    let holder_name = holder_name(journal.record.id);
    let holder_c = CString::new(holder_name.as_str())?;
    let holder = open_holder(parent, &holder_c)?;
    let claimed = super::hex(&super::random::<16>()?);
    let claimed_c = CString::new(claimed.as_str())?;
    journal.claim = Some(Box::new(Claim {
        original: parent_path.join(name.to_str()?),
        holder: parent_path.join(&holder_name),
        name: claimed,
        dev: metadata.dev(),
        ino: metadata.ino(),
    }));
    save(base, journal)?;
    if !move_into_holder(
        parent,
        name,
        &holder,
        &holder_c,
        &claimed_c,
        (metadata.dev(), metadata.ino()),
    )? {
        journal.claim = None;
        save(base, journal)?;
        anyhow::bail!(
            "{} was replaced before removal; replacement restored",
            name.to_string_lossy()
        );
    }
    finish_claim(
        base, journal, parent, &holder_c, &holder, &claimed_c, directory,
    )
}

fn move_into_holder(
    parent: &File,
    name: &CStr,
    holder: &File,
    holder_name: &CStr,
    claimed: &CStr,
    identity: (u64, u64),
) -> Result<bool> {
    rename_noreplace(parent, name, holder, claimed)
        .with_context(|| format!("claim {}", name.to_string_lossy()))?;
    let stat = stat_at(holder, claimed).context("inspect claimed entry")?;
    if (stat.st_dev as u64, stat.st_ino) == identity {
        return Ok(true);
    }
    if rename_noreplace(holder, claimed, parent, name).is_ok() {
        remove_at(parent, holder_name, true)?;
        return Ok(false);
    }
    anyhow::bail!(
        "{} was replaced before removal; replacement preserved at {}/{}",
        name.to_string_lossy(),
        holder_name.to_string_lossy(),
        claimed.to_string_lossy()
    )
}

fn open_holder(parent: &File, holder_name: &CStr) -> Result<File> {
    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), holder_name.as_ptr(), 0o700) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.kind() == std::io::ErrorKind::AlreadyExists,
            "create cleanup holder: {error}"
        );
    }
    let holder =
        open_directory_at(parent, holder_name, libc::O_RDONLY).context("open cleanup holder")?;
    verify_owned(&holder, None).context("cleanup holder")?;
    holder.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    Ok(holder)
}

fn finish_claim(
    base: &Path,
    journal: &mut Journal,
    holder_parent: &File,
    holder_name: &CStr,
    holder: &File,
    claimed: &CStr,
    directory: &File,
) -> Result<()> {
    let removed = remove_contents(directory, &mut cleanup_budget(), 0)
        .and_then(|()| remove_at(holder, claimed, true))
        .and_then(|()| remove_at(holder_parent, holder_name, true));
    let claim = journal.claim.clone().context("missing claim record")?;
    match removed {
        Ok(()) => {
            holder_parent.sync_all()?;
            complete_claim(base, journal)
        }
        Err(error) => {
            if journal.destination == ReceiveDestination::Cache
                && claim.original == base.join("received").join(super::hex(&journal.record.id.0))
            {
                journal.directory = claim.location();
                journal.record.paths = journal
                    .roots
                    .iter()
                    .filter(|root| {
                        CString::new(root.name.as_str())
                            .ok()
                            .and_then(|name| stat_at(directory, &name).ok())
                            .is_some_and(|stat| {
                                stat.st_dev as u64 == root.dev && stat.st_ino == root.ino
                            })
                    })
                    .map(|root| journal.directory.join(&root.name))
                    .collect();
                save(base, journal)?;
            }
            Err(error.context(format!(
                "remaining data retained at {}",
                claim.location().display()
            )))
        }
    }
}

fn complete_claim(base: &Path, journal: &mut Journal) -> Result<()> {
    if let Some(claim) = journal.claim.take() {
        if journal.directory == claim.location() {
            journal.directory = claim.original;
            journal.record.paths.clear();
        }
    }
    save(base, journal)
}

fn resume_claim(base: &Path, journal: &mut Journal, original: &Path) -> Result<bool> {
    let Some(claim) = journal.claim.clone() else {
        return Ok(false);
    };
    if claim.original != original {
        return Ok(false);
    }
    let holder_parent_path = claim
        .holder
        .parent()
        .context("claim holder has no parent")?;
    let holder_parent = Directory::open(holder_parent_path)
        .context("cleanup parent is unavailable; claim retained for recovery")?;
    let holder_name = CString::new(
        claim
            .holder
            .file_name()
            .and_then(|n| n.to_str())
            .context("invalid claim holder")?,
    )?;
    let original_name = CString::new(
        claim
            .original
            .file_name()
            .and_then(|n| n.to_str())
            .context("invalid claim original")?,
    )?;
    let identity = (claim.dev, claim.ino);
    let claimed = CString::new(claim.name.as_str())?;
    let holder = match open_directory_at(&holder_parent.0, &holder_name, libc::O_RDONLY) {
        Ok(holder) => {
            verify_owned(&holder, None).context("cleanup holder")?;
            Some(holder)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("open cleanup holder"),
    };
    let claimed_stat = match &holder {
        Some(holder) => match stat_at(holder, &claimed) {
            Ok(stat) => Some(stat),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("inspect claimed entry"),
        },
        None => None,
    };
    let (holder, directory) = match (holder, claimed_stat) {
        (Some(holder), Some(stat)) => {
            anyhow::ensure!(
                (stat.st_dev as u64, stat.st_ino) == identity,
                "claimed entry {} was replaced; preserved for inspection",
                claim.location().display()
            );
            let directory = owned_directory(&holder, &claimed, Some(identity))?;
            (holder, directory)
        }
        (holder, _) => match stat_at(&holder_parent.0, &original_name) {
            Ok(stat) => {
                anyhow::ensure!(
                    (stat.st_dev as u64, stat.st_ino) == identity,
                    "claimed entry {} is missing and {} was replaced; preserved for inspection",
                    claim.location().display(),
                    claim.original.display()
                );
                let directory = owned_directory(&holder_parent.0, &original_name, Some(identity))?;
                let holder = match holder {
                    Some(holder) => holder,
                    None => open_holder(&holder_parent.0, &holder_name)?,
                };
                if !move_into_holder(
                    &holder_parent.0,
                    &original_name,
                    &holder,
                    &holder_name,
                    &claimed,
                    identity,
                )? {
                    journal.claim = None;
                    save(base, journal)?;
                    anyhow::bail!(
                        "{} was replaced before removal; replacement restored",
                        claim.original.display()
                    );
                }
                (holder, directory)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if holder.is_some() {
                    remove_at(&holder_parent.0, &holder_name, true)?;
                    holder_parent.sync()?;
                }
                complete_claim(base, journal)?;
                return Ok(true);
            }
            Err(error) => return Err(error).context("inspect claimed original"),
        },
    };
    finish_claim(
        base,
        journal,
        &holder_parent.0,
        &holder_name,
        &holder,
        &claimed,
        &directory,
    )?;
    Ok(true)
}

fn inventory_cache(directory: &File, remaining: &mut usize, depth: usize) -> Result<(u64, bool)> {
    anyhow::ensure!(
        depth <= splice_proto::files::MAX_DEPTH + 2,
        "cache inventory depth limit reached"
    );
    let mut bytes = 0u64;
    let names = Directory::names(directory, *remaining, &Cancellation::default())?;
    let empty = names.is_empty();
    for name in names {
        anyhow::ensure!(*remaining > 0, "cache inventory entry limit reached");
        *remaining -= 1;
        let name = CString::new(name.as_bytes())?;
        let stat = stat_at(directory, &name).context("inspect cache entry")?;
        let length = if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            inventory_cache(
                &owned_directory(directory, &name, None)?,
                remaining,
                depth + 1,
            )?
            .0
        } else if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
            stat.st_size.max(0) as u64
        } else {
            0
        };
        bytes = bytes
            .checked_add(length)
            .context("cache inventory size overflow")?;
    }
    Ok((bytes, empty))
}

fn cleanup_staging(base: &Path, journal: &mut Journal) -> Result<()> {
    anyhow::ensure!(
        journal.staging == format!(".splice-{}.partial", super::hex(&journal.record.id.0)),
        "invalid staging identity"
    );
    let path = journal.directory.join(&journal.staging);
    if resume_claim(base, journal, &path)? {
        return Ok(());
    }
    if let Some((dev, ino)) = journal.stage_identity {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_dir() && metadata.dev() == dev && metadata.ino() == ino,
                    "recovery staging identity changed"
                );
                let parent_path = journal.directory.clone();
                let directory = Directory::open(&parent_path)?;
                let name = CString::new(journal.staging.as_str())?;
                let stage = owned_directory(&directory.0, &name, Some((dev, ino)))?;
                claim_and_remove(base, journal, &directory.0, &parent_path, &name, &stage)?;
                directory.sync()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    } else {
        remove_empty_intent_stage(journal)?;
    }
    Ok(())
}

fn remove_empty_intent_stage(journal: &Journal) -> Result<()> {
    let path = journal.directory.join(&journal.staging);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o777 == 0o700,
                "unrecognized incomplete staging directory"
            );
            std::fs::remove_dir(path).context("incomplete staging intent is not empty")?;
            Directory::open(&journal.directory)?.sync()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct ClearFailure {
    pub record: Option<Box<TransferRecord>>,
    pub error: anyhow::Error,
}

fn journal_path(base: &Path, transfer: super::TransferId) -> PathBuf {
    base.join("records")
        .join(format!("{}.json", super::hex(&transfer.0)))
}

pub(super) fn clear_received(base: &Path, transfer: super::TransferId) -> Result<(), ClearFailure> {
    let path = journal_path(base, transfer);
    let unloaded = |error| ClearFailure {
        record: None,
        error,
    };
    let mut journal = match load_journal(base, &path) {
        Ok(journal) => journal,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            let directory = base.join("received").join(super::hex(&transfer.0));
            return match std::fs::remove_dir(directory) {
                Ok(()) => Directory::open(&base.join("received"))
                    .and_then(|received| received.sync())
                    .map_err(unloaded),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(unloaded(error.into())),
            };
        }
        Err(error) => return Err(unloaded(error)),
    };
    clear_loaded(base, &mut journal, &path).map_err(|error| ClearFailure {
        record: Some(Box::new(journal.record.clone())),
        error,
    })
}

fn clear_loaded(base: &Path, journal: &mut Journal, path: &Path) -> Result<()> {
    let result = clear_journal(base, journal, path);
    if let Err(error) = &result {
        journal.clearing = false;
        journal.record.error = Some(super::bounded_error(
            &format!("clear failed: {error:#}"),
            1024,
        ));
        if let Err(persist) = save(base, journal) {
            return Err(anyhow::anyhow!(
                "{error:#}; cannot persist failed-clear state: {persist:#}"
            ));
        }
    }
    result
}

fn clear_journal(base: &Path, journal: &mut Journal, path: &Path) -> Result<()> {
    let receipt_dir = base.join("received").join(super::hex(&journal.record.id.0));
    let mut receipt = None;
    if journal.destination == ReceiveDestination::Cache {
        let relocated = journal
            .claim
            .as_ref()
            .filter(|claim| claim.original == receipt_dir && claim.location() == journal.directory);
        anyhow::ensure!(
            journal.directory == receipt_dir || relocated.is_some(),
            "cache journal directory mismatch"
        );
        if journal.directory.try_exists()? {
            let root = Directory::open(&journal.directory)?;
            if let Some(claim) = relocated {
                let metadata = root.0.metadata()?;
                anyhow::ensure!(
                    metadata.dev() == claim.dev && metadata.ino() == claim.ino,
                    "relocated receipt identity changed; retain ownership for inspection"
                );
            }
            let mut remaining = cleanup_budget();
            preflight_clear(&root, &mut remaining)?;
            for published in &journal.roots {
                let stat = match root.stat(Path::new(&published.name)) {
                    Ok(stat) => stat,
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        continue
                    }
                    Err(error) => return Err(error),
                };
                anyhow::ensure!(
                    stat.st_dev as u64 == published.dev && stat.st_ino == published.ino,
                    "published receipt identity changed; retain ownership for inspection"
                );
            }
            receipt = Some(root);
        }
    }
    journal.clearing = true;
    save(base, journal)?;
    cleanup_staging(base, journal)?;
    if journal.destination == ReceiveDestination::Cache
        && !resume_claim(base, journal, &receipt_dir)?
    {
        if let Some(root) = receipt {
            let received_path = base.join("received");
            let received = Directory::open(&received_path)?;
            let name = CString::new(super::hex(&journal.record.id.0))?;
            claim_and_remove(base, journal, &received.0, &received_path, &name, &root.0)?;
            received.sync()?;
        }
    }
    std::fs::remove_file(path)?;
    Directory::open(&base.join("records"))?.sync()?;
    Ok(())
}

fn preflight_clear(directory: &Directory, remaining: &mut usize) -> Result<()> {
    let metadata = directory.0.metadata()?;
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "receipt directory is not owned"
    );
    if metadata.mode() & 0o500 != 0o500 {
        return Ok(());
    }
    let names = Directory::names(&directory.0, *remaining, &Cancellation::default())?;
    for name in names {
        anyhow::ensure!(*remaining > 0, "receipt cleanup entry limit reached");
        *remaining -= 1;
        let path = Path::new(&name);
        let stat = directory.stat(path)?;
        #[cfg(target_os = "macos")]
        anyhow::ensure!(
            stat.st_flags
                & (libc::UF_IMMUTABLE | libc::SF_IMMUTABLE | libc::UF_APPEND | libc::SF_APPEND)
                == 0,
            "receipt contains a locked or append-only entry"
        );
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            match directory.file(path, false, true) {
                Ok(child) => preflight_clear(&Directory(child), remaining)?,
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        error.kind() == std::io::ErrorKind::PermissionDenied
                    }) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

pub(super) fn save(base: &Path, journal: &Journal) -> Result<()> {
    let path = base
        .join("records")
        .join(format!("{}.json", super::hex(&journal.record.id.0)));
    let tmp = path.with_extension(format!("{}.tmp", super::hex(&super::random::<16>()?)));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(journal)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Directory::open(&base.join("records"))?.sync()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

fn timespecs(modified: &super::FileTimestamp) -> [libc::timespec; 2] {
    [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: modified.seconds,
            tv_nsec: modified.nanos.into(),
        },
    ]
}

fn set_metadata(file: &File, mode: u32, modified: &super::FileTimestamp) -> Result<()> {
    file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))?;
    anyhow::ensure!(
        unsafe { libc::futimens(file.as_raw_fd(), timespecs(modified).as_ptr()) } == 0,
        "set modification time: {}",
        std::io::Error::last_os_error()
    );
    Ok(file.sync_all()?)
}

fn apply_metadata(stage: &Directory, path: &Path, entry: &super::ManifestEntry) -> Result<()> {
    let times = timespecs(&entry.modified);
    if matches!(entry.kind, EntryKind::Symlink { .. }) {
        let (parent, name) = stage.parent(path)?;
        #[cfg(target_os = "macos")]
        anyhow::ensure!(
            unsafe {
                libc::fchmodat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    entry.mode as libc::mode_t,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0,
            "set symlink mode: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            unsafe {
                libc::utimensat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    times.as_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0,
            "set symlink modification time: {}",
            std::io::Error::last_os_error()
        );
    } else {
        let file = stage.file(path, false, entry.kind == EntryKind::Directory)?;
        set_metadata(&file, entry.mode, &entry.modified)?;
    }
    Ok(())
}

pub(super) struct Staging {
    pub journal: Journal,
    base: PathBuf,
    destination: Directory,
    stage: Arc<Directory>,
    relative: HashMap<EntryId, PathBuf>,
}

impl Staging {
    pub fn new(
        base: PathBuf,
        record: TransferRecord,
        destination: ReceiveDestination,
        manifest: Manifest,
        cancel: &Cancellation,
    ) -> Result<Self> {
        manifest.validate()?;
        cancel.check()?;
        let id = super::hex(&record.id.0);
        let directory = match &destination {
            ReceiveDestination::Directory(path) => {
                anyhow::ensure!(path.is_absolute(), "receive directory must be absolute");
                let canonical = path.canonicalize()?;
                Directory::open(&canonical)?;
                canonical
            }
            ReceiveDestination::Cache => {
                let parent = Directory::open(&base.join("received"))?;
                parent.mkdir(&id)?;
                parent.sync()?;
                base.join("received").join(&id)
            }
        };
        let dest = Directory::open(&directory)?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        anyhow::ensure!(
            unsafe { libc::fstatvfs(dest.0.as_raw_fd(), &mut stat) } == 0,
            "cannot inspect destination space"
        );
        let available = (stat.f_bavail as u128) * (stat.f_frsize as u128);
        anyhow::ensure!(
            available >= u128::from(manifest.total_bytes) + 16 * 1024 * 1024,
            "insufficient destination disk space"
        );
        let staging = format!(".splice-{id}.partial");
        let mut journal = Journal {
            record,
            destination,
            manifest,
            directory,
            staging: staging.clone(),
            roots: vec![],
            stage_identity: None,
            clearing: false,
            claim: None,
        };
        save(&base, &journal)?;
        let stage = dest.mkdir(&staging)?;
        dest.sync()?;
        let identity = stage.0.metadata()?;
        journal.stage_identity = Some((identity.dev(), identity.ino()));
        save(&base, &journal)?;
        let mut result = Self {
            journal,
            base,
            destination: dest,
            stage: Arc::new(stage),
            relative: HashMap::new(),
        };
        for entry in &result.journal.manifest.entries {
            cancel.check()?;
            let parent = match entry.parent {
                Some(id) => Directory(
                    result.stage.file(
                        result
                            .relative
                            .get(&id)
                            .context("missing destination parent")?,
                        false,
                        true,
                    )?,
                ),
                None => Directory(result.stage.0.try_clone()?),
            };
            let relative = match entry.parent {
                Some(id) => result
                    .relative
                    .get(&id)
                    .context("missing relative parent")?
                    .join(&entry.name),
                None => PathBuf::from(&entry.name),
            };
            if entry.kind == EntryKind::Directory {
                parent.mkdir(&entry.name)?;
            }
            result.relative.insert(entry.id, relative);
        }
        Ok(result)
    }

    pub async fn create_file(&self, id: EntryId) -> Result<File> {
        let stage = self.stage.clone();
        let path = self
            .relative
            .get(&id)
            .context("unknown destination entry")?
            .clone();
        tokio::task::spawn_blocking(move || stage.file(&path, true, false)).await?
    }

    pub fn complete(&mut self, cancel: &Cancellation) -> Result<Vec<PathBuf>> {
        cancel.check()?;
        for entry in &self.journal.manifest.entries {
            cancel.check()?;
            if let EntryKind::Symlink { target } = &entry.kind {
                self.stage.symlink(
                    self.relative
                        .get(&entry.id)
                        .context("missing symlink path")?,
                    target,
                )?;
            }
        }
        for entry in self.journal.manifest.entries.iter().rev() {
            cancel.check()?;
            if entry.parent.is_none() && entry.kind == EntryKind::Directory {
                continue;
            }
            apply_metadata(
                &self.stage,
                self.relative
                    .get(&entry.id)
                    .context("missing metadata path")?,
                entry,
            )?;
        }
        self.stage.sync()?;
        self.journal.record.state = TransferState::Verifying;
        for entry in self
            .journal
            .manifest
            .entries
            .iter()
            .filter(|e| e.parent.is_none())
        {
            let metadata = self.stage.stat(Path::new(&entry.name))?;
            self.journal.roots.push(PublishedRoot {
                name: entry.name.clone(),
                dev: metadata.st_dev as u64,
                ino: metadata.st_ino,
            });
        }
        save(&self.base, &self.journal)?;
        for index in 0..self.journal.roots.len() {
            cancel.check()?;
            let name = self.journal.roots[index].name.clone();
            let entry = self
                .journal
                .manifest
                .entries
                .iter()
                .find(|entry| entry.parent.is_none() && entry.name == name)
                .context("missing root entry")?;
            let (mode, modified) = (entry.mode, entry.modified);
            let directory = (entry.kind == EntryKind::Directory)
                .then(|| self.stage.file(Path::new(&name), false, true))
                .transpose()?;
            let current = Directory::open(&self.journal.directory)?.0.metadata()?;
            let expected = self.destination.0.metadata()?;
            anyhow::ensure!(
                current.dev() == expected.dev() && current.ino() == expected.ino(),
                "receive destination moved during transfer"
            );
            self.stage.publish(&name, &self.destination)?;
            if let Some(directory) = directory {
                set_metadata(&directory, mode, &modified)?;
            }
            self.journal
                .record
                .paths
                .push(self.journal.directory.join(&name));
            save(&self.base, &self.journal)?;
        }
        self.journal.record.state = TransferState::Ready;
        self.journal.record.bytes = self.journal.record.total_bytes;
        save(&self.base, &self.journal)?;
        self.cleanup()?;
        Ok(self.journal.record.paths.clone())
    }

    pub fn fail(&mut self, error: String, cancelled: bool, bytes: u64) -> Result<()> {
        for root in &self.journal.roots {
            if let Ok(metadata) = self.destination.stat(Path::new(&root.name)) {
                let path = self.journal.directory.join(&root.name);
                if metadata.st_dev as u64 == root.dev
                    && metadata.st_ino == root.ino
                    && !self.journal.record.paths.contains(&path)
                {
                    self.journal.record.paths.push(path);
                }
            }
        }
        self.journal.record.state = if cancelled {
            TransferState::Cancelled
        } else {
            TransferState::Failed
        };
        self.journal.record.error = Some(error);
        self.journal.record.bytes = bytes;
        save(&self.base, &self.journal)?;
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<()> {
        let expected = self.stage.0.metadata()?;
        let name = CString::new(self.journal.staging.as_str())?;
        let stat = stat_at(&self.destination.0, &name)?;
        anyhow::ensure!(
            stat.st_mode & libc::S_IFMT == libc::S_IFDIR
                && stat.st_dev as u64 == expected.dev()
                && stat.st_ino == expected.ino(),
            "staging directory moved; retained for recovery"
        );
        let parent_path = self.journal.directory.clone();
        claim_and_remove(
            &self.base,
            &mut self.journal,
            &self.destination.0,
            &parent_path,
            &name,
            &self.stage.0,
        )
        .context("staging directory cleanup; retained for recovery")?;
        self.destination.sync()
    }
}

pub(super) fn load_enabled(base: &Path) -> Result<bool> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(base.join("settings.json"))
    {
        Ok(file) => {
            anyhow::ensure!(
                file.metadata()?.len() <= 16,
                "invalid file sharing settings"
            );
            Ok(serde_json::from_reader(file)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).context("read file sharing settings"),
    }
}

pub(super) fn save_enabled(base: &Path, enabled: bool) -> Result<()> {
    let temporary = base.join(format!(
        "settings-{}.tmp",
        super::hex(&super::random::<16>()?)
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(&enabled)?)?;
    file.sync_all()?;
    std::fs::rename(temporary, base.join("settings.json"))?;
    Directory::open(base)?.sync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{FileOfferId, TransferDirection, TransferId};
    use splice_proto::MachineId;

    fn staging(base: &Path, destination: &Path) -> Staging {
        staging_into(base, ReceiveDestination::Directory(destination.into()))
    }

    fn staging_into(base: &Path, destination: ReceiveDestination) -> Staging {
        initialize(base).unwrap();
        let manifest = Manifest {
            generation: [1; 16],
            entries: vec![
                crate::files::ManifestEntry {
                    mode: 0o700,
                    modified: splice_proto::files::FileTimestamp {
                        seconds: 1_700_000_000,
                        nanos: 0,
                    },
                    id: EntryId(0),
                    parent: None,
                    name: "first".into(),
                    kind: EntryKind::File { size: 3 },
                },
                crate::files::ManifestEntry {
                    mode: 0o700,
                    modified: splice_proto::files::FileTimestamp {
                        seconds: 1_700_000_000,
                        nanos: 0,
                    },
                    id: EntryId(1),
                    parent: None,
                    name: "second".into(),
                    kind: EntryKind::File { size: 3 },
                },
            ],
            total_bytes: 6,
        };
        let record = TransferRecord {
            id: TransferId(super::super::random().unwrap()),
            offer: FileOfferId([2; 16]),
            peer: MachineId("source".into()),
            direction: TransferDirection::Receive,
            state: TransferState::Receiving,
            bytes: 0,
            total_bytes: 6,
            paths: vec![],
            error: None,
        };
        let staging = Staging::new(
            base.into(),
            record,
            destination,
            manifest,
            &Cancellation::default(),
        )
        .unwrap();
        for id in [EntryId(0), EntryId(1)] {
            let mut file = staging
                .stage
                .file(staging.relative.get(&id).unwrap(), true, false)
                .unwrap();
            file.write_all(b"abc").unwrap();
            file.sync_all().unwrap();
        }
        staging
    }

    #[test]
    fn applies_modes_mtimes_and_real_relative_links_before_publication() {
        let source = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let root = source.path().join("root");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("script"), b"executable").unwrap();
        std::os::unix::fs::symlink("../script", root.join("nested/link")).unwrap();
        let directory = Directory::open(source.path()).unwrap();
        let timestamp = super::super::FileTimestamp {
            seconds: 1_600_000_123,
            nanos: 123_456_789,
        };
        for (path, kind, mode) in [
            ("root/script", EntryKind::File { size: 10 }, 0o751),
            (
                "root/nested/link",
                EntryKind::Symlink {
                    target: "../script".into(),
                },
                0o777,
            ),
            ("root/nested", EntryKind::Directory, 0o750),
            ("root", EntryKind::Directory, 0o751),
        ] {
            apply_metadata(
                &directory,
                Path::new(path),
                &super::super::ManifestEntry {
                    id: EntryId(0),
                    parent: None,
                    name: String::new(),
                    kind,
                    mode,
                    modified: timestamp,
                },
            )
            .unwrap();
        }
        std::fs::set_permissions(root.join("script"), std::fs::Permissions::from_mode(0o4751))
            .unwrap();
        let selection =
            super::super::manifest::Selection::build(vec![root], &Cancellation::default()).unwrap();
        assert_eq!(selection.manifest.total_bytes, 10);
        initialize(base.path()).unwrap();
        let record = TransferRecord {
            id: TransferId([7; 16]),
            offer: FileOfferId([8; 16]),
            peer: MachineId("source".into()),
            direction: TransferDirection::Receive,
            state: TransferState::Receiving,
            bytes: 0,
            total_bytes: 10,
            paths: vec![],
            error: None,
        };
        let mut stage = Staging::new(
            base.path().into(),
            record,
            ReceiveDestination::Directory(destination.path().into()),
            selection.manifest.clone(),
            &Cancellation::default(),
        )
        .unwrap();
        for entry in &selection.manifest.entries {
            if matches!(entry.kind, EntryKind::File { .. }) {
                let mut output = stage
                    .stage
                    .file(&stage.relative[&entry.id], true, false)
                    .unwrap();
                std::io::copy(&mut selection.open(entry.id).unwrap(), &mut output).unwrap();
                output.sync_all().unwrap();
            }
        }
        stage.complete(&Cancellation::default()).unwrap();
        for entry in &selection.manifest.entries {
            let path = destination.path().join(&stage.relative[&entry.id]);
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert_eq!(metadata.mode() & 0o7777, entry.mode, "{}", path.display());
            assert_eq!(
                (metadata.mtime(), metadata.mtime_nsec()),
                (timestamp.seconds, timestamp.nanos.into())
            );
            if let EntryKind::Symlink { target } = &entry.kind {
                assert!(metadata.file_type().is_symlink());
                assert_eq!(std::fs::read_link(path).unwrap(), PathBuf::from(target));
            }
        }
        assert_eq!(
            std::fs::read(destination.path().join("root/nested/link")).unwrap(),
            b"executable"
        );
        assert_eq!(
            initialize(base.path()).unwrap().journals[0].record.state,
            TransferState::Ready
        );
    }

    #[test]
    fn failed_clear_preserves_ready_ownership_and_does_not_poison_recovery() {
        let base = tempfile::tempdir().unwrap();
        let mut bad = staging_into(base.path(), ReceiveDestination::Cache);
        bad.complete(&Cancellation::default()).unwrap();
        let id = bad.journal.record.id;
        let paths = bad.journal.record.paths.clone();
        std::fs::remove_file(&paths[0]).unwrap();
        std::fs::write(&paths[0], b"abc").unwrap();
        assert!(clear_received(base.path(), id).is_err());
        let path = base
            .path()
            .join("records")
            .join(format!("{}.json", super::super::hex(&id.0)));
        let persisted = load_journal(base.path(), &path).unwrap();
        assert_eq!(persisted.record.state, TransferState::Ready);
        assert_eq!(persisted.record.paths, paths);
        assert!(persisted.record.error.is_some());
        assert!(!persisted.clearing);
        bad.journal.clearing = true;
        save(base.path(), &bad.journal).unwrap();
        let recovered = initialize(base.path()).unwrap();
        assert!(recovered.error_count > 0);
        assert_eq!(recovered.journals[0].record.state, TransferState::Ready);
        assert_eq!(recovered.journals[0].record.paths, paths);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"abc");
        let mut good = staging_into(base.path(), ReceiveDestination::Cache);
        good.complete(&Cancellation::default()).unwrap();
        assert_eq!(initialize(base.path()).unwrap().journals.len(), 2);
        std::fs::remove_file(&paths[0]).unwrap();
        clear_received(base.path(), id).unwrap();
        assert_eq!(initialize(base.path()).unwrap().journals.len(), 1);
    }

    #[test]
    fn corrupt_journal_is_quarantined_with_persistent_visible_diagnostics() {
        let base = tempfile::tempdir().unwrap();
        let mut good = staging_into(base.path(), ReceiveDestination::Cache);
        good.complete(&Cancellation::default()).unwrap();
        let path = base
            .path()
            .join("records")
            .join("1234567890abcdef1234567890abcdef.json");
        std::fs::write(&path, b"corrupt journal").unwrap();
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.journals.len(), 1);
        assert_eq!(recovered.journals[0].record.state, TransferState::Ready);
        assert_eq!(recovered.error_count, 1);
        assert!(recovered.errors[0].message.contains("Quarantined"));
        assert!(!path.exists());
        let quarantine = std::fs::read_dir(base.path().join("records"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "bad"))
            .unwrap();
        assert_eq!(std::fs::read(quarantine).unwrap(), b"corrupt journal");
        assert_eq!(initialize(base.path()).unwrap().error_count, 1);
    }

    #[test]
    fn unsupported_journal_retains_payload_and_cache_reservation() {
        let base = tempfile::tempdir().unwrap();
        let mut stage = staging_into(base.path(), ReceiveDestination::Cache);
        stage.complete(&Cancellation::default()).unwrap();
        std::fs::write(&stage.journal.record.paths[0], b"retained bytes").unwrap();
        let mut old = serde_json::to_value(&stage.journal).unwrap();
        for entry in old["manifest"]["entries"].as_array_mut().unwrap() {
            entry.as_object_mut().unwrap().remove("mode");
            entry.as_object_mut().unwrap().remove("modified");
        }
        let path = base.path().join("records").join(format!(
            "{}.json",
            super::super::hex(&stage.journal.record.id.0)
        ));
        std::fs::write(path, serde_json::to_vec(&old).unwrap()).unwrap();
        let recovered = initialize(base.path()).unwrap();
        assert!(recovered.journals.is_empty());
        assert_eq!(recovered.untracked_cache_bytes, 17);
        assert_eq!(recovered.error_count, 2);
        assert_eq!(
            std::fs::read(&stage.journal.record.paths[0]).unwrap(),
            b"retained bytes"
        );
        assert_eq!(initialize(base.path()).unwrap().untracked_cache_bytes, 17);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn locked_cache_entry_clear_preserves_ready_receipt_and_all_roots() {
        use std::os::fd::AsRawFd;
        struct Unlock(File);
        impl Drop for Unlock {
            fn drop(&mut self) {
                unsafe {
                    libc::fchflags(self.0.as_raw_fd(), 0);
                }
            }
        }
        let base = tempfile::tempdir().unwrap();
        let mut stage = staging_into(base.path(), ReceiveDestination::Cache);
        stage.complete(&Cancellation::default()).unwrap();
        let locked = Unlock(File::open(&stage.journal.record.paths[1]).unwrap());
        assert_eq!(
            unsafe { libc::fchflags(locked.0.as_raw_fd(), libc::UF_IMMUTABLE) },
            0
        );
        let id = stage.journal.record.id;
        assert!(clear_received(base.path(), id).is_err());
        assert!(stage.journal.record.paths.iter().all(|path| path.exists()));
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.journals[0].record.state, TransferState::Ready);
        assert!(recovered.journals[0].record.error.is_some());
        drop(locked);
        clear_received(base.path(), id).unwrap();
    }

    #[test]
    fn recovery_reclaims_empty_stage_between_mkdir_and_identity_journal() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        for name in ["first", "second"] {
            std::fs::remove_file(
                stage
                    .journal
                    .directory
                    .join(&stage.journal.staging)
                    .join(name),
            )
            .unwrap();
        }
        stage.journal.stage_identity = None;
        save(base.path(), &stage.journal).unwrap();
        drop(stage);
        assert_eq!(
            initialize(base.path()).unwrap().journals[0].record.state,
            TransferState::Failed
        );
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[test]
    fn recovery_does_not_adopt_nonempty_stage_without_identity() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        let private = stage.journal.directory.join(&stage.journal.staging);
        stage.journal.stage_identity = None;
        save(base.path(), &stage.journal).unwrap();
        drop(stage);
        assert_eq!(initialize(base.path()).unwrap().error_count, 1);
        assert_eq!(std::fs::read(private.join("first")).unwrap(), b"abc");
    }

    #[test]
    fn interrupted_clear_finishes_without_removing_saved_files() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        stage.complete(&Cancellation::default()).unwrap();
        stage.journal.clearing = true;
        save(base.path(), &stage.journal).unwrap();
        drop(stage);
        assert!(initialize(base.path()).unwrap().journals.is_empty());
        assert_eq!(
            std::fs::read(destination.path().join("first")).unwrap(),
            b"abc"
        );
        assert_eq!(
            std::fs::read_dir(base.path().join("records"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn recovery_reclaims_cache_directory_created_before_journal() {
        let base = tempfile::tempdir().unwrap();
        initialize(base.path()).unwrap();
        let parent = Directory::open(&base.path().join("received")).unwrap();
        parent.mkdir("0123456789abcdef0123456789abcdef").unwrap();
        initialize(base.path()).unwrap();
        assert_eq!(
            std::fs::read_dir(base.path().join("received"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn crash_after_publish_before_result_recovers_published_roots() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        for name in ["first", "second"] {
            let metadata = stage
                .stage
                .file(Path::new(name), false, false)
                .unwrap()
                .metadata()
                .unwrap();
            stage.journal.roots.push(PublishedRoot {
                name: name.into(),
                dev: metadata.dev(),
                ino: metadata.ino(),
            });
        }
        save(base.path(), &stage.journal).unwrap();
        for name in ["first", "second"] {
            stage.stage.publish(name, &stage.destination).unwrap();
        }
        drop(stage);
        let records = initialize(base.path()).unwrap().journals;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record.state, TransferState::Ready);
        assert_eq!(records[0].record.bytes, records[0].record.total_bytes);
        assert_eq!(records[0].record.paths.len(), 2);
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 2);
    }

    #[test]
    fn collision_preserves_existing_data_and_records_each_published_root() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        std::fs::write(destination.path().join("second"), b"existing").unwrap();
        assert!(stage.complete(&Cancellation::default()).is_err());
        assert_eq!(
            stage.journal.record.paths,
            [destination.path().canonicalize().unwrap().join("first")]
        );
        stage.fail("destination exists".into(), false, 6).unwrap();
        assert_eq!(
            std::fs::read(destination.path().join("second")).unwrap(),
            b"existing"
        );
        let records = initialize(base.path()).unwrap().journals;
        assert_eq!(records[0].record.state, TransferState::Failed);
        assert_eq!(
            records[0].record.paths,
            [destination.path().canonicalize().unwrap().join("first")]
        );
    }

    #[test]
    fn interrupted_unverified_staging_is_never_published_and_is_removed_on_recovery() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        drop(staging(base.path(), destination.path()));
        let records = initialize(base.path()).unwrap().journals;
        assert_eq!(records[0].record.state, TransferState::Failed);
        assert!(records[0].record.paths.is_empty());
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[test]
    fn cancellation_before_publication_leaves_no_success_files() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(stage.complete(&cancel).is_err());
        stage.fail("cancelled".into(), true, 6).unwrap();
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o777
    }

    fn relax(path: &Path) {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            set_mode(path, 0o700);
            for entry in std::fs::read_dir(path).unwrap() {
                relax(&entry.unwrap().path());
            }
        }
    }

    fn readonly_source() -> (tempfile::TempDir, PathBuf) {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/inner"), b"inner").unwrap();
        std::fs::write(root.join("top"), b"top").unwrap();
        set_mode(&root.join("sub/inner"), 0o444);
        set_mode(&root.join("top"), 0o444);
        set_mode(&root.join("sub"), 0o500);
        set_mode(&root, 0o555);
        (source, root)
    }

    fn receive_tree(
        base: &Path,
        destination: ReceiveDestination,
        root: &Path,
    ) -> (Staging, super::super::manifest::Selection) {
        initialize(base).unwrap();
        let selection = super::super::manifest::Selection::build(
            vec![root.to_path_buf()],
            &Cancellation::default(),
        )
        .unwrap();
        let record = TransferRecord {
            id: TransferId(super::super::random().unwrap()),
            offer: FileOfferId([8; 16]),
            peer: MachineId("source".into()),
            direction: TransferDirection::Receive,
            state: TransferState::Receiving,
            bytes: 0,
            total_bytes: selection.manifest.total_bytes,
            paths: vec![],
            error: None,
        };
        let stage = Staging::new(
            base.into(),
            record,
            destination,
            selection.manifest.clone(),
            &Cancellation::default(),
        )
        .unwrap();
        for entry in &selection.manifest.entries {
            if matches!(entry.kind, EntryKind::File { .. }) {
                let mut output = stage
                    .stage
                    .file(&stage.relative[&entry.id], true, false)
                    .unwrap();
                std::io::copy(&mut selection.open(entry.id).unwrap(), &mut output).unwrap();
                output.sync_all().unwrap();
            }
        }
        (stage, selection)
    }

    #[test]
    fn readonly_root_and_nested_directories_publish_to_save_and_cache_and_clear() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let (mut saved, selection) = receive_tree(
            base.path(),
            ReceiveDestination::Directory(destination.path().into()),
            &root,
        );
        let paths = saved.complete(&Cancellation::default()).unwrap();
        let published = destination.path().canonicalize().unwrap().join("root");
        assert_eq!(paths, std::slice::from_ref(&published));
        for entry in &selection.manifest.entries {
            let path = destination.path().join(&saved.relative[&entry.id]);
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert_eq!(metadata.mode() & 0o777, entry.mode, "{}", path.display());
            assert_eq!(
                (metadata.mtime(), metadata.mtime_nsec()),
                (entry.modified.seconds, entry.modified.nanos.into())
            );
        }
        let (mut cached, _) = receive_tree(base.path(), ReceiveDestination::Cache, &root);
        cached.complete(&Cancellation::default()).unwrap();
        let cache_root = cached.journal.record.paths[0].clone();
        assert_eq!(mode_of(&cache_root), 0o555);
        assert_eq!(mode_of(&cache_root.join("sub")), 0o500);
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert!(recovered.journals.iter().all(|journal| {
            journal.record.state == TransferState::Ready
                && journal.record.bytes == journal.record.total_bytes
        }));
        assert!(!destination
            .path()
            .join(&saved.journal.staging)
            .try_exists()
            .unwrap());
        clear_received(base.path(), cached.journal.record.id).unwrap();
        assert!(!cached.journal.directory.try_exists().unwrap());
        clear_received(base.path(), saved.journal.record.id).unwrap();
        assert_eq!(mode_of(&published), 0o555);
        assert_eq!(mode_of(&published.join("sub")), 0o500);
        assert_eq!(
            std::fs::read(published.join("sub/inner")).unwrap(),
            b"inner"
        );
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
        assert!(initialize(base.path()).unwrap().journals.is_empty());
        relax(destination.path());
        relax(source.path());
    }

    #[test]
    fn failed_publish_removes_readonly_staging_without_touching_destination() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(destination.path().join("root"), b"existing").unwrap();
        let (mut stage, _) = receive_tree(
            base.path(),
            ReceiveDestination::Directory(destination.path().into()),
            &root,
        );
        assert!(stage.complete(&Cancellation::default()).is_err());
        stage.fail("destination exists".into(), false, 8).unwrap();
        let names: Vec<_> = std::fs::read_dir(destination.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["root"]);
        assert_eq!(
            std::fs::read(destination.path().join("root")).unwrap(),
            b"existing"
        );
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert_eq!(recovered.journals[0].record.state, TransferState::Failed);
        assert!(recovered.journals[0].record.paths.is_empty());
        relax(source.path());
    }

    #[test]
    fn cancelled_staging_with_applied_readonly_modes_is_removed() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let (mut stage, selection) = receive_tree(
            base.path(),
            ReceiveDestination::Directory(destination.path().into()),
            &root,
        );
        for entry in selection.manifest.entries.iter().rev() {
            if entry.parent.is_some() {
                apply_metadata(&stage.stage, &stage.relative[&entry.id], entry).unwrap();
            }
        }
        let private = destination.path().join(&stage.journal.staging);
        assert_eq!(mode_of(&private.join("root/sub")), 0o500);
        stage.fail("cancelled".into(), true, 5).unwrap();
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert_eq!(recovered.journals[0].record.state, TransferState::Cancelled);
        relax(source.path());
    }

    #[test]
    fn non_searchable_owned_cache_directories_clear_and_inventory() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let (mut cached, _) = receive_tree(base.path(), ReceiveDestination::Cache, &root);
        cached.complete(&Cancellation::default()).unwrap();
        let cache_root = cached.journal.record.paths[0].clone();
        set_mode(&cache_root.join("sub"), 0o600);
        set_mode(&cache_root, 0o400);
        clear_received(base.path(), cached.journal.record.id).unwrap();
        assert!(!cached.journal.directory.try_exists().unwrap());
        assert!(std::fs::read_dir(base.path().join("received"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".splice-")));
        let (mut untracked, _) = receive_tree(base.path(), ReceiveDestination::Cache, &root);
        untracked.complete(&Cancellation::default()).unwrap();
        let cache_root = untracked.journal.record.paths[0].clone();
        set_mode(&cache_root.join("sub"), 0o600);
        set_mode(&cache_root, 0o400);
        std::fs::remove_file(base.path().join("records").join(format!(
            "{}.json",
            super::super::hex(&untracked.journal.record.id.0)
        )))
        .unwrap();
        let recovered = initialize(base.path()).unwrap();
        assert!(recovered.journals.is_empty());
        assert_eq!(recovered.untracked_cache_bytes, 8);
        assert_eq!(recovered.error_count, 1);
        assert_eq!(
            std::fs::read(cache_root.join("sub/inner")).unwrap(),
            b"inner"
        );
        relax(source.path());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unreadable_owned_directory_fails_clear_clearly_and_records_relocation() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let (mut cached, _) = receive_tree(base.path(), ReceiveDestination::Cache, &root);
        cached.complete(&Cancellation::default()).unwrap();
        let id = cached.journal.record.id;
        let original = cached.journal.directory.clone();
        set_mode(&original.join("root/sub"), 0o000);
        let error = clear_received(base.path(), id).unwrap_err().error;
        assert!(format!("{error:#}").contains("not readable"), "{error:#}");
        let holder = base.path().join("received").join(holder_name(id));
        assert!(!original.try_exists().unwrap());
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert_eq!(recovered.untracked_cache_bytes, 0);
        let record = &recovered.journals[0].record;
        assert_eq!(record.state, TransferState::Ready);
        assert!(record.error.is_some());
        assert_eq!(record.paths.len(), 1);
        assert!(record.paths[0].starts_with(&holder));
        assert_eq!(mode_of(&record.paths[0].join("sub")), 0o000);
        assert_eq!(std::fs::read(record.paths[0].join("top")).unwrap(), b"top");
        set_mode(&record.paths[0].join("sub"), 0o500);
        clear_received(base.path(), id).unwrap();
        assert!(!holder.try_exists().unwrap());
        assert_eq!(
            std::fs::read_dir(base.path().join("received"))
                .unwrap()
                .count(),
            0
        );
        assert!(initialize(base.path()).unwrap().journals.is_empty());
        relax(source.path());
    }

    #[test]
    fn hard_linked_external_file_inside_receipt_is_unlinked_without_changing_it() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let target = external.path().join("shared");
        std::fs::write(&target, b"external").unwrap();
        set_mode(&target, 0o444);
        let (mut cached, _) = receive_tree(base.path(), ReceiveDestination::Cache, &root);
        cached.complete(&Cancellation::default()).unwrap();
        let cache_root = cached.journal.record.paths[0].clone();
        set_mode(&cache_root, 0o700);
        std::fs::remove_file(cache_root.join("top")).unwrap();
        std::fs::hard_link(&target, cache_root.join("top")).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 2);
        clear_received(base.path(), cached.journal.record.id).unwrap();
        assert!(!cached.journal.directory.try_exists().unwrap());
        assert_eq!(mode_of(&target), 0o444);
        assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), b"external");
        relax(source.path());
    }

    #[test]
    fn denied_clear_preflight_does_not_mutate_non_searchable_directories() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let (mut cached, _) = receive_tree(base.path(), ReceiveDestination::Cache, &root);
        cached.complete(&Cancellation::default()).unwrap();
        let id = cached.journal.record.id;
        let cache_root = cached.journal.record.paths[0].clone();
        set_mode(&cache_root, 0o700);
        set_mode(&cache_root.join("sub"), 0o600);
        let aside = cached.journal.directory.join("aside");
        std::fs::rename(&cache_root, &aside).unwrap();
        std::fs::create_dir(&cache_root).unwrap();
        let error = clear_received(base.path(), id).unwrap_err().error;
        assert!(
            format!("{error:#}").contains("identity changed"),
            "{error:#}"
        );
        assert_eq!(mode_of(&aside.join("sub")), 0o600);
        assert!(std::fs::read_dir(base.path().join("received"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".splice-")));
        let persisted = initialize(base.path()).unwrap();
        assert_eq!(persisted.journals[0].record.state, TransferState::Ready);
        std::fs::remove_dir(&cache_root).unwrap();
        std::fs::rename(&aside, &cache_root).unwrap();
        clear_received(base.path(), id).unwrap();
        assert!(!cached.journal.directory.try_exists().unwrap());
        relax(source.path());
    }

    fn claim_manually(base: &Path, stage: &mut Staging) -> (PathBuf, PathBuf) {
        let parent = stage.journal.directory.clone();
        let original = parent.join(&stage.journal.staging);
        let holder = parent.join(holder_name(stage.journal.record.id));
        std::fs::create_dir(&holder).unwrap();
        let metadata = std::fs::symlink_metadata(&original).unwrap();
        let name = super::super::hex(&[9; 16]);
        stage.journal.claim = Some(Box::new(Claim {
            original: original.clone(),
            holder: holder.clone(),
            name: name.clone(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        }));
        save(base, &stage.journal).unwrap();
        std::fs::rename(&original, holder.join(&name)).unwrap();
        let location = holder.join(name);
        (holder, location)
    }

    #[test]
    fn crash_after_claim_resumes_removal_by_recorded_identity() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let (mut stage, selection) = receive_tree(
            base.path(),
            ReceiveDestination::Directory(destination.path().into()),
            &root,
        );
        for entry in selection.manifest.entries.iter().rev() {
            apply_metadata(&stage.stage, &stage.relative[&entry.id], entry).unwrap();
        }
        let (holder, claimed) = claim_manually(base.path(), &mut stage);
        assert_eq!(mode_of(&claimed.join("root")), 0o555);
        drop(stage);
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert_eq!(recovered.journals[0].record.state, TransferState::Failed);
        assert!(recovered.journals[0].claim.is_none());
        assert!(!holder.try_exists().unwrap());
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
        relax(source.path());
    }

    #[test]
    fn claimed_entry_with_changed_identity_is_preserved_and_reported() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        let id = stage.journal.record.id;
        let (holder, claimed) = claim_manually(base.path(), &mut stage);
        let mut journal = stage.journal.clone();
        journal.claim.as_mut().unwrap().ino ^= 1;
        save(base.path(), &journal).unwrap();
        drop(stage);
        for _ in 0..2 {
            let recovered = initialize(base.path()).unwrap();
            assert_eq!(recovered.error_count, 1);
            assert!(recovered.cleanup_failures.contains(&id));
            assert!(recovered.errors[0].message.contains("preserved"));
            assert!(recovered.journals[0].claim.is_some());
            assert_eq!(std::fs::read(claimed.join("first")).unwrap(), b"abc");
        }
        assert!(holder.try_exists().unwrap());
        assert!(clear_received(base.path(), id).is_err());
        assert_eq!(std::fs::read(claimed.join("first")).unwrap(), b"abc");
    }

    #[test]
    fn relocated_receipt_is_tracked_and_clears_from_recorded_location() {
        let base = tempfile::tempdir().unwrap();
        let mut cached = staging_into(base.path(), ReceiveDestination::Cache);
        cached.complete(&Cancellation::default()).unwrap();
        let id = cached.journal.record.id;
        let original = cached.journal.directory.clone();
        let holder = base.path().join("received").join(holder_name(id));
        std::fs::create_dir(&holder).unwrap();
        let metadata = std::fs::symlink_metadata(&original).unwrap();
        let name = super::super::hex(&[5; 16]);
        let location = holder.join(&name);
        let mut journal = cached.journal.clone();
        journal.claim = Some(Box::new(Claim {
            original: original.clone(),
            holder: holder.clone(),
            name,
            dev: metadata.dev(),
            ino: metadata.ino(),
        }));
        journal.directory = location.clone();
        journal.record.paths = vec![location.join("first"), location.join("second")];
        save(base.path(), &journal).unwrap();
        std::fs::rename(&original, &location).unwrap();
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert_eq!(recovered.untracked_cache_bytes, 0);
        assert_eq!(recovered.journals[0].record.state, TransferState::Ready);
        assert_eq!(recovered.journals[0].record.paths, journal.record.paths);
        assert_eq!(std::fs::read(&journal.record.paths[0]).unwrap(), b"abc");
        std::fs::remove_file(&journal.record.paths[1]).unwrap();
        std::fs::write(&journal.record.paths[1], b"abc").unwrap();
        assert!(clear_received(base.path(), id).is_err());
        assert_eq!(std::fs::read(&journal.record.paths[0]).unwrap(), b"abc");
        std::fs::remove_file(&journal.record.paths[1]).unwrap();
        clear_received(base.path(), id).unwrap();
        assert!(!holder.try_exists().unwrap());
        assert_eq!(
            std::fs::read_dir(base.path().join("received"))
                .unwrap()
                .count(),
            0
        );
        assert!(initialize(base.path()).unwrap().journals.is_empty());
    }

    #[test]
    fn save_stage_cleanup_uses_a_private_holder_beside_the_stage() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        let holder = stage
            .journal
            .directory
            .join(holder_name(stage.journal.record.id));
        stage.fail("cancelled".into(), true, 0).unwrap();
        assert!(stage.journal.claim.is_none());
        assert!(!holder.try_exists().unwrap());
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
        assert!(initialize(base.path()).unwrap().journals[0].claim.is_none());
    }

    fn record_claim(base: &Path, stage: &mut Staging, create_holder: bool) -> PathBuf {
        let parent = stage.journal.directory.clone();
        let original = parent.join(&stage.journal.staging);
        let holder = parent.join(holder_name(stage.journal.record.id));
        if create_holder {
            std::fs::create_dir(&holder).unwrap();
        }
        let metadata = std::fs::symlink_metadata(&original).unwrap();
        stage.journal.claim = Some(Box::new(Claim {
            original,
            holder: holder.clone(),
            name: super::super::hex(&[9; 16]),
            dev: metadata.dev(),
            ino: metadata.ino(),
        }));
        save(base, &stage.journal).unwrap();
        holder
    }

    #[test]
    fn inaccessible_claim_parent_preserves_ownership_until_access_returns() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let ancestor = destination.path().join("ancestor");
        let parent = ancestor.join("destination");
        std::fs::create_dir_all(&parent).unwrap();
        let mut stage = staging(base.path(), &parent);
        record_claim(base.path(), &mut stage, true);
        let claim = stage.journal.claim.clone().unwrap();
        std::fs::rename(&claim.original, claim.location()).unwrap();
        set_mode(&ancestor, 0o000);
        let result = resume_claim(base.path(), &mut stage.journal, &claim.original);
        set_mode(&ancestor, 0o700);
        assert!(result.is_err());
        assert!(stage.journal.claim.is_some());
        let reloaded = load_journal(base.path(), &journal_path(base.path(), stage.journal.record.id)).unwrap();
        assert!(reloaded.claim.is_some());
        assert_eq!(std::fs::read(claim.location().join("first")).unwrap(), b"abc");
        assert!(resume_claim(base.path(), &mut stage.journal, &claim.original).unwrap());
        assert!(!claim.holder.exists());
    }

    #[test]
    fn completed_relocated_claim_stays_valid_before_journal_removal() {
        for already_removed in [false, true] {
            let base = tempfile::tempdir().unwrap();
            let mut cached = staging_into(base.path(), ReceiveDestination::Cache);
            cached.complete(&Cancellation::default()).unwrap();
            let id = cached.journal.record.id;
            let original = cached.journal.directory.clone();
            let metadata = std::fs::metadata(&original).unwrap();
            let holder = base.path().join("received").join(holder_name(id));
            std::fs::create_dir(&holder).unwrap();
            let claim = Claim {
                original: original.clone(),
                holder: holder.clone(),
                name: super::super::hex(&[7; 16]),
                dev: metadata.dev(),
                ino: metadata.ino(),
            };
            std::fs::rename(&original, claim.location()).unwrap();
            cached.journal.directory = claim.location();
            cached.journal.record.paths = cached.journal.roots.iter()
                .map(|root| claim.location().join(&root.name)).collect();
            cached.journal.claim = Some(Box::new(claim));
            cached.journal.clearing = true;
            save(base.path(), &cached.journal).unwrap();
            if already_removed {
                std::fs::remove_dir_all(&holder).unwrap();
            }
            assert!(resume_claim(base.path(), &mut cached.journal, &original).unwrap());
            let reloaded = load_journal(base.path(), &journal_path(base.path(), id)).unwrap();
            assert_eq!(reloaded.directory, original);
            assert!(reloaded.claim.is_none());
            assert!(reloaded.record.paths.is_empty());
            let recovered = initialize(base.path()).unwrap();
            assert_eq!(recovered.error_count, 0);
            assert!(recovered.journals.is_empty());
        }
    }

    #[test]
    fn crash_before_claim_rename_resumes_the_pending_move() {
        for create_holder in [false, true] {
            let (source, root) = readonly_source();
            let base = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let (mut stage, selection) = receive_tree(
                base.path(),
                ReceiveDestination::Directory(destination.path().into()),
                &root,
            );
            for entry in selection.manifest.entries.iter().rev() {
                apply_metadata(&stage.stage, &stage.relative[&entry.id], entry).unwrap();
            }
            let holder = record_claim(base.path(), &mut stage, create_holder);
            let original = destination.path().join(&stage.journal.staging);
            drop(stage);
            assert!(original.try_exists().unwrap());
            let recovered = initialize(base.path()).unwrap();
            assert_eq!(recovered.error_count, 0, "holder {create_holder}");
            assert_eq!(recovered.journals[0].record.state, TransferState::Failed);
            assert!(recovered.journals[0].claim.is_none());
            assert!(!holder.try_exists().unwrap());
            assert!(!original.try_exists().unwrap());
            assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
            relax(source.path());
        }
    }

    #[test]
    fn crash_before_claim_rename_with_replaced_original_preserves_it() {
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut stage = staging(base.path(), destination.path());
        let id = stage.journal.record.id;
        record_claim(base.path(), &mut stage, false);
        let original = destination.path().join(&stage.journal.staging);
        drop(stage);
        let aside = destination.path().join("aside");
        std::fs::rename(&original, &aside).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("other"), b"other").unwrap();
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 1);
        assert!(recovered.cleanup_failures.contains(&id));
        assert!(recovered.errors[0].message.contains("preserved"));
        assert_eq!(std::fs::read(original.join("other")).unwrap(), b"other");
        assert_eq!(std::fs::read(aside.join("first")).unwrap(), b"abc");
    }

    fn deep_directories(root: &Path) -> PathBuf {
        let mut path = root.join("deep");
        for _ in 0..splice_proto::files::MAX_DEPTH + 4 {
            path = path.join("d");
        }
        std::fs::create_dir_all(&path).unwrap();
        root.join("deep")
    }

    #[test]
    fn interrupted_clear_failure_during_recovery_keeps_relocated_paths_in_the_same_journal() {
        let base = tempfile::tempdir().unwrap();
        let mut cached = staging_into(base.path(), ReceiveDestination::Cache);
        cached.complete(&Cancellation::default()).unwrap();
        let id = cached.journal.record.id;
        let original = cached.journal.directory.clone();
        let paths = cached.journal.record.paths.clone();
        deep_directories(&original);
        cached.journal.clearing = true;
        save(base.path(), &cached.journal).unwrap();
        let holder = base.path().join("received").join(holder_name(id));
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 1);
        assert!(recovered.cleanup_failures.contains(&id));
        assert_eq!(recovered.untracked_cache_bytes, 0);
        let record = &recovered.journals[0].record;
        assert_eq!(record.state, TransferState::Ready);
        assert!(!recovered.journals[0].clearing);
        assert!(record.error.as_ref().unwrap().contains("depth"));
        assert_eq!(record.paths.len(), 2);
        assert!(record.paths.iter().all(|path| path.starts_with(&holder)));
        assert_ne!(record.paths, paths);
        assert_eq!(std::fs::read(&record.paths[0]).unwrap(), b"abc");
        assert!(!original.try_exists().unwrap());
        let reloaded = load_journal(base.path(), &journal_path(base.path(), id)).unwrap();
        assert_eq!(reloaded.record.paths, record.paths);
        assert_eq!(reloaded.directory, record.paths[0].parent().unwrap());
        let again = initialize(base.path()).unwrap();
        assert_eq!(again.journals[0].record.paths, record.paths);
        assert_eq!(again.untracked_cache_bytes, 0);
        std::fs::remove_dir_all(reloaded.directory.join("deep")).unwrap();
        clear_received(base.path(), id).unwrap();
        assert!(!holder.try_exists().unwrap());
        assert!(initialize(base.path()).unwrap().journals.is_empty());
    }

    #[test]
    fn crash_before_receipt_claim_rename_finishes_clearing() {
        let base = tempfile::tempdir().unwrap();
        let mut cached = staging_into(base.path(), ReceiveDestination::Cache);
        cached.complete(&Cancellation::default()).unwrap();
        let id = cached.journal.record.id;
        let original = cached.journal.directory.clone();
        let metadata = std::fs::symlink_metadata(&original).unwrap();
        cached.journal.clearing = true;
        cached.journal.claim = Some(Box::new(Claim {
            original: original.clone(),
            holder: base.path().join("received").join(holder_name(id)),
            name: super::super::hex(&[3; 16]),
            dev: metadata.dev(),
            ino: metadata.ino(),
        }));
        save(base.path(), &cached.journal).unwrap();
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert!(recovered.journals.is_empty());
        assert_eq!(recovered.untracked_cache_bytes, 0);
        assert_eq!(
            std::fs::read_dir(base.path().join("received"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(base.path().join("records"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn live_clear_failure_reports_relocated_record() {
        let base = tempfile::tempdir().unwrap();
        let mut cached = staging_into(base.path(), ReceiveDestination::Cache);
        cached.complete(&Cancellation::default()).unwrap();
        let id = cached.journal.record.id;
        deep_directories(&cached.journal.directory);
        let failure = clear_received(base.path(), id).unwrap_err();
        let record = failure.record.unwrap();
        assert_eq!(record.state, TransferState::Ready);
        assert_eq!(record.paths.len(), 2);
        assert!(record.paths[0].starts_with(base.path().join("received").join(holder_name(id))));
        assert_eq!(std::fs::read(&record.paths[1]).unwrap(), b"abc");
        assert_eq!(
            load_journal(base.path(), &journal_path(base.path(), id))
                .unwrap()
                .record
                .paths,
            record.paths
        );
    }

    #[test]
    fn crash_between_root_rename_and_metadata_completes_metadata_on_recovery() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let (mut stage, selection) = receive_tree(
            base.path(),
            ReceiveDestination::Directory(destination.path().into()),
            &root,
        );
        for entry in selection.manifest.entries.iter().rev() {
            if entry.parent.is_some() {
                apply_metadata(&stage.stage, &stage.relative[&entry.id], entry).unwrap();
            }
        }
        let metadata = stage.stage.stat(Path::new("root")).unwrap();
        stage.journal.roots.push(PublishedRoot {
            name: "root".into(),
            dev: metadata.st_dev as u64,
            ino: metadata.st_ino,
        });
        save(base.path(), &stage.journal).unwrap();
        stage.stage.publish("root", &stage.destination).unwrap();
        drop(stage);
        let published = destination.path().join("root");
        assert_eq!(mode_of(&published), 0o700);
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert_eq!(recovered.journals[0].record.state, TransferState::Ready);
        assert_eq!(recovered.journals[0].record.bytes, 8);
        let entry = selection
            .manifest
            .entries
            .iter()
            .find(|entry| entry.parent.is_none())
            .unwrap();
        let metadata = std::fs::symlink_metadata(&published).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o555);
        assert_eq!(
            (metadata.mtime(), metadata.mtime_nsec()),
            (entry.modified.seconds, entry.modified.nanos.into())
        );
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
        relax(destination.path());
        relax(source.path());
    }

    #[test]
    fn recovery_removes_staged_readonly_root_without_error() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let (stage, selection) = receive_tree(
            base.path(),
            ReceiveDestination::Directory(destination.path().into()),
            &root,
        );
        for entry in selection.manifest.entries.iter().rev() {
            apply_metadata(&stage.stage, &stage.relative[&entry.id], entry).unwrap();
        }
        let private = destination.path().join(&stage.journal.staging);
        assert_eq!(mode_of(&private.join("root")), 0o555);
        drop(stage);
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 0);
        assert_eq!(recovered.journals[0].record.state, TransferState::Failed);
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
        relax(source.path());
    }

    #[test]
    fn cleanup_never_follows_links_or_alters_published_destination_trees() {
        let (source, root) = readonly_source();
        let base = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::create_dir(external.path().join("locked")).unwrap();
        std::fs::write(external.path().join("locked/keep"), b"keep").unwrap();
        set_mode(&external.path().join("locked"), 0o500);
        let (mut saved, _) = receive_tree(
            base.path(),
            ReceiveDestination::Directory(destination.path().into()),
            &root,
        );
        saved.complete(&Cancellation::default()).unwrap();
        let published = saved.journal.record.paths[0].clone();
        let mut stage = staging(base.path(), destination.path());
        let private = destination.path().join(&stage.journal.staging);
        std::os::unix::fs::symlink(&published, private.join("root")).unwrap();
        std::os::unix::fs::symlink(external.path().join("locked"), private.join("locked")).unwrap();
        std::fs::create_dir(private.join("nested")).unwrap();
        std::os::unix::fs::symlink(external.path(), private.join("nested/out")).unwrap();
        stage.fail("cancelled".into(), true, 0).unwrap();
        assert!(!private.try_exists().unwrap());
        assert_eq!(mode_of(&published), 0o555);
        assert_eq!(mode_of(&published.join("sub")), 0o500);
        assert_eq!(std::fs::read(published.join("top")).unwrap(), b"top");
        assert_eq!(mode_of(&external.path().join("locked")), 0o500);
        assert_eq!(
            std::fs::read(external.path().join("locked/keep")).unwrap(),
            b"keep"
        );
        let mut replaced = staging(base.path(), destination.path());
        let private = destination.path().join(&replaced.journal.staging);
        relax(&private);
        std::fs::remove_dir_all(&private).unwrap();
        std::os::unix::fs::symlink(external.path(), &private).unwrap();
        assert!(replaced.fail("cancelled".into(), true, 0).is_err());
        let journal = replaced.journal.clone();
        drop(replaced);
        let recovered = initialize(base.path()).unwrap();
        assert_eq!(recovered.error_count, 1);
        assert!(recovered.cleanup_failures.contains(&journal.record.id));
        assert!(std::fs::symlink_metadata(&private)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(mode_of(&external.path().join("locked")), 0o500);
        assert_eq!(
            std::fs::read(external.path().join("locked/keep")).unwrap(),
            b"keep"
        );
        std::fs::remove_file(&private).unwrap();
        relax(destination.path());
        relax(external.path());
        relax(source.path());
    }
}
