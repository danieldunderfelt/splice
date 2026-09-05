use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub target: PathBuf,
    pub bundle: bool,
}

impl Installation {
    pub fn detect(executable: &Path) -> Result<Self> {
        let executable = executable.canonicalize()?;
        #[cfg(target_os = "linux")]
        {
            ensure!(
                std::env::var_os("FLATPAK_ID").is_none(),
                "Flatpak installations update through Flatpak"
            );
            let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
            let expected = home.join(".local/bin/splice");
            ensure!(executable == expected, "This installation is managed outside Splice. Install the release at ~/.local/bin/splice to enable in-app updates");
            ensure!(std::env::var_os("INVOCATION_ID").is_some(), "Start Splice with systemctl --user start app-splice.service to enable in-app updates");
            Ok(Self {
                target: executable,
                bundle: false,
            })
        }
        #[cfg(target_os = "macos")]
        {
            let target = executable
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .context("Splice must run from its app bundle")?;
            ensure!(
                target.extension().is_some_and(|s| s == "app")
                    && executable == target.join("Contents/MacOS/splice"),
                "Splice must run from its signed app bundle"
            );
            bundle_team(target)?;
            Ok(Self {
                target: target.into(),
                bundle: true,
            })
        }
    }

    pub fn executable(&self) -> PathBuf {
        if self.bundle {
            self.target.join("Contents/MacOS/splice")
        } else {
            self.target.clone()
        }
    }

    pub fn check_writable(&self) -> Result<()> {
        let metadata = std::fs::symlink_metadata(&self.target)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "cannot update a symbolic-link installation"
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Splice does not own this installation; use its package manager"
        );
        let parent = self
            .target
            .parent()
            .context("installation has no parent directory")?;
        tempfile::NamedTempFile::new_in(parent)
            .context("installation directory is not writable")?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub transaction: String,
    pub installation: Installation,
    pub staged: PathBuf,
    pub manifest: crate::manifest::Manifest,
    pub parent_pid: u32,
    pub receipt: PathBuf,
    pub configuration: PathBuf,
}

pub fn durable_json(path: &Path, value: &impl Serialize) -> Result<()> {
    durable_write(path, &serde_json::to_vec_pretty(value)?)
}

fn durable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("state file has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn unpack(archive: &[u8], destination: &Path, bundle: bool) -> Result<PathBuf> {
    let root = if bundle { "Splice.app" } else { "splice" };
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut size = 0_u64;
    let mut names = std::collections::HashSet::new();
    let mut count = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        ensure!(
            path.components()
                .all(|p| matches!(p, std::path::Component::Normal(_))),
            "archive contains an unsafe path"
        );
        ensure!(
            path.starts_with(root) && (bundle || path == Path::new(root)),
            "archive contains an unexpected path"
        );
        ensure!(
            names.insert(path.clone()),
            "archive contains duplicate paths"
        );
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "archive contains a link or special file"
        );
        size = size
            .checked_add(entry.size())
            .context("archive size overflow")?;
        count += 1;
        ensure!(
            size <= 512 * 1024 * 1024 && count <= 4096,
            "expanded archive exceeds its limit"
        );
        ensure!(
            entry.unpack_in(destination)?,
            "archive path escaped staging directory"
        );
        let extracted = destination.join(path);
        let executable = extracted
            == destination.join(if bundle {
                "Splice.app/Contents/MacOS/splice"
            } else {
                "splice"
            });
        std::fs::set_permissions(
            &extracted,
            std::fs::Permissions::from_mode(if kind.is_dir() || executable {
                0o755
            } else {
                0o644
            }),
        )?;
        if kind.is_file() {
            std::fs::File::open(&extracted)?.sync_all()?;
        }
    }
    let result = destination.join(root);
    ensure!(result.exists(), "archive is missing Splice");
    Ok(result)
}

