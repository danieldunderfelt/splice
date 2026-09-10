use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use splice_platform::files::{EntryId, EntryKind, FileEntry, FileError, FileManifest, FileOfferId};

pub const MAX_ENTRIES: usize = 20_000;
pub const MAX_DEPTH: usize = 64;
pub const MAX_LINK_EXPANSIONS: usize = 64;

#[derive(Debug)]
pub struct SourceTree {
    pub manifest: FileManifest,
    pub paths: HashMap<EntryId, PathBuf>,
}

pub fn from_paths(
    offer: FileOfferId,
    origin: &str,
    roots: &[PathBuf],
) -> Result<SourceTree, FileError> {
    if roots.is_empty() {
        return Err(FileError::Unavailable("empty selection".into()));
    }
    let mut tree = SourceTree {
        manifest: FileManifest {
            offer,
            origin: origin.to_owned(),
            entries: Vec::new(),
        },
        paths: HashMap::new(),
    };
    let mut seen_basenames: HashSet<String> = HashSet::new();
    for root in roots {
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| FileError::Unavailable(format!("root has an unsupported name: {}", root.display())))?
            .to_owned();
        let folded = name.to_lowercase();
        if !seen_basenames.insert(folded) {
            return Err(FileError::LimitExceeded(format!(
                "case-colliding root names including {name}"
            )));
        }
        add_entry(&mut tree, root, None, name, 0)?;
    }
    validate_links(&tree)?;
    Ok(tree)
}

fn add_entry(
    tree: &mut SourceTree,
    path: &Path,
    parent: Option<EntryId>,
    name: String,
    depth: usize,
) -> Result<EntryId, FileError> {
    if tree.manifest.entries.len() >= MAX_ENTRIES {
        return Err(FileError::LimitExceeded(format!(
            "selection exceeds {MAX_ENTRIES} entries"
        )));
    }
    if depth > MAX_DEPTH {
        return Err(FileError::LimitExceeded(format!(
            "selection exceeds depth {MAX_DEPTH}"
        )));
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FileError::Unavailable(format!("unsafe name: {name:?}")));
    }
    let meta = std::fs::symlink_metadata(path)?;
    let id = EntryId::new();
    #[cfg(unix)]
    let (mtime, mtime_nanos, mode) = {
        use std::os::unix::fs::MetadataExt;
        (meta.mtime(), meta.mtime_nsec().clamp(0, 999_999_999) as u32, meta.mode() & 0o777)
    };
    #[cfg(not(unix))]
    let (mtime, mtime_nanos, mode) = {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        (mtime, 0, 0o644)
    };

    let file_type = meta.file_type();
    let (kind, size, link_target) = if file_type.is_symlink() {
        let target = std::fs::read_link(path)?;
        let target_text = target
            .to_str()
            .ok_or_else(|| FileError::Denied(format!("symlink target is not valid text: {}", path.display())))?
            .to_owned();
        if target.is_absolute() {
            return Err(FileError::Denied(format!(
                "symlink escapes selection: {}",
                path.display()
            )));
        }
        (EntryKind::Symlink, 0, Some(target_text))
    } else if file_type.is_dir() {
        (EntryKind::Dir, 0, None)
    } else if file_type.is_file() {
        (EntryKind::File, meta.len(), None)
    } else {
        return Err(FileError::Denied(format!(
            "unsupported special file: {}",
            path.display()
        )));
    };

    tree.manifest.entries.push(FileEntry {
        id,
        parent,
        name: name.clone(),
        kind,
        size,
        mtime,
        mtime_nanos,
        mode,
        link_target,
    });
    tree.paths.insert(id, path.to_path_buf());

    if kind == EntryKind::Dir {
        let remaining = MAX_ENTRIES - tree.manifest.entries.len();
        let mut children = Vec::new();
        for child in std::fs::read_dir(path)? {
            if children.len() >= remaining {
                return Err(FileError::LimitExceeded(format!(
                    "selection exceeds {MAX_ENTRIES} entries"
                )));
            }
            children.push(child?);
        }
        children.sort_by_key(|c| c.file_name());
        let mut folded: HashSet<String> = HashSet::new();
        for child in children {
            let child_name = child
                .file_name()
                .into_string()
                .map_err(|name| FileError::Unavailable(format!("unsupported non-text name under {}: {name:?}", path.display())))?;
            if !folded.insert(child_name.to_lowercase()) {
                return Err(FileError::LimitExceeded(format!(
                    "case-colliding names under {}",
                    path.display()
                )));
            }
            add_entry(tree, &child.path(), Some(id), child_name, depth + 1)?;
        }
    }
    Ok(id)
}

fn validate_links(tree: &SourceTree) -> Result<(), FileError> {
    let entries = &tree.manifest.entries;
    let by_id: HashMap<EntryId, &FileEntry> = entries.iter().map(|entry| (entry.id, entry)).collect();
    let mut by_parent: HashMap<Option<EntryId>, HashMap<&str, &FileEntry>> = HashMap::new();
    for entry in entries {
        by_parent.entry(entry.parent).or_default().insert(entry.name.as_str(), entry);
    }
    for entry in entries {
        let EntryKind::Symlink = entry.kind else { continue };
        let Some(target) = entry.link_target.as_deref() else { continue };
        resolve_link(entry, target, &by_id, &by_parent)?;
    }
    Ok(())
}

