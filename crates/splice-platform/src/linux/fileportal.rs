use std::collections::HashMap;
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;

use zbus::zvariant::{Fd, Value};

use crate::{PlatformError, Result};

use super::portal::err_ctx;

pub const DOCUMENTS_NAME: &str = "org.freedesktop.portal.Documents";
pub const DOCUMENTS_PATH: &str = "/org/freedesktop/portal/documents";
pub const FILETRANSFER_IFACE: &str = "org.freedesktop.portal.FileTransfer";

type Options = HashMap<&'static str, Value<'static>>;

async fn proxy(conn: &zbus::Connection) -> Result<zbus::Proxy<'static>> {
    zbus::Proxy::new(conn, DOCUMENTS_NAME, DOCUMENTS_PATH, FILETRANSFER_IFACE)
        .await
        .map_err(err_ctx("filetransfer proxy"))
}

async fn version(conn: &zbus::Connection) -> u32 {
    match proxy(conn).await {
        Ok(p) => p.get_property::<u32>("version").await.unwrap_or(0),
        Err(_) => 0,
    }
}

pub async fn probe(conn: &zbus::Connection) -> bool {
    if version(conn).await < 1 {
        return false;
    }
    let Ok(proxy) = proxy(conn).await else { return false };
    match begin_transfer(std::sync::Arc::new(proxy), Vec::new()).await {
        Ok(export) => {
            export.stop().await;
            true
        }
        Err(_) => false,
    }
}

#[derive(Debug)]
pub struct PortalExport {
    key: String,
    release: Option<tokio::sync::oneshot::Sender<()>>,
    stopped: tokio::sync::oneshot::Receiver<()>,
}

pub const ADD_FILES_BATCH: usize = 8;

