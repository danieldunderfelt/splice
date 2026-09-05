pub mod hid;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod shortcut;
mod usages;

use crate::Result;
use splice_proto::raw::RawReport;
use tokio::sync::mpsc;

pub trait RawCapture: Send + Sync {
    fn readiness(&self) -> Result<()>;
    fn begin(&self, output: mpsc::Sender<RawReport>, edge: Option<u32>) -> Result<()>;
    fn end(&self);
}

#[async_trait::async_trait]
pub trait RawEmulate: Send + Sync {
    async fn prepare(&self) -> Result<()>;
    fn begin(&self, session: u64) -> Result<()>;
    fn inject(&self, session: u64, report: &RawReport) -> Result<()>;
    fn end(&self, session: u64) -> Result<()>;
}