fn dir_stack<'a>(mut entry: Option<&'a FileEntry>, by_id: &HashMap<EntryId, &'a FileEntry>) -> Vec<EntryId> {
    let mut stack = Vec::new();
    while let Some(current) = entry {
        stack.push(current.id);
        entry = current.parent.and_then(|parent| by_id.get(&parent).copied());
    }
    stack.reverse();
    stack
}

fn resolve_link(
    link: &FileEntry,
    target: &str,
    by_id: &HashMap<EntryId, &FileEntry>,
    by_parent: &HashMap<Option<EntryId>, HashMap<&str, &FileEntry>>,
) -> Result<(), FileError> {
    let escape = || FileError::Denied(format!("symlink escapes selection: {}", link.name));
    let parts = |text: &str| -> Vec<String> { text.split('/').filter(|part| !part.is_empty() && *part != ".").map(str::to_owned).collect() };
    let mut stack = dir_stack(link.parent.and_then(|parent| by_id.get(&parent).copied()), by_id);
    let mut unresolved = 0usize;
    let mut pending: VecDeque<String> = parts(target).into();
    let mut expansions = 0usize;
    while let Some(component) = pending.pop_front() {
        if component == ".." {
            if unresolved > 0 {
                unresolved -= 1;
            } else if stack.pop().is_none() {
                return Err(escape());
            }
            continue;
        }
        let child = by_parent.get(&stack.last().copied()).and_then(|siblings| siblings.get(component.as_str()).copied());
        match child {
            Some(entry) if entry.kind == EntryKind::Symlink => {
                expansions += 1;
                if expansions > MAX_LINK_EXPANSIONS {
                    return Err(FileError::Denied(format!("symlink chain is too deep: {}", link.name)));
                }
                let nested = parts(entry.link_target.as_deref().unwrap_or_default());
                stack = dir_stack(entry.parent.and_then(|parent| by_id.get(&parent).copied()), by_id);
                unresolved = 0;
                for part in nested.into_iter().rev() {
                    pending.push_front(part);
                }
            }
            Some(entry) => stack.push(entry.id),
            None => unresolved += 1,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_tree_with_dirs_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("sub/empty")).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        std::fs::write(root.join("sub/b.bin"), vec![0u8; 100]).unwrap();
        let tree = from_paths(FileOfferId::new(), "test", &[root]).unwrap();
        assert_eq!(tree.manifest.entries.len(), 5);
        let roots: Vec<_> = tree.manifest.roots().collect();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "root");
        assert_eq!(roots[0].kind, EntryKind::Dir);
    }

    #[test]
    fn rejects_case_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Name.txt"), b"a").unwrap();
        std::fs::write(root.join("name.txt"), b"b").unwrap();
        let err = from_paths(FileOfferId::new(), "test", &[root]).unwrap_err();
        assert!(matches!(err, FileError::LimitExceeded(_)));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_relative_in_tree_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("real.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("real.txt", root.join("link.txt")).unwrap();
        let tree = from_paths(FileOfferId::new(), "test", &[root]).unwrap();
        let link = tree
            .manifest
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::Symlink)
            .unwrap();
        assert_eq!(link.link_target.as_deref(), Some("real.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_chained_in_tree_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/real.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("sub", root.join("alias")).unwrap();
        std::os::unix::fs::symlink("alias/real.txt", root.join("via.txt")).unwrap();
        let tree = from_paths(FileOfferId::new(), "test", &[root]).unwrap();
        assert_eq!(tree.manifest.entries.len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_escaping_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("../../outside", root.join("evil")).unwrap();
        let err = from_paths(FileOfferId::new(), "test", &[root]).unwrap_err();
        assert!(matches!(err, FileError::Denied(_)));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_through_an_alias_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("real.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(".", root.join("alias")).unwrap();
        let mut target = String::new();
        for _ in 0..10 {
            target.push_str("alias/");
        }
        for _ in 0..10 {
            target.push_str("../");
        }
        target.push_str("etc/passwd");
        std::os::unix::fs::symlink(&target, root.join("evil")).unwrap();
        let err = from_paths(FileOfferId::new(), "test", &[root]).unwrap_err();
        assert!(matches!(err, FileError::Denied(_)), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_loops() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        let err = from_paths(FileOfferId::new(), "test", &[root]).unwrap_err();
        assert!(matches!(err, FileError::Denied(_)), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_names() {
        use std::os::unix::ffi::OsStringExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let bad = std::ffi::OsString::from_vec(b"bad-\xff".to_vec());
        std::fs::write(root.join(&bad), b"x").unwrap();
        let err = from_paths(FileOfferId::new(), "test", &[root]).unwrap_err();
        assert!(matches!(err, FileError::Unavailable(_)), "{err}");
    }

    #[test]
    fn enumeration_budget_stops_oversized_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let budget = MAX_ENTRIES - 1;
        for index in 0..budget + 8 {
            std::fs::write(root.join(format!("f{index:06}")), b"").unwrap();
        }
        let err = from_paths(FileOfferId::new(), "test", &[root]).unwrap_err();
        assert!(matches!(err, FileError::LimitExceeded(_)), "{err}");
    }
}