#[async_trait::async_trait]
trait TransferPortal: Send + Sync {
    async fn start(&self) -> Result<String>;
    async fn add(&self, key: &str, fds: &[Fd<'_>]) -> Result<()>;
    async fn stop(&self, key: &str) -> Result<()>;
}

#[async_trait::async_trait]
impl TransferPortal for zbus::Proxy<'static> {
    async fn start(&self) -> Result<String> {
        let mut opts = Options::new();
        opts.insert("autostop", Value::from(false));
        self.call::<_, _, String>("StartTransfer", &(opts,))
            .await
            .map_err(err_ctx("StartTransfer"))
    }

    async fn add(&self, key: &str, fds: &[Fd<'_>]) -> Result<()> {
        self.call::<_, _, ()>("AddFiles", &(key, fds, Options::new()))
            .await
            .map_err(err_ctx("AddFiles"))
    }

    async fn stop(&self, key: &str) -> Result<()> {
        self.call::<_, _, ()>("StopTransfer", &(key,))
            .await
            .map_err(err_ctx("StopTransfer"))
    }
}

async fn begin_transfer(portal: std::sync::Arc<dyn TransferPortal>, fds: Vec<OwnedFd>) -> Result<PortalExport> {
    let (mut reply, receive) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let key = match portal.start().await {
            Ok(key) => key,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let fds: Vec<Fd> = fds.into_iter().map(Fd::from).collect();
        let added = tokio::select! {
            biased;
            _ = reply.closed() => Ok(false),
            result = async {
                for batch in fds.chunks(ADD_FILES_BATCH) {
                    portal.add(&key, batch).await?;
                }
                Result::Ok(true)
            } => result,
        };
        let (release, released) = tokio::sync::oneshot::channel();
        let (stopped, stop_complete) = tokio::sync::oneshot::channel();
        let failure = match added {
            Ok(true) => {
                let export = PortalExport { key: key.clone(), release: Some(release), stopped: stop_complete };
                let _ = reply.send(Ok(export));
                let _ = released.await;
                None
            }
            Ok(false) => None,
            Err(error) => Some((reply, error)),
        };
        if let Err(error) = portal.stop(&key).await {
            tracing::warn!(%error, "portal transfer cleanup failed");
        }
        let _ = stopped.send(());
        if let Some((reply, error)) = failure {
            let _ = reply.send(Err(error));
        }
    });
    receive.await.map_err(|_| PlatformError::Unavailable("portal export task stopped".into()))?
}

impl PortalExport {
    pub async fn begin(conn: &zbus::Connection, fds: Vec<OwnedFd>) -> Result<PortalExport> {
        if fds.is_empty() {
            return Err(PlatformError::Other(anyhow::anyhow!("portal export requires at least one fd")));
        }
        begin_transfer(std::sync::Arc::new(proxy(conn).await?), fds).await
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub async fn stop(mut self) {
        self.release.take();
        let _ = (&mut self.stopped).await;
    }
}

pub async fn retrieve(conn: &zbus::Connection, key: &str) -> Result<Vec<PathBuf>> {
    let proxy = proxy(conn).await?;
    let opts = Options::new();
    let reply = proxy
        .call_method("RetrieveFiles", &(key, opts))
        .await
        .map_err(err_ctx("RetrieveFiles"))?;
    let paths = reply
        .body()
        .deserialize::<Vec<String>>()
        .map_err(err_ctx("RetrieveFiles reply"))?;
    Ok(paths.into_iter().map(PathBuf::from).collect())
}

pub fn open_held(paths: &[PathBuf]) -> Result<Vec<OwnedFd>> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let file = std::fs::File::open(path).map_err(|e| {
            PlatformError::Permission(format!("open portal-granted {}: {e}", path.display()))
        })?;
        out.push(OwnedFd::from(file));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Semaphore};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        Start,
        Add(usize),
        Stop,
    }

    struct MockPortal {
        calls: parking_lot::Mutex<Vec<Step>>,
        events: mpsc::UnboundedSender<Step>,
        blocked: Option<Step>,
        failed: Option<Step>,
        gate: Semaphore,
    }

    impl MockPortal {
        fn new(blocked: Option<Step>, failed: Option<Step>) -> (Arc<Self>, mpsc::UnboundedReceiver<Step>) {
            let (events, rx) = mpsc::unbounded_channel();
            (Arc::new(Self { calls: Default::default(), events, blocked, failed, gate: Semaphore::new(0) }), rx)
        }

        async fn call(&self, step: Step) -> Result<()> {
            self.calls.lock().push(step);
            self.events.send(step).unwrap();
            if self.blocked == Some(step) {
                self.gate.acquire().await.unwrap().forget();
            }
            if self.failed == Some(step) {
                return Err(PlatformError::Unavailable("injected portal failure".into()));
            }
            Ok(())
        }

        fn stops(&self) -> usize {
            self.calls.lock().iter().filter(|step| **step == Step::Stop).count()
        }
    }

    #[async_trait::async_trait]
    impl TransferPortal for MockPortal {
        async fn start(&self) -> Result<String> {
            self.call(Step::Start).await?;
            Ok("owned-transfer".into())
        }

        async fn add(&self, key: &str, fds: &[Fd<'_>]) -> Result<()> {
            assert_eq!(key, "owned-transfer");
            assert!(!fds.is_empty() && fds.len() <= ADD_FILES_BATCH);
            let batch = self.calls.lock().iter().filter(|step| matches!(step, Step::Add(_))).count() + 1;
            self.call(Step::Add(batch)).await
        }

        async fn stop(&self, key: &str) -> Result<()> {
            assert_eq!(key, "owned-transfer");
            self.call(Step::Stop).await
        }
    }

    fn descriptors(count: usize) -> Vec<OwnedFd> {
        (0..count).map(|_| std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap().into()).collect()
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Step>) -> Step {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn cancellation_during_start_still_owns_the_returned_key() {
        let (portal, mut events) = MockPortal::new(Some(Step::Start), None);
        let task = tokio::spawn(begin_transfer(portal.clone(), descriptors(1)));
        assert_eq!(next(&mut events).await, Step::Start);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        portal.gate.add_permits(1);
        assert_eq!(next(&mut events).await, Step::Stop);
        assert_eq!(*portal.calls.lock(), vec![Step::Start, Step::Stop]);
    }

    #[tokio::test]
    async fn cancellation_during_each_add_batch_stops_once() {
        for blocked in 1..=2 {
            let (portal, mut events) = MockPortal::new(Some(Step::Add(blocked)), None);
            let task = tokio::spawn(begin_transfer(portal.clone(), descriptors(ADD_FILES_BATCH + 1)));
            assert_eq!(next(&mut events).await, Step::Start);
            for batch in 1..=blocked {
                assert_eq!(next(&mut events).await, Step::Add(batch));
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(next(&mut events).await, Step::Stop);
            assert_eq!(portal.stops(), 1);
        }
    }

    #[tokio::test]
    async fn each_add_failure_stops_once_before_returning() {
        for failed in 1..=2 {
            let (portal, _events) = MockPortal::new(None, Some(Step::Add(failed)));
            assert!(begin_transfer(portal.clone(), descriptors(ADD_FILES_BATCH + 1)).await.is_err());
            assert_eq!(portal.stops(), 1);
            assert_eq!(portal.calls.lock().last(), Some(&Step::Stop));
        }
    }

    #[tokio::test]
    async fn failed_start_has_no_transfer_to_stop() {
        let (portal, _events) = MockPortal::new(None, Some(Step::Start));
        assert!(begin_transfer(portal.clone(), descriptors(1)).await.is_err());
        assert_eq!(portal.stops(), 0);
    }

    #[tokio::test]
    async fn successful_export_is_owned_until_replaced_or_released() {
        let (portal, mut events) = MockPortal::new(None, None);
        let export = begin_transfer(portal.clone(), descriptors(1)).await.unwrap();
        assert_eq!(export.key(), "owned-transfer");
        assert_eq!(portal.stops(), 0);
        assert_eq!(next(&mut events).await, Step::Start);
        assert_eq!(next(&mut events).await, Step::Add(1));
        drop(export);
        assert_eq!(next(&mut events).await, Step::Stop);
        assert_eq!(portal.stops(), 1);
        let (portal, _events) = MockPortal::new(None, None);
        begin_transfer(portal.clone(), descriptors(1)).await.unwrap().stop().await;
        assert_eq!(portal.stops(), 1);
    }

    #[tokio::test]
    async fn cancellation_of_explicit_stop_does_not_cancel_cleanup() {
        let (portal, mut events) = MockPortal::new(Some(Step::Stop), None);
        let export = begin_transfer(portal.clone(), descriptors(1)).await.unwrap();
        let task = tokio::spawn(export.stop());
        assert_eq!(next(&mut events).await, Step::Start);
        assert_eq!(next(&mut events).await, Step::Add(1));
        assert_eq!(next(&mut events).await, Step::Stop);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        portal.gate.add_permits(1);
        assert_eq!(portal.stops(), 1);
    }
}
