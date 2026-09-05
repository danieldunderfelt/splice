use super::*;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Fixture {
    host: Host,
    directory: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(tampered: bool, wrong_build: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("splice");
    std::fs::write(&target, "old executable").unwrap();
    let mut build = BuildInfo::current();
    build.version = if wrong_build { "8.0.0" } else { "1.2.0" }.into();
    build.commit = "b".repeat(40);
    build.dirty = false;
    build.protocol = 4;
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' '{}'\n",
        serde_json::to_string(&build).unwrap()
    );
    let mut archive = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut archive, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "splice", script.as_bytes())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    let target_name = BuildInfo::current().target;
    let name = format!("splice-{target_name}.tar.gz");
    let manifest = manifest::Manifest {
        schema: 1,
        version: "1.2.0".into(),
        commit: "b".repeat(40),
        protocol: 4,
        assets: BTreeMap::from([(
            target_name,
            manifest::Asset {
                name: name.clone(),
                size: archive.len() as u64,
                sha256: hex::encode(Sha256::digest(&archive)),
            },
        )]),
    };
    if tampered {
        archive[0] ^= 1;
    }
    let key = SigningKey::from_bytes(&[12; 32]);
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let signature = key.sign(&bytes).to_bytes().to_vec();
    let routes = BTreeMap::from([
        (
            "/latest".to_string(),
            br#"{"tag_name":"v1.2.0","draft":false,"prerelease":false}"#.to_vec(),
        ),
        ("/releases/v1.2.0/splice-update.json".into(), bytes),
        ("/releases/v1.2.0/splice-update.sig".into(), signature),
        (format!("/releases/v1.2.0/{name}"), archive),
    ]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let n = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            let path = request.split_whitespace().nth(1).unwrap();
            let body = routes.get(path).unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        }
    });
    let source = Source {
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        latest: format!("http://{address}/latest"),
        releases: format!("http://{address}/releases"),
        key: key.verifying_key().to_bytes(),
    };
    let mut host = Host::configured(
        directory.path(),
        Ok(install::Installation {
            target,
            bundle: false,
        }),
        source,
    )
    .unwrap();
    Arc::get_mut(&mut host.0).unwrap().helper = |path| {
        std::fs::write(path.with_file_name("helper-started"), "started")?;
        let plan: install::Plan = serde_json::from_slice(&std::fs::read(path)?)?;
        install::write_marker(&plan, "helper-ready.json")?;
        Ok(())
    };
    Fixture {
        host,
        directory,
        server,
    }
}

async fn wait(host: &Host, phase: Phase) {
    let mut state = host.state();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let current = state.borrow_and_update().clone();
            if current.phase == phase {
                return;
            }
            assert_ne!(current.phase, Phase::Failed, "{:?}", current.message);
            state.changed().await.unwrap();
        }
    })
    .await
    .expect("updater state did not advance");
}

