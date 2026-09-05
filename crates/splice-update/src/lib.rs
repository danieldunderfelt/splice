pub mod control;
pub mod install;
pub mod manifest;

#[cfg(test)]
mod tests;

use anyhow::{ensure, Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use splice_proto::BuildInfo;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Checking,
    Current,
    Available,
    Downloading,
    Ready,
    Restarting,
    Failed,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateState {
    pub build: BuildInfo,
    pub phase: Phase,
    pub version: Option<String>,
    pub downloaded: u64,
    pub total: u64,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub transaction: String,
    pub version: String,
    pub installed: bool,
    pub error: Option<String>,
}

struct Prepared {
    lease: std::fs::File,
    directory: tempfile::TempDir,
    plan: install::Plan,
}

struct Inner {
    directory: PathBuf,
    installation: Option<install::Installation>,
    state: watch::Sender<UpdateState>,
    operation: Arc<Mutex<()>>,
    prepared: Mutex<Option<Prepared>>,
    restart: watch::Sender<bool>,
    transaction: watch::Sender<Option<String>>,
    source: Source,
    helper: fn(&Path) -> Result<()>,
}

struct Source {
    client: reqwest::Client,
    latest: String,
    releases: String,
    key: [u8; 32],
}

impl Source {
    fn github() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .https_only(true)
                .user_agent(concat!("Splice/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(180))
                .build()?,
            latest: format!(
                "https://api.github.com/repos/{}/releases/latest",
                manifest::REPOSITORY
            ),
            releases: format!(
                "https://github.com/{}/releases/download",
                manifest::REPOSITORY
            ),
            key: *include_bytes!("../release-public-key.bin"),
        })
    }
}

#[derive(Clone)]
pub struct Host(Arc<Inner>);

impl Host {
    pub fn new(directory: &Path) -> Result<Self> {
        Self::configured(
            directory,
            install::Installation::detect(&std::env::current_exe()?),
            Source::github()?,
        )
    }

    fn configured(
        directory: &Path,
        detected: Result<install::Installation>,
        source: Source,
    ) -> Result<Self> {
        let directory = directory.join("updates");
        std::fs::create_dir_all(&directory)?;
        let (installation, mut phase, mut message) = match detected {
            Ok(value) => (Some(value), Phase::Idle, None),
            Err(error) => (None, Phase::Unsupported, Some(format!("{error:#}"))),
        };
        let previous = (|| -> Result<Option<Receipt>> {
            match std::fs::read(directory.join("result.json")) {
                Ok(bytes) => Ok(Some(
                    serde_json::from_slice(&bytes).context("invalid persisted update result")?,
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error).context("reading previous update result"),
            }
        })();
        match previous {
            Ok(Some(receipt)) if installation.is_some() => {
                if receipt.error.is_some() {
                    phase = Phase::Failed;
                }
                message = receipt.error.or_else(|| {
                    Some(format!(
                        "{} Splice {}",
                        if receipt.installed {
                            "Installed"
                        } else {
                            "Update pending for"
                        },
                        receipt.version
                    ))
                });
            }
            Err(error) => {
                if installation.is_some() {
                    phase = Phase::Failed;
                }
                message = Some(format!("Cannot load update status: {error:#}"));
            }
            _ => {}
        }
        let state = UpdateState {
            build: BuildInfo::current(),
            phase,
            version: None,
            downloaded: 0,
            total: 0,
            message,
        };
        Ok(Self(Arc::new(Inner {
            directory,
            installation,
            state: watch::channel(state).0,
            operation: Arc::new(Mutex::new(())),
            prepared: Mutex::new(None),
            restart: watch::channel(false).0,
            transaction: watch::channel(None).0,
            source,
            helper: install::launch_helper,
        })))
    }

    pub fn state(&self) -> watch::Receiver<UpdateState> {
        self.0.state.subscribe()
    }
    pub fn restart(&self) -> watch::Receiver<bool> {
        self.0.restart.subscribe()
    }

    fn change(&self, phase: Phase, message: Option<String>) {
        self.0.state.send_modify(|s| {
            s.phase = phase;
            s.message = message;
        });
    }

    fn try_lease(&self) -> Result<Option<std::fs::File>> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.0.directory.join("transaction.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub fn confirm_running(&self) -> Result<()> {
        let path = self.0.directory.join("plan.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.clean_abandoned_staging()
            }
            Err(error) => return Err(error.into()),
        };
        let plan: install::Plan = serde_json::from_slice(&bytes)?;
        let Some(installation) = &self.0.installation else {
            return Ok(());
        };
        ensure!(
            installation.target == plan.installation.target,
            "pending update belongs to a different installation"
        );
        let build = BuildInfo::current();
        let matches =
            build.version == plan.manifest.version && build.commit == plan.manifest.commit;
        if matches {
            install::durable_json(
                &self.0.directory.join("ready.json"),
                &install::Ready {
                    transaction: plan.transaction.clone(),
                    version: build.version.clone(),
                    commit: build.commit,
                },
            )?;
        }
        let Some(_lease) = self.try_lease()? else {
            self.0
                .transaction
                .send_replace(Some(plan.transaction.clone()));
            self.0.state.send_modify(|state| {
                state.phase = Phase::Restarting;
                state.version = Some(plan.manifest.version.clone());
                state.message = Some("Waiting for the update helper to record its result".into());
            });
            return Ok(());
        };
        let error = (!matches).then(|| format!("Update to {} was interrupted. Splice {} is still installed; check for updates to retry", plan.manifest.version, build.version));
        install::durable_json(
            &plan.receipt,
            &Receipt {
                transaction: plan.transaction.clone(),
                version: plan.manifest.version.clone(),
                installed: matches,
                error: error.clone(),
            },
        )?;
        install::cleanup(&plan, &path)?;
        self.change(
            if matches { Phase::Idle } else { Phase::Failed },
            Some(error.unwrap_or_else(|| {
                format!(
                    "Splice {} started successfully after its update",
                    build.version
                )
            })),
        );
        Ok(())
    }

