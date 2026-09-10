use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use splice_core::files::{FileOfferId, Manifest, TransferId};
use splice_platform::files::ViewId;
use std::io;
use std::path::{Path, PathBuf};

pub const MAX_RECORDS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRecord {
    #[serde(default)]
    pub views: Vec<String>,
    pub offer: FileOfferId,
    pub transfer: TransferId,
    pub origin: String,
    pub manifest: Manifest,
}

pub struct Journal {
    path: PathBuf,
    records: Mutex<Vec<ViewRecord>>,
}

fn valid(record: &ViewRecord) -> bool {
    record.manifest.validate().is_ok()
        && record.views.iter().all(|view| view.parse::<ViewId>().is_ok())
        && record.origin.len() <= 256
}

impl Journal {
    pub fn open(path: PathBuf) -> Result<(Journal, Option<String>), String> {
        let records = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Vec<ViewRecord>>(&bytes).map_err(|error| {
                format!("The retained-file journal at {} is unreadable ({error}); repair it or move it aside to re-enable file sharing", path.display())
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("The retained-file journal at {} cannot be read: {error}", path.display())),
        };
        let mut seen = std::collections::HashSet::new();
        let unusable = records.iter().filter(|record| !valid(record) || !seen.insert(record.transfer)).count();
        let notice = (unusable > 0).then(|| format!("The retained-file journal holds {unusable} unusable records; they are kept but ignored"));
        Ok((Self { path, records: Mutex::new(records) }, notice))
    }

    pub fn records(&self) -> Vec<ViewRecord> {
        let mut seen = std::collections::HashSet::new();
        self.records.lock().iter().filter(|record| valid(record) && seen.insert(record.transfer)).cloned().collect()
    }

    pub fn insert(&self, record: ViewRecord) -> Result<(), String> {
        if !valid(&record) {
            return Err("the receipt record is invalid".into());
        }
        let mut records = self.records.lock();
        let previous = records.clone();
        records.retain(|existing| existing.transfer != record.transfer);
        if records.len() >= MAX_RECORDS {
            *records = previous;
            return Err(format!("the retained-file store is full ({MAX_RECORDS} receipts); clear received files before accepting more"));
        }
        records.push(record);
        save(&self.path, &records).map_err(|error| {
            *records = previous;
            format!("the retained-file journal could not be written: {error}")
        })
    }

    pub fn remove(&self, transfer: TransferId) -> Result<(), String> {
        let mut records = self.records.lock();
        if !records.iter().any(|record| record.transfer == transfer) {
            return Ok(());
        }
        let previous = records.clone();
        records.retain(|record| record.transfer != transfer);
        save(&self.path, &records).map_err(|error| {
            *records = previous;
            format!("the retained-file journal could not be written: {error}")
        })
    }
}

