use std::fmt;
use std::path::{Path, PathBuf};

use percent_encoding::{percent_decode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};

pub const MIME_URI_LIST: &str = "text/uri-list";
pub const MIME_GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
pub const MIME_KDE_CUTSELECTION: &str = "application/x-kde-cutselection";
pub const MIME_PORTAL_FILETRANSFER: &str = "application/vnd.portal.filetransfer";
pub const KDE_CUTSELECTION_COPY: &[u8] = b"0";

const PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

macro_rules! file_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(pub u128);

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse::<$name>()
                    .map_err(|_| serde::de::Error::custom("invalid file id"))
            }
        }

        impl $name {
            pub fn new() -> Self {
                Self(rand::random())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:032x})"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:032x}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = ();
            fn from_str(s: &str) -> std::result::Result<Self, ()> {
                u128::from_str_radix(s, 16).map(Self).map_err(|_| ())
            }
        }
    };
}

file_id!(FileOfferId);
file_id!(EntryId);
file_id!(ViewId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: EntryId,
    pub parent: Option<EntryId>,
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nanos: u32,
    pub mode: u32,
    pub link_target: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileManifest {
    pub offer: FileOfferId,
    pub origin: String,
    pub entries: Vec<FileEntry>,
}

impl FileManifest {
    pub fn roots(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries.iter().filter(|e| e.parent.is_none())
    }

    pub fn entry(&self, id: EntryId) -> Option<&FileEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn children(&self, id: EntryId) -> impl Iterator<Item = &FileEntry> {
        self.entries.iter().filter(move |e| e.parent == Some(id))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewState {
    Offered,
    DroppedAwaitingRead,
    Committed,
    Retired,
}

#[derive(Clone, Debug, Default)]
pub struct ViewStats {
    pub bytes_served: u64,
    pub reads: u64,
    pub denied_reads: u64,
    pub commits: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("access denied: {0}")]
    Denied(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("source changed: {0}")]
    SourceChanged(String),
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("cancelled")]
    Cancelled,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub trait FileContentSource: Send + Sync {
    fn materialize(&self, view: ViewId, entry: EntryId) -> Result<PathBuf, FileError>;
    fn on_commit(&self, view: ViewId);
    fn on_cancel(&self, view: ViewId, committed: bool);
}

fn os_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.as_os_str().to_string_lossy().into_owned().into_bytes()
    }
}

fn os_from_bytes(bytes: Vec<u8>) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
    }
}

pub fn encode_file_uri(path: &Path) -> String {
    let bytes = os_bytes(path);
    let encoded = percent_encoding::percent_encode(&bytes, PATH_ENCODE_SET);
    format!("file://{encoded}")
}

pub fn decode_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path_part = match rest.find('/') {
        Some(0) => rest,
        Some(_) => {
            let (host, path) = rest.split_at(rest.find('/').unwrap());
            if host.eq_ignore_ascii_case("localhost") {
                path
            } else {
                return None;
            }
        }
        None => return None,
    };
    let decoded = percent_decode(path_part.as_bytes()).collect::<Vec<u8>>();
    if decoded.is_empty() || decoded[0] != b'/' {
        return None;
    }
    Some(os_from_bytes(decoded))
}

pub fn encode_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        out.push_str(&encode_file_uri(path));
        out.push_str("\r\n");
    }
    out
}

pub fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(data);
    text.split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(decode_file_uri)
        .collect()
}

pub fn encode_gnome_copied_files(paths: &[PathBuf]) -> String {
    let mut out = String::from("copy\n");
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&encode_file_uri(path));
    }
    out
}

pub fn parse_gnome_copied_files(data: &[u8]) -> Option<(bool, Vec<PathBuf>)> {
    let text = String::from_utf8_lossy(data);
    let mut lines = text.split('\n');
    let cut = match lines.next()? {
        "copy" => false,
        "cut" => true,
        _ => return None,
    };
    let paths = lines
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
        .filter_map(decode_file_uri)
        .collect();
    Some((cut, paths))
}