pub async fn preflight(
    staged: &Path,
    installation: &Installation,
    manifest: &crate::manifest::Manifest,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    if installation.bundle {
        verify_bundle(staged, &installation.target)?;
    }
    let executable = if installation.bundle {
        staged.join("Contents/MacOS/splice")
    } else {
        staged.into()
    };
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(executable)
            .arg("--version-json")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("updated executable did not respond to preflight")??;
    ensure!(
        output.status.success(),
        "updated executable failed preflight"
    );
    let build: splice_proto::BuildInfo = serde_json::from_slice(&output.stdout)
        .context("updated executable returned invalid build information")?;
    ensure!(
        !build.dirty
            && build.version == manifest.version
            && build.commit == manifest.commit
            && build.protocol == manifest.protocol
            && build.target == splice_proto::BuildInfo::current().target,
        "updated executable does not match its signed manifest"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn bundle_team(path: &Path) -> Result<String> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=4"])
        .arg(path)
        .output()?;
    ensure!(
        output.status.success(),
        "cannot inspect app signing identity"
    );
    let details = String::from_utf8(output.stderr)?;
    let team = details
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier="))
        .context("app is missing its Developer ID team")?;
    ensure!(
        team.len() == 10 && team.bytes().all(|b| b.is_ascii_alphanumeric()),
        "app must have a Developer ID signature before using OTA"
    );
    Ok(team.into())
}

#[cfg(target_os = "macos")]
fn verify_bundle(staged: &Path, installed: &Path) -> Result<()> {
    let team = bundle_team(installed)?;
    ensure!(
        bundle_team(staged)? == team,
        "update was signed by a different Apple developer team"
    );
    let requirement = format!("anchor apple generic and identifier \"dev.splice.app\" and certificate leaf[subject.OU] = \"{team}\"");
    let verified = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "-R", &requirement])
        .arg(staged)
        .output()?;
    ensure!(
        verified.status.success(),
        "updated app signature validation failed"
    );
    let assessed = std::process::Command::new("/usr/sbin/spctl")
        .args(["--assess", "--type", "execute"])
        .arg(staged)
        .output()?;
    ensure!(
        assessed.status.success(),
        "updated app did not pass Gatekeeper assessment"
    );
    Ok(())
}