    fn clean_abandoned_staging(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let Some(installation) = &self.0.installation else {
            return Ok(());
        };
        let Some(_lease) = self.try_lease()? else {
            return Ok(());
        };
        for entry in std::fs::read_dir(
            installation
                .target
                .parent()
                .context("missing installation parent")?,
        )? {
            let entry = entry?;
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".splice-update-")
            {
                continue;
            }
            let metadata = entry.path().symlink_metadata()?;
            if metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() } {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }

    pub fn fail(&self, error: String) {
        self.0.restart.send_replace(false);
        self.change(Phase::Failed, Some(error));
    }

    pub fn refresh_result(&self) {
        let state = self.0.state.borrow().clone();
        if state.phase != Phase::Restarting {
            return;
        }
        match std::fs::read(self.0.directory.join("result.json")) {
            Ok(bytes) => match serde_json::from_slice::<Receipt>(&bytes) {
                Ok(receipt)
                    if self.0.transaction.borrow().as_ref() == Some(&receipt.transaction)
                        && state.version.as_ref() == Some(&receipt.version) =>
                {
                    if let Some(error) = receipt.error {
                        self.0.restart.send_replace(false);
                        self.fail(error);
                    } else if receipt.installed {
                        self.change(
                            Phase::Idle,
                            Some(format!("Installed Splice {}", receipt.version)),
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => self.fail(format!("Invalid update result: {error}")),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => self.fail(format!("Cannot read update result: {error}")),
        }
    }

    pub fn request(&self, action: control::Action) -> Result<UpdateState> {
        if action == control::Action::Status {
            self.refresh_result();
            return Ok(self.0.state.borrow().clone());
        }
        ensure!(
            self.0.installation.is_some(),
            "{}",
            self.0
                .state
                .borrow()
                .message
                .as_deref()
                .unwrap_or("this installation cannot update itself")
        );
        ensure!(
            !*self.0.restart.borrow()
                && !(self.0.state.borrow().phase == Phase::Restarting
                    && self.0.transaction.borrow().is_some()),
            "Splice is already restarting for an update"
        );
        let guard = self
            .0
            .operation
            .clone()
            .try_lock_owned()
            .context("an update operation is already in progress")?;
        match &action {
            control::Action::Prepare { version } | control::Action::Install { version } => {
                manifest::version(version)?;
            }
            _ => {}
        }
        self.0.transaction.send_replace(None);
        self.change(
            match action {
                control::Action::Check => Phase::Checking,
                control::Action::Prepare { .. } => Phase::Downloading,
                _ => Phase::Restarting,
            },
            None,
        );
        let host = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let result = match action {
                control::Action::Check => host.check().await,
                control::Action::Prepare { version } => host.prepare(&version).await,
                control::Action::Install { version } => host.install(&version).await,
                control::Action::Status => unreachable!(),
            };
            if let Err(error) = result {
                host.fail(format!("{error:#}"));
            }
        });
        Ok(self.0.state.borrow().clone())
    }

    async fn bytes(&self, url: &str, limit: usize, progress: bool) -> Result<Vec<u8>> {
        let response = self
            .0
            .source
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            response.content_length().is_none_or(|n| n <= limit as u64),
            "download exceeds its size limit"
        );
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= limit,
                "download exceeds its size limit"
            );
            bytes.extend_from_slice(&chunk);
            if progress {
                self.0
                    .state
                    .send_modify(|s| s.downloaded = bytes.len() as u64);
            }
        }
        Ok(bytes)
    }

    async fn release(&self, version: &str) -> Result<manifest::Manifest> {
        manifest::version(version)?;
        let root = format!("{}/v{version}", self.0.source.releases);
        let bytes = self
            .bytes(
                &format!("{root}/splice-update.json"),
                manifest::MAX_MANIFEST,
                false,
            )
            .await?;
        let signature = self
            .bytes(&format!("{root}/splice-update.sig"), 64, false)
            .await?;
        let manifest = manifest::verify(&bytes, &signature, &self.0.source.key)?;
        ensure!(
            manifest.version == version,
            "signed manifest version does not match the requested release"
        );
        Ok(manifest)
    }

    async fn check(&self) -> Result<()> {
        self.0.prepared.lock().await.take();
        #[derive(Deserialize)]
        struct Release {
            tag_name: String,
            draft: bool,
            prerelease: bool,
        }
        let bytes = self
            .bytes(&self.0.source.latest, 1024 * 1024, false)
            .await?;
        let release: Release = serde_json::from_slice(&bytes)?;
        ensure!(
            !release.draft && !release.prerelease,
            "release feed returned an unpublished or prerelease build"
        );
        let version = release
            .tag_name
            .strip_prefix('v')
            .context("release tag must start with v")?;
        let manifest = self.release(version).await?;
        ensure!(
            manifest.assets.contains_key(&BuildInfo::current().target),
            "release does not contain a build for this computer"
        );
        let newer = manifest::version(version)? > manifest::version(&BuildInfo::current().version)?;
        self.0.state.send_modify(|s| {
            s.version = newer.then(|| version.to_string());
            s.phase = if newer {
                Phase::Available
            } else {
                Phase::Current
            };
        });
        Ok(())
    }

    async fn prepare(&self, version: &str) -> Result<()> {
        ensure!(
            manifest::version(version)? > manifest::version(&BuildInfo::current().version)?,
            "updates must be newer than the installed version; downgrades are refused"
        );
        let installation = self
            .0
            .installation
            .as_ref()
            .context("installation cannot update itself")?
            .clone();
        installation.check_writable()?;
        self.0.prepared.lock().await.take();
        let lease = self
            .try_lease()?
            .context("another Splice process is updating this installation")?;
        let manifest = self.release(version).await?;
        let asset = manifest
            .assets
            .get(&BuildInfo::current().target)
            .context("release has no asset for this computer")?;
        self.0.state.send_modify(|s| {
            s.version = Some(version.into());
            s.downloaded = 0;
            s.total = asset.size;
        });
        let bytes = self
            .bytes(
                &format!("{}/v{version}/{}", self.0.source.releases, asset.name),
                asset.size as usize,
                true,
            )
            .await?;
        manifest::verify_archive(&bytes, asset)?;
        let parent = installation
            .target
            .parent()
            .context("missing installation directory")?
            .to_path_buf();
        let bundle = installation.bundle;
        let (directory, staged) = tokio::task::spawn_blocking(move || -> Result<_> {
            let directory = tempfile::Builder::new()
                .prefix(".splice-update-")
                .tempdir_in(parent)?;
            let staged = install::unpack(&bytes, directory.path(), bundle)?;
            Ok((directory, staged))
        })
        .await??;
        install::preflight(&staged, &installation, &manifest).await?;
        let plan = install::Plan {
            transaction: directory
                .path()
                .file_name()
                .context("missing staging identity")?
                .to_string_lossy()
                .into_owned(),
            installation,
            staged,
            manifest,
            parent_pid: std::process::id(),
            receipt: self.0.directory.join("result.json"),
            configuration: self
                .0
                .directory
                .parent()
                .context("missing config directory")?
                .join("config.json"),
        };
        *self.0.prepared.lock().await = Some(Prepared {
            directory,
            plan,
            lease,
        });
        self.change(Phase::Ready, None);
        Ok(())
    }

    async fn install(&self, version: &str) -> Result<()> {
        let mut prepared = self.0.prepared.lock().await;
        let staged = prepared
            .as_ref()
            .context("download and verify this release before installing")?;
        ensure!(
            staged.plan.manifest.version == version,
            "prepared release does not match the install request"
        );
        let path = self.0.directory.join("plan.json");
        install::durable_json(&path, &staged.plan)?;
        self.0
            .transaction
            .send_replace(Some(staged.plan.transaction.clone()));
        let helper = self.0.helper;
        let helper_path = path.clone();
        tokio::task::spawn_blocking(move || helper(&helper_path)).await??;
        let staged = prepared.take().expect("prepared release is present");
        let plan = staged.plan;
        let _retained = staged.directory.keep();
        drop(staged.lease);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if install::marker_matches(&plan, "helper-ready.json")? {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("update helper did not acknowledge startup; Splice remains running")??;
        install::durable_json(
            &plan.receipt,
            &Receipt {
                transaction: plan.transaction.clone(),
                version: plan.manifest.version.clone(),
                installed: false,
                error: None,
            },
        )?;
        install::write_marker(&plan, "commit.json")?;
        self.0.restart.send_replace(true);
        Ok(())
    }
}
