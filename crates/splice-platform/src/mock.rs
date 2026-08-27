//! Scriptable in-memory platform for `splice-core` tests and the integration smoke test.
//!
//! - Records every call (edges set, capture begin/end, injected events, clipboard ops).
//! - Test code drives the engine by pushing [`PlatformEvent`]s through the handle.

use crate::{
    Capture, ClipFetch, Clipboard, ClipboardOffer, EdgeSpec, Emulate, Platform, PlatformEvent,
    Result,
};
use parking_lot::Mutex;
use splice_proto::{DisplayRect, InputEvent, Vec2};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Default)]
pub struct MockState {
    pub edges: Vec<EdgeSpec>,
    pub capturing: bool,
    pub capture_ends: Vec<Option<Vec2>>,
    pub entered: Vec<Vec2>,
    pub injected: Vec<InputEvent>,
    pub left: usize,
    pub release_all_calls: usize,
    pub remote_offers: Vec<ClipboardOffer>,
    pub local_clip: std::collections::HashMap<String, Vec<u8>>,
}

#[derive(Clone)]
pub struct MockHandle {
    pub state: Arc<Mutex<MockState>>,
    pub events: mpsc::UnboundedSender<PlatformEvent>,
    /// Last fetch callback given to `set_remote_offer` (drive it to simulate a paste).
    pub last_fetch: Arc<Mutex<Option<Arc<dyn ClipFetch>>>>,
}

struct MockCapture(MockHandle);
struct MockEmulate(MockHandle);
struct MockClipboard(MockHandle);

#[async_trait::async_trait]
impl Capture for MockCapture {
    async fn set_edges(&self, edges: Vec<EdgeSpec>) -> Result<()> {
        self.0.state.lock().edges = edges;
        Ok(())
    }
    async fn begin_capture(&self) -> Result<()> {
        self.0.state.lock().capturing = true;
        Ok(())
    }
    async fn end_capture(&self, warp_to: Option<Vec2>) -> Result<()> {
        let mut st = self.0.state.lock();
        st.capturing = false;
        st.capture_ends.push(warp_to);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Emulate for MockEmulate {
    async fn enter(&self, pos: Vec2) -> Result<()> {
        self.0.state.lock().entered.push(pos);
        Ok(())
    }
    async fn inject(&self, ev: InputEvent) -> Result<()> {
        self.0.state.lock().injected.push(ev);
        Ok(())
    }
    async fn leave(&self) -> Result<()> {
        self.0.state.lock().left += 1;
        Ok(())
    }
    async fn release_all(&self) -> Result<()> {
        self.0.state.lock().release_all_calls += 1;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Clipboard for MockClipboard {
    async fn set_remote_offer(
        &self,
        offer: ClipboardOffer,
        fetch: Arc<dyn ClipFetch>,
    ) -> Result<()> {
        self.0.state.lock().remote_offers.push(offer);
        *self.0.last_fetch.lock() = Some(fetch);
        Ok(())
    }
    async fn read_local(&self, mime: &str) -> Result<Vec<u8>> {
        Ok(self
            .0
            .state
            .lock()
            .local_clip
            .get(mime)
            .cloned()
            .unwrap_or_default())
    }
}

/// Build a mock platform with the given displays. Returns the platform (hand to the engine)
/// and a handle for the test to inspect state and push events.
pub fn create(displays: Vec<DisplayRect>) -> (Platform, MockHandle) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = MockHandle {
        state: Arc::new(Mutex::new(MockState::default())),
        events: tx,
        last_fetch: Arc::new(Mutex::new(None)),
    };
    let platform = Platform {
        capture: Arc::new(MockCapture(handle.clone())),
        emulate: Arc::new(MockEmulate(handle.clone())),
        clipboard: Arc::new(MockClipboard(handle.clone())),
        displays,
        events: rx,
    };
    (platform, handle)
}

/// Convenience: a 1920x1080 single display at origin.
pub fn one_display() -> Vec<DisplayRect> {
    vec![DisplayRect { id: "d0".into(), x: 0, y: 0, w: 1920, h: 1080, scale: 1.0 }]
}
