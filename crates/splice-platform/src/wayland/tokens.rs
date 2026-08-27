//! Portal restore-token persistence (`data_dir/tokens.json`, atomic write).
//!
//! Restore tokens are SINGLE-USE: the portal hands out a replacement on every successful
//! Start, and the replacement must be persisted immediately or the next launch re-prompts
//! for consent (docs/research/wayland-input.md).

use parking_lot::Mutex;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    InputCapture,
    RemoteDesktop,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct TokenFile {
    input_capture: Option<String>,
    remote_desktop: Option<String>,
}

pub struct TokenStore {
    path: PathBuf,
    inner: Mutex<TokenFile>,
}

impl TokenStore {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("tokens.json");
        let inner = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, inner: Mutex::new(inner) }
    }

    pub fn get(&self, kind: TokenKind) -> Option<String> {
        let inner = self.inner.lock();
        match kind {
            TokenKind::InputCapture => inner.input_capture.clone(),
            TokenKind::RemoteDesktop => inner.remote_desktop.clone(),
        }
    }

    /// Stores the replacement token and persists it immediately (tokens are single-use).
    pub fn set(&self, kind: TokenKind, token: String) {
        {
            let mut inner = self.inner.lock();
            match kind {
                TokenKind::InputCapture => inner.input_capture = Some(token),
                TokenKind::RemoteDesktop => inner.remote_desktop = Some(token),
            }
        }
        self.persist();
    }

    /// Drops a token that the portal has rejected or that must force a re-prompt
    /// (e.g. clipboard not granted on a restored RemoteDesktop session).
    pub fn clear(&self, kind: TokenKind) {
        {
            let mut inner = self.inner.lock();
            match kind {
                TokenKind::InputCapture => inner.input_capture = None,
                TokenKind::RemoteDesktop => inner.remote_desktop = None,
            }
        }
        self.persist();
    }

    fn persist(&self) {
        let inner = self.inner.lock();
        let write = || -> io::Result<()> {
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(&*inner).map_err(io::Error::other)?)?;
            std::fs::rename(&tmp, &self.path)
        };
        if let Err(err) = write() {
            tracing::warn!(error = %err, path = %self.path.display(), "failed to persist portal tokens");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_clear() {
        let dir = std::env::temp_dir().join(format!("splice-tokens-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TokenStore::load(&dir);
        assert_eq!(store.get(TokenKind::InputCapture), None);
        store.set(TokenKind::InputCapture, "abc".into());
        store.set(TokenKind::RemoteDesktop, "def".into());

        let reloaded = TokenStore::load(&dir);
        assert_eq!(reloaded.get(TokenKind::InputCapture).as_deref(), Some("abc"));
        assert_eq!(reloaded.get(TokenKind::RemoteDesktop).as_deref(), Some("def"));

        reloaded.clear(TokenKind::InputCapture);
        let reloaded = TokenStore::load(&dir);
        assert_eq!(reloaded.get(TokenKind::InputCapture), None);
        assert_eq!(reloaded.get(TokenKind::RemoteDesktop).as_deref(), Some("def"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
