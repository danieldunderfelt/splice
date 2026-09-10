use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use splice_platform::files::{EntryId, FileError, FileContentSource, ViewId};

use crate::manifest::SourceTree;

#[derive(Clone, Debug)]
pub enum SourceEvent {
    Commit { view: ViewId },
    Cancel { view: ViewId, committed: bool },
    Materialized { view: ViewId, entry: EntryId, bytes: u64, sha256: String },
}

pub struct LocalDirSource {
    views: Mutex<HashMap<ViewId, HashMap<EntryId, PathBuf>>>,
    cache_dir: PathBuf,
    on_event: Box<dyn Fn(SourceEvent) + Send + Sync>,
    pub commits: AtomicU64,
    pub cancels: AtomicU64,
    pub bytes_materialized: AtomicU64,
}

impl LocalDirSource {
    pub fn new(cache_dir: PathBuf, on_event: Box<dyn Fn(SourceEvent) + Send + Sync>) -> std::io::Result<Self> {
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            views: Mutex::new(HashMap::new()),
            cache_dir,
            on_event,
            commits: AtomicU64::new(0),
            cancels: AtomicU64::new(0),
            bytes_materialized: AtomicU64::new(0),
        })
    }

    pub fn register_view(&self, view: ViewId, tree: &SourceTree) {
        self.views.lock().insert(view, tree.paths.clone());
    }

    pub fn drop_view(&self, view: ViewId) {
        self.views.lock().remove(&view);
        let dir = self.cache_dir.join(view.to_string());
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn sha256_of(path: &PathBuf) -> Result<String, FileError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

impl FileContentSource for LocalDirSource {
    fn materialize(&self, view: ViewId, entry: EntryId) -> Result<PathBuf, FileError> {
        let source = {
            let views = self.views.lock();
            let Some(paths) = views.get(&view) else {
                return Err(FileError::Unavailable(format!("unknown view {view}")));
            };
            paths
                .get(&entry)
                .cloned()
                .ok_or_else(|| FileError::NotFound(format!("unknown entry in view {view}")))?
        };
        let before = std::fs::metadata(&source)?;
        let dir = self.cache_dir.join(view.to_string());
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(entry.to_string());
        let source_hash = sha256_of(&source)?;
        {
            let mut input = std::fs::File::open(&source)?;
            let mut output = std::fs::File::create(&dest)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        }
        let dest_hash = sha256_of(&dest)?;
        if source_hash != dest_hash {
            let _ = std::fs::remove_file(&dest);
            return Err(FileError::SourceChanged(format!(
                "hash mismatch copying {}",
                source.display()
            )));
        }
        let after = std::fs::metadata(&source)?;
        if before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            let _ = std::fs::remove_file(&dest);
            return Err(FileError::SourceChanged(format!(
                "source changed during copy: {}",
                source.display()
            )));
        }
        let bytes = after.len();
        self.bytes_materialized.fetch_add(bytes, Ordering::Relaxed);
        (self.on_event)(SourceEvent::Materialized {
            view,
            entry,
            bytes,
            sha256: dest_hash,
        });
        Ok(dest)
    }

    fn on_commit(&self, view: ViewId) {
        self.commits.fetch_add(1, Ordering::Relaxed);
        (self.on_event)(SourceEvent::Commit { view });
    }

    fn on_cancel(&self, view: ViewId, committed: bool) {
        self.cancels.fetch_add(1, Ordering::Relaxed);
        (self.on_event)(SourceEvent::Cancel { view, committed });
    }
}
