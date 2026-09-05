pub mod hid;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) mod shortcut;
mod usages;

use crate::Result;
use splice_proto::raw::RawReport;
use tokio::sync::mpsc;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct RawOperation {
    error: parking_lot::Mutex<Option<String>>,
}

impl RawOperation {
    pub fn fail(&self, reason: String) {
        *self.error.lock() = Some(reason);
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }
}

pub trait RawCapture: Send + Sync {
    fn prepare(&self) -> Result<()>;
    fn begin(
        &self,
        output: mpsc::Sender<RawReport>,
        edge: Option<u32>,
        operation: Arc<RawOperation>,
    ) -> Result<()>;
    fn end(&self);
}

#[async_trait::async_trait]
pub trait RawEmulate: Send + Sync {
    async fn prepare(&self) -> Result<()>;
    fn begin(&self, session: u64) -> Result<()>;
    fn inject(&self, session: u64, report: &RawReport) -> Result<()>;
    fn end(&self, session: u64) -> Result<()>;
}