pub fn launch_helper(plan_path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("systemd-run")
            .args(["--user", "--collect", "--quiet", "--unit"])
            .arg(format!("splice-update-{}", std::process::id()))
            .arg(std::env::current_exe()?)
            .arg("--apply-update")
            .arg(plan_path)
            .status()?;
        ensure!(
            status.success(),
            "could not start the independent update service"
        );
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("--apply-update")
            .arg(plan_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    Ok(())
}

pub fn apply(plan_path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(plan_path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 65536,
        "unsafe update plan file"
    );
    let plan: Plan = serde_json::from_reader(file)?;
    let lock_path = plan.receipt.with_file_name("transaction.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;
    lock.lock()
        .context("cannot acquire the update transaction lock")?;
    write_marker(&plan, "helper-ready.json")?;
    let handoff = (|| -> Result<()> {
        let started = std::time::Instant::now();
        while !marker_matches(&plan, "commit.json")? {
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "Splice did not commit the prepared update"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        wait_parent(plan.parent_pid, Duration::from_secs(60))
    })();
    if let Err(error) = handoff {
        durable_json(
            &plan.receipt,
            &crate::Receipt {
                transaction: plan.transaction.clone(),
                version: plan.manifest.version.clone(),
                installed: false,
                error: Some(format!("{error:#}; installation was not changed")),
            },
        )?;
        cleanup(&plan, plan_path)?;
        return Err(error);
    }
    let mut runtime = ProcessRuntime {
        plan: &plan,
        child: None,
    };
    let outcome = transact(&plan, &mut runtime);
    let can_cleanup = !matches!(outcome, Outcome::RecoveryFailed(_));
    let result = outcome.result();
    let receipt = crate::Receipt {
        transaction: plan.transaction.clone(),
        version: plan.manifest.version.clone(),
        installed: result.is_ok(),
        error: result
            .as_ref()
            .err()
            .map(|error| format!("Update failed: {error:#}")),
    };
    let recorded = durable_json(&plan.receipt, &receipt);
    if can_cleanup && recorded.is_ok() {
        cleanup(&plan, plan_path)?;
    }
    result.and(recorded)
}

pub(crate) fn write_marker(plan: &Plan, name: &str) -> Result<()> {
    durable_json(
        &plan
            .staged
            .parent()
            .context("missing staging directory")?
            .join(name),
        &plan.transaction,
    )
}

pub(crate) fn marker_matches(plan: &Plan, name: &str) -> Result<bool> {
    let path = plan
        .staged
        .parent()
        .context("missing staging directory")?
        .join(name);
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<String>(&bytes)? == plan.transaction),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn wait_parent(pid: u32, timeout: Duration) -> Result<()> {
    ensure!(
        pid > 1 && pid <= i32::MAX as u32,
        "invalid Splice process ID"
    );
    let started = std::time::Instant::now();
    loop {
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if alive != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        ensure!(
            started.elapsed() < timeout,
            "Splice did not stop within the update deadline"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn cleanup(plan: &Plan, plan_path: &Path) -> Result<()> {
    let directory = plan
        .staged
        .parent()
        .context("staging directory is missing")?;
    ensure!(
        directory
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(".splice-update-")),
        "invalid staging directory"
    );
    match std::fs::remove_dir_all(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match std::fs::remove_file(plan_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

trait AppRuntime {
    fn preflight(&mut self) -> Result<()>;
    fn launch(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn ready(&mut self) -> Result<()>;
}

#[derive(Debug)]
enum Outcome {
    Committed,
    Restored(anyhow::Error),
    RecoveryFailed(anyhow::Error),
}

impl Outcome {
    fn result(self) -> Result<()> {
        match self {
            Self::Committed => Ok(()),
            Self::Restored(error) | Self::RecoveryFailed(error) => Err(error),
        }
    }
}

fn transact(plan: &Plan, runtime: &mut impl AppRuntime) -> Outcome {
    let mut replaced = false;
    let mut configuration = None;
    let outcome = (|| -> Result<()> {
        runtime.preflight()?;
        let saved = Configuration::capture(&plan.configuration)?;
        durable_json(
            &plan
                .staged
                .parent()
                .context("missing staging directory")?
                .join("configuration-before.json"),
            &saved,
        )?;
        configuration = Some(saved);
        replace(
            &plan.staged,
            &plan.installation.target,
            plan.installation.bundle,
        )?;
        replaced = true;
        sync_installation(&plan.installation.target)?;
        runtime.launch()?;
        runtime.ready()?;
        Ok(())
    })();
    if let Err(error) = outcome {
        if replaced {
            if let Err(stop) = runtime.stop() {
                return Outcome::RecoveryFailed(anyhow::anyhow!("{error:#}; failed trial could not stop: {stop:#}; previous app remains in staging"));
            }
            if let Err(restore) = restore(plan).and_then(|()| {
                configuration
                    .as_ref()
                    .context("configuration snapshot is missing")?
                    .restore()
            }) {
                return Outcome::RecoveryFailed(anyhow::anyhow!(
                    "{error:#}; could not restore previous app: {restore:#}"
                ));
            }
        }
        return match runtime.launch() {
            Ok(()) => Outcome::Restored(anyhow::anyhow!(
                "{error:#}; the previous installation was restored and restarted"
            )),
            Err(restart) => Outcome::RecoveryFailed(anyhow::anyhow!(
                "{error:#}; could not restart the previous installation: {restart:#}"
            )),
        };
    }
    Outcome::Committed
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}

impl Configuration {
    fn capture(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("saving configuration before update"),
        };
        Ok(Self {
            path: path.into(),
            bytes,
        })
    }

    fn restore(&self) -> Result<()> {
        match &self.bytes {
            Some(bytes) => durable_write(&self.path, bytes),
            None => match std::fs::remove_file(&self.path) {
                Ok(()) => sync_installation(&self.path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    Err(error).context("removing configuration written by the failed update")
                }
            },
        }
    }
}

fn restore(plan: &Plan) -> Result<()> {
    if plan.installation.bundle {
        replace(&plan.staged, &plan.installation.target, true)?;
        sync_installation(&plan.installation.target)
    } else {
        std::fs::rename(
            plan.staged.with_extension("previous"),
            &plan.installation.target,
        )?;
        std::fs::File::open(
            plan.installation
                .target
                .parent()
                .context("missing installation directory")?,
        )?
        .sync_all()?;
        Ok(())
    }
}

struct ProcessRuntime<'a> {
    plan: &'a Plan,
    child: Option<std::process::Child>,
}

impl AppRuntime for ProcessRuntime<'_> {
    fn preflight(&mut self) -> Result<()> {
        self.plan.installation.check_writable()?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(preflight(
                &self.plan.staged,
                &self.plan.installation,
                &self.plan.manifest,
            ))
    }

    fn launch(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let status = std::process::Command::new("systemctl")
                .args(["--user", "start", "app-splice.service"])
                .status()?;
            ensure!(status.success(), "app-splice.service could not start");
        }
        #[cfg(target_os = "macos")]
        {
            self.child =
                Some(std::process::Command::new(self.plan.installation.executable()).spawn()?);
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let status = std::process::Command::new("systemctl")
                .args(["--user", "stop", "app-splice.service"])
                .status()?;
            ensure!(status.success(), "app-splice.service could not stop");
        }
        if let Some(mut child) = self.child.take() {
            if child.try_wait()?.is_none() {
                child.kill()?;
                child.wait()?;
            }
        }
        Ok(())
    }

    fn ready(&mut self) -> Result<()> {
        let started = std::time::Instant::now();
        let path = self.plan.receipt.with_file_name("ready.json");
        loop {
            if let Some(child) = &mut self.child {
                ensure!(
                    child.try_wait()?.is_none(),
                    "updated Splice exited before finishing startup"
                );
            }
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let ready: Ready = serde_json::from_slice(&bytes)?;
                    if ready.transaction == self.plan.transaction
                        && ready.version == self.plan.manifest.version
                        && ready.commit == self.plan.manifest.commit
                    {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            ensure!(
                started.elapsed() < Duration::from_secs(60),
                "updated Splice did not finish startup within 60 seconds"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub transaction: String,
    pub version: String,
    pub commit: String,
}

pub fn replace(staged: &Path, target: &Path, bundle: bool) -> Result<()> {
    if bundle {
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let from = std::ffi::CString::new(staged.as_os_str().as_bytes())?;
            let to = std::ffi::CString::new(target.as_os_str().as_bytes())?;
            let result = unsafe {
                libc::renameatx_np(
                    libc::AT_FDCWD,
                    from.as_ptr(),
                    libc::AT_FDCWD,
                    to.as_ptr(),
                    libc::RENAME_SWAP,
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("atomically exchanging app bundles");
            }
        }
        #[cfg(not(target_os = "macos"))]
        anyhow::bail!("app bundle replacement requires macOS");
    } else {
        ensure!(
            std::fs::symlink_metadata(target)?.is_file(),
            "installed executable is not a regular file"
        );
        let previous = staged.with_extension("previous");
        std::fs::hard_link(target, &previous).context("retaining the previous executable")?;
        std::fs::rename(staged, target).context("atomically replacing the executable")?;
    }
    Ok(())
}

fn sync_installation(target: &Path) -> Result<()> {
    std::fs::File::open(target.parent().context("missing installation parent")?)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRuntime {
        failure: &'static str,
        events: Vec<&'static str>,
        configuration: Option<PathBuf>,
    }

    impl AppRuntime for FakeRuntime {
        fn preflight(&mut self) -> Result<()> {
            self.events.push("preflight");
            ensure!(self.failure != "preflight", "preflight failed");
            Ok(())
        }
        fn launch(&mut self) -> Result<()> {
            self.events.push("launch");
            ensure!(
                self.failure != "launch"
                    || self.events.iter().filter(|e| **e == "launch").count() != 1,
                "trial launch failed"
            );
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            self.events.push("stop");
            ensure!(self.failure != "stop", "stop failed");
            Ok(())
        }
        fn ready(&mut self) -> Result<()> {
            if let Some(path) = &self.configuration {
                std::fs::write(path, "trial configuration")?;
            }
            self.events.push("ready");
            ensure!(self.failure.is_empty(), "trial did not become ready");
            Ok(())
        }
    }

    fn plan(dir: &Path) -> Plan {
        let target = dir.join("splice");
        let staged = dir.join("new");
        std::fs::write(&target, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();
        Plan {
            transaction: "test-transaction".into(),
            installation: Installation {
                target,
                bundle: false,
            },
            staged,
            manifest: crate::manifest::Manifest {
                schema: 1,
                version: "1.2.0".into(),
                commit: "a".repeat(40),
                protocol: 3,
                assets: Default::default(),
            },
            parent_pid: 1,
            receipt: dir.join("result.json"),
            configuration: dir.join("config.json"),
        }
    }

    #[test]
    fn transaction_commits_only_after_the_new_process_confirms_readiness() {
        let directory = tempfile::tempdir().unwrap();
        let plan = plan(directory.path());
        let mut runtime = FakeRuntime {
            failure: "",
            events: vec![],
            configuration: None,
        };
        assert!(matches!(transact(&plan, &mut runtime), Outcome::Committed));
        assert_eq!(runtime.events, ["preflight", "launch", "ready"]);
        assert_eq!(
            std::fs::read_to_string(&plan.installation.target).unwrap(),
            "new"
        );
    }

    #[test]
    fn failed_trial_restores_old_bytes_before_relaunching() {
        for failure in ["preflight", "launch", "ready"] {
            let directory = tempfile::tempdir().unwrap();
            let plan = plan(directory.path());
            let mut runtime = FakeRuntime {
                failure,
                events: vec![],
                configuration: None,
            };
            assert!(
                matches!(transact(&plan, &mut runtime), Outcome::Restored(_)),
                "failure at {failure}"
            );
            assert_eq!(
                std::fs::read_to_string(&plan.installation.target).unwrap(),
                "old"
            );
            assert_eq!(runtime.events.last(), Some(&"launch"));
            if failure != "preflight" {
                assert!(runtime.events.contains(&"stop"));
            }
        }
    }

    #[test]
    fn failed_trial_restores_configuration_including_its_original_absence() {
        for existing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let plan = plan(directory.path());
            if existing {
                std::fs::write(&plan.configuration, "original configuration").unwrap();
            }
            let mut runtime = FakeRuntime {
                failure: "ready",
                events: vec![],
                configuration: Some(plan.configuration.clone()),
            };
            assert!(matches!(
                transact(&plan, &mut runtime),
                Outcome::Restored(_)
            ));
            if existing {
                assert_eq!(
                    std::fs::read_to_string(&plan.configuration).unwrap(),
                    "original configuration"
                );
            } else {
                assert!(!plan.configuration.exists());
            }
        }
    }

    #[test]
    fn stop_failure_retains_recovery_files_and_reports_it() {
        let directory = tempfile::tempdir().unwrap();
        let plan = plan(directory.path());
        let mut runtime = FakeRuntime {
            failure: "stop",
            events: vec![],
            configuration: None,
        };
        assert!(matches!(
            transact(&plan, &mut runtime),
            Outcome::RecoveryFailed(_)
        ));
        assert_eq!(
            std::fs::read_to_string(plan.staged.with_extension("previous")).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(&plan.installation.target).unwrap(),
            "new"
        );
    }

    #[test]
    fn executable_replacement_keeps_previous_bytes_and_never_truncates_open_readers() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("splice");
        let staged = dir.path().join("new");
        std::fs::write(&target, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();
        let mut open = std::fs::File::open(&target).unwrap();
        replace(&staged, &target, false).unwrap();
        let mut previous = String::new();
        open.read_to_string(&mut previous).unwrap();
        assert_eq!(previous, "old");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(staged.with_extension("previous")).unwrap(),
            "old"
        );
    }

    #[test]
    fn missing_staged_file_leaves_installation_intact() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("splice");
        std::fs::write(&target, "old").unwrap();
        assert!(replace(&dir.path().join("missing"), &target, false).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "old");
    }

    #[test]
    fn archive_refuses_links_and_files_outside_the_expected_root() {
        for (name, kind) in [
            ("other", tar::EntryType::Regular),
            ("splice", tar::EntryType::Symlink),
        ] {
            let mut bytes = Vec::new();
            {
                let zip = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::fast());
                let mut tar = tar::Builder::new(zip);
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o755);
                header.set_entry_type(kind);
                header.set_cksum();
                tar.append_data(&mut header, name, &[][..]).unwrap();
                tar.into_inner().unwrap().finish().unwrap();
            }
            assert!(unpack(&bytes, tempfile::tempdir().unwrap().path(), false).is_err());
        }
    }
}