#[tokio::test]
async fn signed_download_is_preflighted_before_explicit_restart() {
    let fixture = fixture(false, false).await;
    let host = &fixture.host;
    host.request(control::Action::Check).unwrap();
    assert!(host.request(control::Action::Check).is_err());
    wait(host, Phase::Available).await;
    host.request(control::Action::Prepare {
        version: "1.2.0".into(),
    })
    .unwrap();
    wait(host, Phase::Ready).await;
    assert_eq!(
        std::fs::read_to_string(fixture.directory.path().join("splice")).unwrap(),
        "old executable"
    );
    assert!(!*host.restart().borrow());
    let state = host.state().borrow().clone();
    assert_eq!(state.downloaded, state.total);
    host.request(control::Action::Install {
        version: "1.2.0".into(),
    })
    .unwrap();
    let mut restart = host.restart();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !*restart.borrow_and_update() {
            restart.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(fixture
        .directory
        .path()
        .join("updates/helper-started")
        .exists());
    assert_eq!(
        std::fs::read_to_string(fixture.directory.path().join("splice")).unwrap(),
        "old executable"
    );
}

#[tokio::test]
async fn invalid_archive_or_executable_never_becomes_installable() {
    for (tampered, wrong_build) in [(true, false), (false, true)] {
        let fixture = fixture(tampered, wrong_build).await;
        fixture
            .host
            .request(control::Action::Prepare {
                version: "1.2.0".into(),
            })
            .unwrap();
        wait(&fixture.host, Phase::Failed).await;
        assert!(fixture.host.0.prepared.lock().await.is_none());
        assert!(!*fixture.host.restart().borrow());
        assert_eq!(
            std::fs::read_to_string(fixture.directory.path().join("splice")).unwrap(),
            "old executable"
        );
    }
}

#[tokio::test]
async fn downgrade_and_install_without_preparation_are_explicit_failures() {
    let fixture = fixture(false, false).await;
    fixture
        .host
        .request(control::Action::Prepare {
            version: "0.9.0".into(),
        })
        .unwrap();
    wait(&fixture.host, Phase::Failed).await;
    assert!(fixture
        .host
        .state()
        .borrow()
        .message
        .as_ref()
        .unwrap()
        .contains("downgrades"));
    fixture
        .host
        .request(control::Action::Install {
            version: "1.2.0".into(),
        })
        .unwrap();
    wait(&fixture.host, Phase::Failed).await;
    assert!(!*fixture.host.restart().borrow());
    assert!(!fixture
        .directory
        .path()
        .join("updates/helper-started")
        .exists());
}

#[tokio::test]
async fn interrupted_update_is_reported_and_its_abandoned_files_are_removed() {
    let fixture = fixture(false, false).await;
    let staged_dir = tempfile::Builder::new()
        .prefix(".splice-update-")
        .tempdir_in(fixture.directory.path())
        .unwrap()
        .keep();
    let staged = staged_dir.join("splice");
    std::fs::write(&staged, "new executable").unwrap();
    let plan = install::Plan {
        transaction: "interrupted".into(),
        installation: fixture.host.0.installation.clone().unwrap(),
        staged,
        manifest: manifest::Manifest {
            schema: 1,
            version: "9.0.0".into(),
            commit: "b".repeat(40),
            protocol: 9,
            assets: Default::default(),
        },
        parent_pid: std::process::id(),
        receipt: fixture.directory.path().join("updates/result.json"),
        configuration: fixture.directory.path().join("config.json"),
    };
    install::durable_json(&fixture.directory.path().join("updates/plan.json"), &plan).unwrap();
    fixture.host.confirm_running().unwrap();
    assert_eq!(fixture.host.state().borrow().phase, Phase::Failed);
    assert!(fixture
        .host
        .state()
        .borrow()
        .message
        .as_ref()
        .unwrap()
        .contains("interrupted"));
    assert!(!staged_dir.exists());
    assert!(!fixture.directory.path().join("updates/plan.json").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.directory.path().join("splice")).unwrap(),
        "old executable"
    );
}

#[tokio::test]
async fn startup_cannot_remove_staging_owned_by_another_live_update() {
    let fixture = fixture(false, false).await;
    let _lease = fixture.host.try_lease().unwrap().unwrap();
    let staged = tempfile::Builder::new()
        .prefix(".splice-update-")
        .tempdir_in(fixture.directory.path())
        .unwrap();
    fixture.host.confirm_running().unwrap();
    assert!(staged.path().exists());
    assert!(fixture.host.try_lease().unwrap().is_none());
}

#[tokio::test]
async fn a_prepared_download_can_be_checked_and_prepared_again() {
    let fixture = fixture(false, false).await;
    fixture.host.prepare("1.2.0").await.unwrap();
    let first = fixture
        .host
        .0
        .prepared
        .lock()
        .await
        .as_ref()
        .unwrap()
        .directory
        .path()
        .to_path_buf();
    fixture.host.check().await.unwrap();
    assert!(!first.exists());
    fixture.host.prepare("1.2.0").await.unwrap();
    assert_eq!(fixture.host.state().borrow().phase, Phase::Ready);
}

#[tokio::test]
async fn stale_failure_for_the_same_version_cannot_cancel_a_new_attempt() {
    let fixture = fixture(false, false).await;
    fixture.host.prepare("1.2.0").await.unwrap();
    fixture.host.change(Phase::Restarting, None);
    let path = fixture.directory.path().join("updates/result.json");
    install::durable_json(
        &path,
        &Receipt {
            transaction: "previous-attempt".into(),
            version: "1.2.0".into(),
            installed: false,
            error: Some("old failure".into()),
        },
    )
    .unwrap();
    fixture.host.refresh_result();
    assert_eq!(fixture.host.state().borrow().phase, Phase::Restarting);
    fixture.host.install("1.2.0").await.unwrap();
    fixture.host.refresh_result();
    assert!(*fixture.host.restart().borrow());
    assert!(fixture.host.request(control::Action::Check).is_err());
    let receipt: Receipt = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_ne!(receipt.transaction, "previous-attempt");
    assert!(receipt.error.is_none());
}

