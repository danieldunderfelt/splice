use std::os::unix::io::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

use crate::files::{
    self, MIME_GNOME_COPIED_FILES, MIME_PORTAL_FILETRANSFER, MIME_URI_LIST,
};
use crate::{Clipboard, PlatformError, Result};

use super::fileportal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileSelectionProbe {
    GnomeCopiedFiles,
    UriList,
    PortalKey,
}

pub fn probe_mimes(mimes: &[String]) -> Option<FileSelectionProbe> {
    let has = |want: &str| mimes.iter().any(|m| m == want);
    if has(MIME_GNOME_COPIED_FILES) {
        Some(FileSelectionProbe::GnomeCopiedFiles)
    } else if has(MIME_URI_LIST) {
        Some(FileSelectionProbe::UriList)
    } else if has(MIME_PORTAL_FILETRANSFER) {
        Some(FileSelectionProbe::PortalKey)
    } else {
        None
    }
}

pub struct FileSelection {
    pub paths: Vec<PathBuf>,
    pub cut_requested: bool,
    pub portal_fds: Vec<OwnedFd>,
}

pub async fn read_selection(
    clipboard: &Arc<dyn Clipboard>,
    conn: &zbus::Connection,
    mimes: &[String],
) -> Result<Option<FileSelection>> {
    let Some(probe) = probe_mimes(mimes) else {
        return Ok(None);
    };
    match probe {
        FileSelectionProbe::GnomeCopiedFiles => {
            let data = clipboard.read_local(MIME_GNOME_COPIED_FILES).await?;
            let Some((cut, paths)) = files::parse_gnome_copied_files(&data) else {
                return Err(PlatformError::Other(anyhow::anyhow!(
                    "malformed {MIME_GNOME_COPIED_FILES} selection"
                )));
            };
            if paths.is_empty() {
                return Ok(None);
            }
            Ok(Some(FileSelection {
                paths,
                cut_requested: cut,
                portal_fds: Vec::new(),
            }))
        }
        FileSelectionProbe::UriList => {
            let data = clipboard.read_local(MIME_URI_LIST).await?;
            let paths = files::parse_uri_list(&data);
            if paths.is_empty() {
                return Ok(None);
            }
            Ok(Some(FileSelection {
                paths,
                cut_requested: false,
                portal_fds: Vec::new(),
            }))
        }
        FileSelectionProbe::PortalKey => {
            let data = clipboard.read_local(MIME_PORTAL_FILETRANSFER).await?;
            let key = String::from_utf8(data)
                .map_err(|_| PlatformError::Other(anyhow::anyhow!("portal key is not utf-8")))?;
            let key = key.trim_matches(|c| c == '\0' || c == '\n' || c == '\r').to_owned();
            if key.is_empty() {
                return Ok(None);
            }
            let paths = fileportal::retrieve(conn, &key).await?;
            let portal_fds = fileportal::open_held(&paths)?;
            Ok(Some(FileSelection {
                paths,
                cut_requested: false,
                portal_fds,
            }))
        }
    }
}

pub fn completed_offer(paths: &[PathBuf]) -> Vec<(String, Vec<u8>)> {
    files::completed_representations(paths)
        .into_iter()
        .map(|(mime, bytes)| (mime.to_owned(), bytes))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_prefers_gnome_format() {
        let mimes = vec![
            MIME_URI_LIST.to_owned(),
            MIME_GNOME_COPIED_FILES.to_owned(),
        ];
        assert_eq!(probe_mimes(&mimes), Some(FileSelectionProbe::GnomeCopiedFiles));
    }

    #[test]
    fn probe_falls_back_to_uri_list_then_portal() {
        assert_eq!(
            probe_mimes(&["text/plain".into(), MIME_URI_LIST.into()]),
            Some(FileSelectionProbe::UriList)
        );
        assert_eq!(
            probe_mimes(&[MIME_PORTAL_FILETRANSFER.into()]),
            Some(FileSelectionProbe::PortalKey)
        );
        assert_eq!(probe_mimes(&["text/plain".into()]), None);
    }

    #[test]
    fn completed_offer_mimes() {
        let offer = completed_offer(&[PathBuf::from("/cache/x.txt")]);
        let mimes: Vec<&str> = offer.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            mimes,
            vec![MIME_URI_LIST, MIME_GNOME_COPIED_FILES, files::MIME_KDE_CUTSELECTION]
        );
    }
}
