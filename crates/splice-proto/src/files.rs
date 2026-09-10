use crate::{MachineId, ProtoError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

pub const FILE_PORT: u16 = 41720;
pub const FILE_CHUNK: usize = 64 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
pub const MAX_ENTRIES: usize = 4096;
pub const MAX_ROOTS: usize = 128;
pub const MAX_DEPTH: usize = 64;
pub const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub const OFFER_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;
pub const BULK_MAGIC: &[u8; 8] = b"SPLFILE7";

macro_rules! identifier {
    ($name:ident, $inner:ty) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(pub $inner);
    };
}
identifier!(FileOfferId, [u8; 16]);
identifier!(TransferId, [u8; 16]);
identifier!(NativeDragId, [u8; 16]);
identifier!(EntryId, u32);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OfferOrigin {
    Selection,
    Clipboard { generation: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Directory,
    File { size: u64 },
    Symlink { target: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTimestamp {
    pub seconds: i64,
    pub nanos: u32,
}

impl FileTimestamp {
    pub fn valid(self) -> bool {
        (-62_135_596_800..=253_402_300_799).contains(&self.seconds) && self.nanos < 1_000_000_000
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub name: String,
    pub kind: EntryKind,
    pub mode: u32,
    pub modified: FileTimestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub generation: [u8; 16],
    pub entries: Vec<ManifestEntry>,
    pub total_bytes: u64,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0', ':'])
        && !name.ends_with(['.', ' '])
}

fn link_components(target: &str) -> Result<std::collections::VecDeque<String>, ProtoError> {
    if target.is_empty() || target.len() > 4096 || target.starts_with('/') || target.ends_with('/')
    {
        return Err(ProtoError::InvalidData("invalid relative symlink target"));
    }
    target
        .split('/')
        .map(|part| {
            if part == "." || part == ".." || valid_name(part) {
                Ok(part.to_string())
            } else {
                Err(ProtoError::InvalidData(
                    "nonportable symlink target component",
                ))
            }
        })
        .collect()
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ProtoError> {
        let invalid = || ProtoError::InvalidData("invalid file manifest");
        if self.entries.is_empty()
            || self.entries.len() > MAX_ENTRIES
            || self.generation == [0; 16]
            || self.total_bytes > MAX_TOTAL_BYTES
            || postcard::to_allocvec(self)?.len() > MAX_MANIFEST_BYTES
        {
            return Err(invalid());
        }
        let mut seen = HashMap::new();
        let mut names = HashSet::new();
        let mut total = 0u64;
        let mut roots = 0;
        for entry in &self.entries {
            if !valid_name(&entry.name)
                || entry.mode & !0o777 != 0
                || !entry.modified.valid()
                || (matches!(entry.kind, EntryKind::Symlink { .. })
                    && (entry.parent.is_none() || entry.mode != 0o777))
                || seen.contains_key(&entry.id)
                || !names.insert((
                    entry.parent,
                    entry.name.nfc().collect::<String>().to_lowercase(),
                ))
            {
                return Err(invalid());
            }
            let depth = if let Some(parent) = entry.parent {
                let Some((EntryKind::Directory, depth)) = seen.get(&parent) else {
                    return Err(invalid());
                };
                depth + 1
            } else {
                roots += 1;
                1
            };
            if depth > MAX_DEPTH || roots > MAX_ROOTS {
                return Err(invalid());
            }
            if let EntryKind::File { size } = entry.kind {
                total = total.checked_add(size).ok_or_else(invalid)?;
            }
            seen.insert(entry.id, (entry.kind.clone(), depth));
        }
        if total != self.total_bytes {
            return Err(invalid());
        }
        self.validate_links()
    }

    fn validate_links(&self) -> Result<(), ProtoError> {
        let entries: HashMap<_, _> = self.entries.iter().map(|e| (e.id, e)).collect();
        let names: HashMap<_, _> = self
            .entries
            .iter()
            .map(|e| ((e.parent, e.name.as_str()), e.id))
            .collect();
        let mut edges: HashMap<EntryId, Vec<EntryId>> = HashMap::new();
        for entry in &self.entries {
            if let Some(parent) = entry.parent {
                edges.entry(parent).or_default().push(entry.id);
            }
        }
        let mut budget = MAX_ENTRIES * MAX_DEPTH;
        for entry in &self.entries {
            let EntryKind::Symlink { target } = &entry.kind else {
                continue;
            };
            let mut current = entry
                .parent
                .ok_or(ProtoError::InvalidData("symlink roots are unsupported"))?;
            let mut pending = link_components(target)?;
            let mut expansions = 1;
            while let Some(component) = pending.pop_front() {
                if budget == 0 {
                    return Err(ProtoError::InvalidData(
                        "symlink resolution metadata budget exceeded",
                    ));
                }
                budget -= 1;
                let directory = entries[&current];
                if directory.kind != EntryKind::Directory {
                    return Err(ProtoError::InvalidData("symlink traverses a non-directory"));
                }
                match component.as_str() {
                    "." => {}
                    ".." => {
                        current = directory.parent.ok_or(ProtoError::InvalidData(
                            "symlink target leaves selected root",
                        ))?
                    }
                    name => {
                        let child =
                            *names
                                .get(&(Some(current), name))
                                .ok_or(ProtoError::InvalidData(
                                    "symlink target absent from selected manifest",
                                ))?;
                        if let EntryKind::Symlink { target } = &entries[&child].kind {
                            expansions += 1;
                            if expansions > 40 {
                                return Err(ProtoError::InvalidData(
                                    "symlink cycle or expansion limit exceeded",
                                ));
                            }
                            for part in link_components(target)?.into_iter().rev() {
                                pending.push_front(part);
                            }
                        } else {
                            current = child;
                        }
                    }
                }
            }
            edges.entry(entry.id).or_default().push(current);
        }
        let mut colors = HashMap::new();
        for root in self.entries.iter().filter(|e| e.parent.is_none()) {
            let mut stack = vec![(root.id, false)];
            while let Some((id, finished)) = stack.pop() {
                if finished {
                    colors.insert(id, 2);
                    continue;
                }
                match colors.get(&id) {
                    Some(1) => {
                        return Err(ProtoError::InvalidData(
                            "symlink introduces a manifest cycle",
                        ))
                    }
                    Some(2) => continue,
                    _ => {}
                }
                colors.insert(id, 1);
                stack.push((id, true));
                if let Some(children) = edges.get(&id) {
                    stack.extend(children.iter().rev().map(|id| (*id, false)));
                }
            }
        }
        Ok(())
    }

    pub fn select(&self, roots: &[EntryId]) -> Result<Self, ProtoError> {
        self.validate()?;
        if roots.len() > MAX_ROOTS {
            return Err(ProtoError::InvalidData("too many file roots"));
        }
        let mut selected: HashSet<_> = roots.iter().copied().collect();
        if selected.len() != roots.len()
            || roots.iter().any(|id| {
                !self
                    .entries
                    .iter()
                    .any(|e| e.id == *id && e.parent.is_none())
            })
        {
            return Err(ProtoError::InvalidData("unknown or repeated file root"));
        }
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter(|e| {
                let keep = roots.is_empty()
                    || selected.contains(&e.id)
                    || e.parent.is_some_and(|p| selected.contains(&p));
                if keep {
                    selected.insert(e.id);
                }
                keep
            })
            .cloned()
            .collect();
        let total_bytes = entries
            .iter()
            .map(|e| match e.kind {
                EntryKind::File { size } => size,
                EntryKind::Directory | EntryKind::Symlink { .. } => 0,
            })
            .sum();
        let selected = Self {
            generation: self.generation,
            entries,
            total_bytes,
        };
        selected.validate()?;
        Ok(selected)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub id: FileOfferId,
    pub owner: MachineId,
    pub recipient: MachineId,
    pub origin: OfferOrigin,
    pub manifest: Manifest,
    pub expires_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileMessage {
    Offer(Offer),
    Revoke {
        offer: FileOfferId,
        unaccepted_only: bool,
    },
    Prepare {
        offer: FileOfferId,
        generation: [u8; 16],
        drag: NativeDragId,
        roots: Vec<EntryId>,
    },
    Prepared {
        drag: NativeDragId,
    },
    Dropped {
        drag: NativeDragId,
    },
    DropConfirmed {
        drag: NativeDragId,
    },
    Commit {
        offer: FileOfferId,
        generation: [u8; 16],
        transfer: TransferId,
        roots: Vec<EntryId>,
        drag: Option<NativeDragId>,
    },
    Grant {
        transfer: TransferId,
        token: [u8; 32],
    },
    Cancel {
        transfer: TransferId,
    },
    CancelDrag {
        drag: NativeDragId,
    },
    Result {
        transfer: TransferId,
        error: Option<String>,
    },
    Reject {
        transfer: Option<TransferId>,
        drag: Option<NativeDragId>,
        reason: String,
    },
}

impl FileMessage {
    pub fn validate(&self) -> Result<(), ProtoError> {
        match self {
            Self::Offer(offer) => {
                if offer.id.0 == [0; 16]
                    || offer.owner == offer.recipient
                    || offer.owner.0.is_empty()
                    || offer.owner.0.len() > 256
                    || offer.recipient.0.is_empty()
                    || offer.recipient.0.len() > 256
                {
                    return Err(ProtoError::InvalidData("invalid file offer identity"));
                }
                offer.manifest.validate()
            }
            Self::Prepare { roots, .. } | Self::Commit { roots, .. } if roots.len() > MAX_ROOTS => {
                Err(ProtoError::InvalidData("too many requested roots"))
            }
            Self::Reject { reason, .. } if reason.len() > 1024 => {
                Err(ProtoError::InvalidData("oversize file error"))
            }
            Self::Result {
                error: Some(error), ..
            } if error.len() > 1024 => Err(ProtoError::InvalidData("oversize file error")),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            generation: [1; 16],
            entries: vec![
                ManifestEntry {
                    mode: 0o700,
                    modified: FileTimestamp {
                        seconds: 1_700_000_000,
                        nanos: 0,
                    },
                    id: EntryId(0),
                    parent: None,
                    name: "root".into(),
                    kind: EntryKind::Directory,
                },
                ManifestEntry {
                    mode: 0o700,
                    modified: FileTimestamp {
                        seconds: 1_700_000_000,
                        nanos: 0,
                    },
                    id: EntryId(1),
                    parent: Some(EntryId(0)),
                    name: "file".into(),
                    kind: EntryKind::File {
                        size: 5 * 1024 * 1024 * 1024,
                    },
                },
            ],
            total_bytes: 5 * 1024 * 1024 * 1024,
        }
    }

    fn entry(id: u32, parent: Option<u32>, name: &str, kind: EntryKind) -> ManifestEntry {
        ManifestEntry {
            id: EntryId(id),
            parent: parent.map(EntryId),
            name: name.into(),
            mode: if matches!(kind, EntryKind::Symlink { .. }) {
                0o777
            } else {
                0o755
            },
            modified: FileTimestamp {
                seconds: 1_700_000_000,
                nanos: 123_456_789,
            },
            kind,
        }
    }

    fn links() -> Manifest {
        Manifest {
            generation: [1; 16],
            total_bytes: 1,
            entries: vec![
                entry(0, None, "root", EntryKind::Directory),
                entry(1, Some(0), "sub", EntryKind::Directory),
                entry(2, Some(1), "deep", EntryKind::Directory),
                entry(3, Some(1), "file", EntryKind::File { size: 1 }),
                entry(
                    4,
                    Some(0),
                    "alias",
                    EntryKind::Symlink {
                        target: "sub/deep".into(),
                    },
                ),
                entry(
                    5,
                    Some(0),
                    "link",
                    EntryKind::Symlink {
                        target: "alias/../file".into(),
                    },
                ),
            ],
        }
    }

    #[test]
    fn symlink_resolution_expands_before_parent_traversal_and_rejects_escapes() {
        let m = links();
        assert!(m.validate().is_ok());
        assert_eq!(m.select(&[EntryId(0)]).unwrap(), m);
        for target in [
            "/outside",
            "../root/sub/file",
            "alias/../../../outside",
            "missing",
            "sub/file/..",
            "sub/\\evil",
            "sub//file",
            "C:drive",
            ".",
        ] {
            let mut bad = m.clone();
            bad.entries[5].kind = EntryKind::Symlink {
                target: target.into(),
            };
            assert!(bad.validate().is_err(), "{target}");
        }
        let mut bad = m.clone();
        bad.entries[4].kind = EntryKind::Symlink {
            target: "link".into(),
        };
        bad.entries[5].kind = EntryKind::Symlink {
            target: "alias".into(),
        };
        assert!(bad.validate().is_err());
        let mut bad = m;
        bad.entries[0].kind = EntryKind::Symlink {
            target: "sub".into(),
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn rejects_privileged_modes_invalid_timestamps_and_long_link_chains() {
        let mut m = manifest();
        m.entries[1].mode = 0o4755;
        assert!(m.validate().is_err());
        m.entries[1].mode = 0o755;
        m.entries[1].modified.nanos = 1_000_000_000;
        assert!(m.validate().is_err());
        m.entries[1].modified.nanos = 0;
        m.entries[1].modified.seconds = i64::MAX;
        assert!(m.validate().is_err());
        let mut m = links();
        for n in 6..50 {
            m.entries.push(entry(
                n,
                Some(0),
                &format!("link{n}"),
                EntryKind::Symlink {
                    target: if n == 49 {
                        "sub/file".into()
                    } else {
                        format!("link{}", n + 1)
                    },
                },
            ));
        }
        assert!(m.validate().is_err());
    }

    #[test]
    fn manifest_rejects_traversal_cycles_duplicate_ids_and_wrong_sizes() {
        for name in [
            "../escape",
            "..",
            ".",
            "/tmp/file",
            "a/b",
            "a\\b",
            "nul\0name",
            "C:drive",
            "trailing.",
        ] {
            let mut m = manifest();
            m.entries[0].name = name.into();
            assert!(m.validate().is_err(), "{name:?}");
        }
        let mut m = manifest();
        assert!(m.validate().is_ok());
        m.entries[1].parent = Some(EntryId(1));
        assert!(m.validate().is_err());
        m = manifest();
        m.entries[1].id = EntryId(0);
        assert!(m.validate().is_err());
        m = manifest();
        m.total_bytes += 1;
        assert!(m.validate().is_err());
    }

    #[test]
    fn manifest_rejects_case_and_normalization_collisions() {
        for (a, b) in [("File", "file"), ("é", "e\u{301}")] {
            let mut m = manifest();
            m.entries[1].name = a.into();
            m.entries.push(ManifestEntry {
                mode: 0o700,
                modified: FileTimestamp {
                    seconds: 1_700_000_000,
                    nanos: 0,
                },
                id: EntryId(2),
                parent: Some(EntryId(0)),
                name: b.into(),
                kind: EntryKind::Directory,
            });
            assert!(m.validate().is_err());
        }
    }

    #[test]
    fn scope_only_selects_unique_manifest_roots() {
        let m = manifest();
        assert_eq!(m.select(&[]).unwrap(), m);
        assert_eq!(m.select(&[EntryId(0)]).unwrap(), m);
        assert!(m.select(&[EntryId(1)]).is_err());
        assert!(m.select(&[EntryId(0), EntryId(0)]).is_err());
        assert!(m.select(&[EntryId(7)]).is_err());
    }
}