#[tokio::test]
async fn helper_launch_failure_leaves_the_running_installation_and_allows_retry() {
    let mut fixture = fixture(false, false).await;
    let helper = fixture.host.0.helper;
    Arc::get_mut(&mut fixture.host.0).unwrap().helper = |_| anyhow::bail!("helper launch denied");
    fixture.host.prepare("1.2.0").await.unwrap();
    assert!(fixture.host.install("1.2.0").await.is_err());
    assert!(!*fixture.host.restart().borrow());
    assert_eq!(
        std::fs::read_to_string(fixture.directory.path().join("splice")).unwrap(),
        "old executable"
    );
    Arc::get_mut(&mut fixture.host.0).unwrap().helper = helper;
    fixture.host.install("1.2.0").await.unwrap();
    assert!(*fixture.host.restart().borrow());
}

#[tokio::test]
async fn unacknowledged_helper_never_requests_app_shutdown() {
    let mut fixture = fixture(false, false).await;
    Arc::get_mut(&mut fixture.host.0).unwrap().helper = |_| Ok(());
    fixture.host.prepare("1.2.0").await.unwrap();
    let error = fixture.host.install("1.2.0").await.unwrap_err();
    assert!(error.to_string().contains("acknowledge"));
    assert!(!*fixture.host.restart().borrow());
    assert_eq!(
        std::fs::read_to_string(fixture.directory.path().join("splice")).unwrap(),
        "old executable"
    );
    let plan: install::Plan = serde_json::from_slice(
        &std::fs::read(fixture.directory.path().join("updates/plan.json")).unwrap(),
    )
    .unwrap();
    assert!(!install::marker_matches(&plan, "commit.json").unwrap());
}

#[tokio::test]
async fn restarted_process_observes_the_helpers_final_receipt() {
    for succeeded in [true, false] {
        let fixture = fixture(false, false).await;
        fixture.host.prepare("1.2.0").await.unwrap();
        let mut prepared = fixture.host.0.prepared.lock().await.take().unwrap();
        let build = BuildInfo::current();
        prepared.plan.manifest.version = build.version.clone();
        prepared.plan.manifest.commit = build.commit;
        let path = fixture.directory.path().join("updates/plan.json");
        install::durable_json(&path, &prepared.plan).unwrap();
        fixture.host.confirm_running().unwrap();
        assert_eq!(fixture.host.state().borrow().phase, Phase::Restarting);
        assert!(!*fixture.host.restart().borrow());
        assert!(fixture.host.request(control::Action::Check).is_err());
        install::durable_json(
            &prepared.plan.receipt,
            &Receipt {
                transaction: prepared.plan.transaction.clone(),
                version: build.version.clone(),
                installed: succeeded,
                error: (!succeeded).then(|| "trial failed; previous installation restored".into()),
            },
        )
        .unwrap();
        fixture.host.refresh_result();
        let state = fixture.host.state().borrow().clone();
        assert_eq!(
            state.phase,
            if succeeded {
                Phase::Idle
            } else {
                Phase::Failed
            }
        );
        assert!(state.message.unwrap().contains(if succeeded {
            "Installed Splice"
        } else {
            "previous installation restored"
        }));
        assert!(!*fixture.host.restart().borrow());
    }
}

#[tokio::test]
async fn damaged_update_receipt_is_visible_without_preventing_app_startup() {
    let fixture = fixture(false, false).await;
    std::fs::write(
        fixture.directory.path().join("updates/result.json"),
        "broken json",
    )
    .unwrap();
    let source = Source {
        client: reqwest::Client::new(),
        latest: String::new(),
        releases: String::new(),
        key: [12; 32],
    };
    let host = Host::configured(
        fixture.directory.path(),
        Ok(fixture.host.0.installation.clone().unwrap()),
        source,
    )
    .unwrap();
    assert_eq!(host.state().borrow().phase, Phase::Failed);
    assert!(host
        .state()
        .borrow()
        .message
        .as_ref()
        .unwrap()
        .contains("invalid persisted update result"));
}