pub fn completed_representations(paths: &[PathBuf]) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (MIME_URI_LIST, encode_uri_list(paths).into_bytes()),
        (MIME_GNOME_COPIED_FILES, encode_gnome_copied_files(paths).into_bytes()),
        (MIME_KDE_CUTSELECTION, KDE_CUTSELECTION_COPY.to_vec()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn uri_list_round_trip_simple() {
        let input = paths(&["/home/user/report.pdf", "/tmp/a"]);
        let encoded = encode_uri_list(&input);
        assert_eq!(encoded, "file:///home/user/report.pdf\r\nfile:///tmp/a\r\n");
        assert_eq!(parse_uri_list(encoded.as_bytes()), input);
    }

    #[test]
    fn uri_list_round_trip_special_chars() {
        let input = paths(&[
            "/home/user/my file (final).txt",
            "/data/100% legit/quo\"te",
            "/unicode/ære blåbær/日本語.txt",
            "/newline/in\nname",
        ]);
        let encoded = encode_uri_list(&input);
        assert!(!encoded.contains(' '));
        assert!(encoded.contains("100%25%20legit"));
        assert_eq!(parse_uri_list(encoded.as_bytes()), input);
    }

    #[test]
    fn uri_list_skips_comments_and_rejects_non_file() {
        let data = b"# comment\r\nfile:///a\r\n\nhttps://evil.example/x\r\nsftp://host/y\r\nfile://otherhost/z\r\n";
        assert_eq!(parse_uri_list(data), paths(&["/a"]));
    }

    #[test]
    fn uri_list_accepts_localhost_host() {
        assert_eq!(parse_uri_list(b"file://localhost/etc/hostname\r\n"), paths(&["/etc/hostname"]));
    }

    #[test]
    fn uri_list_rejects_relative() {
        assert_eq!(parse_uri_list(b"file://relative/path\r\n"), Vec::<PathBuf>::new());
    }

    #[test]
    fn gnome_copied_files_copy_round_trip() {
        let input = paths(&["/a b/c.txt", "/d"]);
        let encoded = encode_gnome_copied_files(&input);
        assert_eq!(encoded, "copy\nfile:///a%20b/c.txt\nfile:///d");
        assert_eq!(parse_gnome_copied_files(encoded.as_bytes()), Some((false, input)));
    }

    #[test]
    fn gnome_copied_files_parses_cut() {
        let data = b"cut\nfile:///tmp/x\n";
        assert_eq!(parse_gnome_copied_files(data), Some((true, paths(&["/tmp/x"]))));
    }

    #[test]
    fn gnome_copied_files_rejects_unknown_marker() {
        assert_eq!(parse_gnome_copied_files(b"move\nfile:///a"), None);
    }

    #[test]
    fn completed_offer_has_three_representations() {
        let reps = completed_representations(&paths(&["/cache/offer/file.txt"]));
        assert_eq!(reps.len(), 3);
        assert_eq!(reps[0].0, MIME_URI_LIST);
        assert_eq!(reps[1].0, MIME_GNOME_COPIED_FILES);
        assert_eq!(reps[2].0, MIME_KDE_CUTSELECTION);
        assert_eq!(reps[2].1, b"0");
        assert!(reps[1].1.starts_with(b"copy\n"));
    }

    #[test]
    fn ids_are_random_and_parseable() {
        let a = ViewId::new();
        let b = ViewId::new();
        assert_ne!(a, b);
        assert_eq!(a.to_string().parse::<ViewId>(), Ok(a));
    }

    #[test]
    fn ids_serialize_as_hex_strings() {
        let id = ViewId(0x0123456789abcdef0123456789abcdef);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"0123456789abcdef0123456789abcdef\"");
        assert_eq!(serde_json::from_str::<ViewId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ViewId>("170141183460469231731687303715884105727").is_err());
    }
}
