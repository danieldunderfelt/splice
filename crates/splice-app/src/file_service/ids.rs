use splice_core::files as core;
use splice_platform::files as native;

pub fn offer_to_native(id: core::FileOfferId) -> native::FileOfferId {
    native::FileOfferId(u128::from_be_bytes(id.0))
}

pub fn offer_from_native(id: native::FileOfferId) -> core::FileOfferId {
    core::FileOfferId(id.0.to_be_bytes())
}

pub fn entry_to_native(id: core::EntryId) -> native::EntryId {
    native::EntryId(u128::from(id.0))
}

pub fn entry_from_native(id: native::EntryId) -> Option<core::EntryId> {
    u32::try_from(id.0).ok().map(core::EntryId)
}

pub fn transfer_key(id: core::TransferId) -> String {
    let mut key = String::with_capacity(32);
    for byte in id.0 {
        key.push_str(&format!("{byte:02x}"));
    }
    key
}

pub fn transfer_from_key(key: &str) -> Option<core::TransferId> {
    if key.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(key.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(core::TransferId(bytes))
}

pub fn native_manifest(offer: core::FileOfferId, origin: &str, manifest: &core::Manifest) -> native::FileManifest {
    native::FileManifest {
        offer: offer_to_native(offer),
        origin: origin.to_owned(),
        entries: manifest
            .entries
            .iter()
            .map(|entry| {
                let (kind, size) = match entry.kind {
                    core::EntryKind::Directory => (native::EntryKind::Dir, 0),
                    core::EntryKind::File { size } => (native::EntryKind::File, size),
                    core::EntryKind::Symlink { .. } => (native::EntryKind::Symlink, 0),
                };
                let link_target = match &entry.kind {
                    core::EntryKind::Symlink { target } => Some(target.clone()),
                    _ => None,
                };
                native::FileEntry {
                    id: entry_to_native(entry.id),
                    parent: entry.parent.map(entry_to_native),
                    name: entry.name.clone(),
                    kind,
                    size,
                    mtime: entry.modified.seconds,
                    mtime_nanos: entry.modified.nanos,
                    mode: entry.mode & 0o777,
                    link_target,
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_ids_round_trip_through_native_width() {
        let id = core::FileOfferId([7, 1, 255, 0, 9, 9, 9, 9, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(offer_from_native(offer_to_native(id)), id);
        assert_eq!(offer_to_native(id).to_string().len(), 32);
    }

    #[test]
    fn transfer_keys_round_trip() {
        let id = core::TransferId([7, 1, 255, 0, 9, 9, 9, 9, 1, 2, 3, 4, 5, 6, 7, 8]);
        let key = transfer_key(id);
        assert_eq!(key.len(), 32);
        assert_eq!(transfer_from_key(&key), Some(id));
        assert_eq!(transfer_from_key("zz"), None);
        assert_eq!(transfer_from_key(&key[..31]), None);
    }

    #[test]
    fn entry_ids_widen_and_reject_out_of_range() {
        assert_eq!(entry_from_native(entry_to_native(core::EntryId(u32::MAX))), Some(core::EntryId(u32::MAX)));
        assert_eq!(entry_from_native(native::EntryId(u128::from(u32::MAX) + 1)), None);
    }

    #[test]
    fn native_manifest_preserves_tree_sizes_modes_and_links() {
        let stamp = core::FileTimestamp { seconds: 1_700_000_000, nanos: 123_456_789 };
        let manifest = core::Manifest {
            generation: [1; 16],
            total_bytes: 12,
            entries: vec![
                core::ManifestEntry { id: core::EntryId(1), parent: None, name: "dir".into(), kind: core::EntryKind::Directory, mode: 0o755, modified: stamp },
                core::ManifestEntry { id: core::EntryId(2), parent: Some(core::EntryId(1)), name: "a.txt".into(), kind: core::EntryKind::File { size: 12 }, mode: 0o640, modified: stamp },
                core::ManifestEntry { id: core::EntryId(3), parent: Some(core::EntryId(1)), name: "link".into(), kind: core::EntryKind::Symlink { target: "a.txt".into() }, mode: 0o777, modified: stamp },
            ],
        };
        let native = native_manifest(core::FileOfferId([3; 16]), "gamedev", &manifest);
        assert_eq!(native.origin, "gamedev");
        let roots: Vec<_> = native.roots().collect();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].kind, native::EntryKind::Dir);
        assert_eq!(roots[0].mode, 0o755);
        let file = native.entry(native::EntryId(2)).unwrap();
        assert_eq!(file.parent, Some(native::EntryId(1)));
        assert_eq!(file.size, 12);
        assert_eq!(file.kind, native::EntryKind::File);
        assert_eq!(file.mode, 0o640);
        assert_eq!(file.mtime, 1_700_000_000);
        assert_eq!(file.mtime_nanos, 123_456_789);
        let link = native.entry(native::EntryId(3)).unwrap();
        assert_eq!(link.kind, native::EntryKind::Symlink);
        assert_eq!(link.link_target.as_deref(), Some("a.txt"));
        assert_eq!(link.size, 0);
    }
}