fn save(path: &Path, records: &[ViewRecord]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temp)?;
    file.write_all(&serde_json::to_vec(records).map_err(io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    let dir = std::fs::File::open(parent)?;
    dir.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use splice_core::files::{EntryId, EntryKind, ManifestEntry};

    pub fn record(views: &[&str], transfer_byte: u8) -> ViewRecord {
        ViewRecord {
            views: views.iter().map(|view| view.to_string()).collect(),
            offer: FileOfferId([1; 16]),
            transfer: TransferId([transfer_byte; 16]),
            origin: "gamedev".into(),
            manifest: Manifest {
                generation: [3; 16],
                total_bytes: 1,
                entries: vec![ManifestEntry {
                    id: EntryId(1),
                    parent: None,
                    name: "a".into(),
                    kind: EntryKind::File { size: 1 },
                    mode: 0o644,
                    modified: splice_core::files::FileTimestamp { seconds: 1_700_000_000, nanos: 0 },
                }],
            },
        }
    }

    #[test]
    fn records_survive_reload_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("views").join("linux-views.json");
        let (journal, notice) = Journal::open(path.clone()).unwrap();
        assert!(notice.is_none());
        journal.insert(record(&[&"a".repeat(32)], 1)).unwrap();
        journal.insert(record(&[], 2)).unwrap();
        journal.insert(record(&[&"b".repeat(32), &"c".repeat(32)], 1)).unwrap();
        let (reloaded, notice) = Journal::open(path.clone()).unwrap();
        assert!(notice.is_none());
        let transfers: Vec<_> = reloaded.records().into_iter().map(|record| record.transfer).collect();
        assert_eq!(transfers, vec![TransferId([2; 16]), TransferId([1; 16])]);
        reloaded.remove(TransferId([2; 16])).unwrap();
        reloaded.remove(TransferId([9; 16])).unwrap();
        let (reloaded, _) = Journal::open(path).unwrap();
        assert_eq!(reloaded.records().len(), 1);
        assert_eq!(reloaded.records()[0].views, vec!["b".repeat(32), "c".repeat(32)]);
    }

    #[test]
    fn full_journal_rejects_new_receipts_without_evicting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("linux-views.json");
        let (journal, _) = Journal::open(path.clone()).unwrap();
        for index in 0..MAX_RECORDS {
            journal.insert(record(&[], index as u8)).unwrap();
        }
        let error = journal.insert(record(&[], 250)).unwrap_err();
        assert!(error.contains("full"), "{error}");
        assert_eq!(journal.records().len(), MAX_RECORDS);
        let (reloaded, _) = Journal::open(path).unwrap();
        assert_eq!(reloaded.records().len(), MAX_RECORDS);
        journal.insert(record(&[&"d".repeat(32)], 7)).unwrap();
        assert_eq!(journal.records().len(), MAX_RECORDS);
    }

    #[test]
    fn failed_write_keeps_the_previous_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("linux-views.json");
        let (journal, _) = Journal::open(path.clone()).unwrap();
        journal.insert(record(&[], 1)).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let error = journal.insert(record(&[], 2)).unwrap_err();
        assert!(error.contains("could not be written"), "{error}");
        assert_eq!(journal.records().len(), 1);
        assert_eq!(journal.records()[0].transfer, TransferId([1; 16]));
        let error = journal.remove(TransferId([1; 16])).unwrap_err();
        assert!(error.contains("could not be written"), "{error}");
        assert_eq!(journal.records().len(), 1);
    }

    #[test]
    fn corrupt_journal_disables_storage_and_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("linux-views.json");
        std::fs::write(&path, b"{not json").unwrap();
        let Err(error) = Journal::open(path.clone()) else { panic!("corrupt journal must not open") };
        assert!(error.contains("unreadable"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
        assert!(!path.with_extension("corrupt").exists());
    }

    #[test]
    fn unreadable_journal_disables_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("linux-views.json");
        std::fs::create_dir(&path).unwrap();
        let Err(error) = Journal::open(path) else { panic!("unreadable journal must not open") };
        assert!(error.contains("cannot be read"), "{error}");
    }

    #[test]
    fn unusable_records_are_kept_on_disk_but_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("linux-views.json");
        let mut bad = record(&["not-a-view-id"], 1);
        bad.manifest.total_bytes = 99;
        let good = record(&[], 2);
        let duplicate = record(&[&"e".repeat(32)], 2);
        std::fs::write(&path, serde_json::to_vec(&vec![bad.clone(), good.clone(), duplicate]).unwrap()).unwrap();
        let (journal, notice) = Journal::open(path.clone()).unwrap();
        let notice = notice.expect("unusable records must be reported");
        assert!(notice.contains("2 unusable"), "{notice}");
        assert_eq!(journal.records(), vec![good.clone()]);
        journal.insert(record(&[], 3)).unwrap();
        let on_disk: Vec<ViewRecord> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk.len(), 4);
        assert_eq!(on_disk[0], bad);
        let (reloaded, notice) = Journal::open(path).unwrap();
        assert!(notice.is_some());
        assert_eq!(reloaded.records().len(), 2);
    }
}
