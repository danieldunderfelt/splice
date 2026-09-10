fn identity() -> bool {
    let build = splice_proto::BuildInfo::current();
    match std::env::args().nth(1).as_deref() {
        Some("--version-json") => {
            println!(
                "{}",
                serde_json::to_string(&build).expect("build information serializes")
            );
            true
        }
        Some("--version") => {
            println!(
                "Splice Files {} · {} · protocol {}",
                build.version, build.commit, build.protocol
            );
            true
        }
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::Arc;

    use anyhow::{anyhow, Result};
    use splice_files::helper::{self, PilotLog, Shelf, ShelfHooks, ViewOutcome};
    use splice_files::ipc::{OfferDesc, OfferStateDesc, PeerDesc, ReceiptDesc, ReceiptStateDesc};
    use splice_files::local::{LocalDirSource, SourceEvent};
    use splice_files::manifest::{self, SourceTree};
    use splice_files::mount::{Mount, MountConfig};
    use splice_platform::files::{EntryKind, FileContentSource, FileOfferId};

    if identity() {
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    return match &args[1..] {
        [] => helper::run(),
        [flag, dir] if flag == "--pilot" => pilot(PathBuf::from(dir)),
        _ => {
            eprintln!("usage: splice-files [--version|--version-json|--pilot <dir>]");
            std::process::exit(2);
        }
    };

    fn pilot(dir: PathBuf) -> Result<()> {
        gtk4::init().map_err(|e| anyhow!("gtk init: {e}"))?;
        std::fs::create_dir_all(&dir)?;
        let log = PilotLog::new(&dir.join("pilot.jsonl"))?;
        let source_dir = dir.join("source");
        ensure_pilot_source(&source_dir)?;
        let offer_id = FileOfferId::new();
        let tree = Rc::new(manifest::from_paths(
            offer_id,
            "pilot",
            std::slice::from_ref(&source_dir),
        )?);
        let log_source = Arc::clone(&log);
        let source = Arc::new(LocalDirSource::new(
            dir.join("cache"),
            Box::new(move |event| {
                let (name, fields) = match &event {
                    SourceEvent::Commit { view } => {
                        ("commit", serde_json::json!({ "view": view.to_string() }))
                    }
                    SourceEvent::Cancel { view, committed } => (
                        "cancel",
                        serde_json::json!({ "view": view.to_string(), "committed": committed }),
                    ),
                    SourceEvent::Materialized {
                        view,
                        entry,
                        bytes,
                        sha256,
                    } => (
                        "materialized",
                        serde_json::json!({
                            "view": view.to_string(),
                            "entry": entry.to_string(),
                            "bytes": bytes,
                            "sha256": sha256,
                        }),
                    ),
                };
                log_source.log(name, fields);
            }),
        )?);
        let mount = Rc::new(Mount::spawn(MountConfig {
            mountpoint: dir.join("mnt"),
            content: Arc::clone(&source) as Arc<dyn FileContentSource>,
            workers: 4,
            queue: 64,
        })?);
        log.log(
            "mount",
            serde_json::json!({ "mountpoint": mount.mountpoint().display().to_string() }),
        );

        let names: Vec<String> = tree.manifest.roots().map(|r| r.name.clone()).collect();
        let total_size: u64 = tree
            .manifest
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .map(|e| e.size)
            .sum();
        let offer = OfferDesc {
            offer: offer_id,
            origin: "pilot".into(),
            names,
            count: tree.manifest.entries.len() as u32,
            total_size: Some(total_size),
            state: OfferStateDesc::Available,
            expires_at: None,
        };

        let arm_view = {
            let mount = Rc::clone(&mount);
            let source = Arc::clone(&source);
            let tree = Rc::clone(&tree);
            Rc::new(move || {
                let view = mount.create_view(tree.manifest.clone());
                source.register_view(view, &tree);
                ViewOutcome {
                    view,
                    uris: mount.view_uris(view),
                    portal_key: None,
                }
            })
        };
        let armed: Rc<RefCell<Option<ViewOutcome>>> = Rc::new(RefCell::new(Some((*arm_view)())));
        let published = Rc::new(Cell::new(false));
        let status_slot: Rc<RefCell<Option<Rc<Shelf>>>> = Rc::new(RefCell::new(None));
        let receipts_slot: Rc<RefCell<Vec<ReceiptDesc>>> = Rc::new(RefCell::new(Vec::new()));

        let hooks = ShelfHooks {
            take_view: {
                let armed = Rc::clone(&armed);
                let log = Arc::clone(&log);
                Rc::new(move |_| {
                    let outcome = armed.borrow_mut().take();
                    if let Some(outcome) = &outcome {
                        log.log(
                            "take_view",
                            serde_json::json!({ "view": outcome.view.to_string() }),
                        );
                    }
                    outcome
                })
            },
            arm_view: {
                let armed = Rc::clone(&armed);
                let log = Arc::clone(&log);
                let arm_view = Rc::clone(&arm_view);
                Rc::new(move |_| {
                    let outcome = (*arm_view)();
                    log.log(
                        "arm_view",
                        serde_json::json!({ "view": outcome.view.to_string() }),
                    );
                    *armed.borrow_mut() = Some(outcome);
                })
            },
            take_receipt_view: {
                let armed = Rc::clone(&armed);
                let log = Arc::clone(&log);
                Rc::new(move |_| {
                    let outcome = armed.borrow_mut().take();
                    if let Some(outcome) = &outcome {
                        log.log(
                            "take_receipt_view",
                            serde_json::json!({ "view": outcome.view.to_string() }),
                        );
                    }
                    outcome
                })
            },
            arm_receipt_view: {
                let armed = Rc::clone(&armed);
                let log = Arc::clone(&log);
                let arm_view = Rc::clone(&arm_view);
                Rc::new(move |_| {
                    let outcome = (*arm_view)();
                    log.log(
                        "arm_receipt_view",
                        serde_json::json!({ "view": outcome.view.to_string() }),
                    );
                    *armed.borrow_mut() = Some(outcome);
                })
            },
            drag_started: {
                let log = Arc::clone(&log);
                Rc::new(move |view| {
                    log.log(
                        "drag_started",
                        serde_json::json!({ "view": view.to_string() }),
                    );
                })
            },
            drop_performed: {
                let mount = Rc::clone(&mount);
                let log = Arc::clone(&log);
                Rc::new(move |view| {
                    let recorded = mount.drop_performed(view);
                    log.log(
                        "drop_performed",
                        serde_json::json!({ "view": view.to_string(), "recorded": recorded }),
                    );
                })
            },
            drag_cancelled: {
                let mount = Rc::clone(&mount);
                let source = Arc::clone(&source);
                let log = Arc::clone(&log);
                Rc::new(move |view| {
                    let stats = mount.stats(view);
                    let outcome = mount.cancel_view(view);
                    source.drop_view(view);
                    log.log(
                        "drag_cancelled",
                        serde_json::json!({
                            "view": view.to_string(),
                            "outcome": format!("{outcome:?}"),
                            "stats": stats_json(stats),
                        }),
                    );
                })
            },
            drag_finished: {
                let mount = Rc::clone(&mount);
                let log = Arc::clone(&log);
                Rc::new(move |view| {
                    log.log(
                        "drag_finished",
                        serde_json::json!({
                            "view": view.to_string(),
                            "stats": stats_json(mount.stats(view)),
                        }),
                    );
                })
            },
            source_drop: {
                let log = Arc::clone(&log);
                Rc::new(move |recipient, paths, fds| {
                    log.log(
                        "source_drop",
                        serde_json::json!({
                            "recipient": recipient,
                            "paths": paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                            "portal_fds": fds.len(),
                        }),
                    );
                    Ok(())
                })
            },
            receive: {
                let mount = Rc::clone(&mount);
                let source = Arc::clone(&source);
                let tree = Rc::clone(&tree);
                let log = Arc::clone(&log);
                let dir = dir.clone();
                let published = Rc::clone(&published);
                let status_slot = Rc::clone(&status_slot);
                let receipts_slot = Rc::clone(&receipts_slot);
                Rc::new(move |offer| {
                    let message = match pilot_materialize(
                        &mount,
                        &source,
                        &tree,
                        &dir.join("inbox"),
                    ) {
                        Ok(uris) => {
                            let result = helper::publish_completed(&uris, None);
                            log.log(
                                "receive",
                                serde_json::json!({
                                    "offer": offer.to_string(),
                                    "uris": uris.iter().map(|u| u.display().to_string()).collect::<Vec<_>>(),
                                    "error": result.as_ref().err().map(|e| e.to_string()),
                                }),
                            );
                            match result {
                                Ok(()) => {
                                    published.set(true);
                                    let names: Vec<String> =
                                        tree.manifest.roots().map(|r| r.name.clone()).collect();
                                    let total_size: u64 = tree
                                        .manifest
                                        .entries
                                        .iter()
                                        .filter(|e| e.kind == EntryKind::File)
                                        .map(|e| e.size)
                                        .sum();
                                    *receipts_slot.borrow_mut() = vec![ReceiptDesc {
                                        receipt: "pilot-receipt".into(),
                                        offer,
                                        origin: "pilot".into(),
                                        count: names.len() as u32,
                                        names,
                                        total_size: Some(total_size),
                                        state: ReceiptStateDesc::Retained,
                                        error: None,
                                        published: true,
                                    }];
                                    if let Some(shelf) = status_slot.borrow().as_ref() {
                                        shelf.set_receipts(receipts_slot.borrow().clone());
                                    }
                                    "Files received — paste in any folder".to_owned()
                                }
                                Err(err) => format!("clipboard publish failed: {err}"),
                            }
                        }
                        Err(err) => {
                            log.log(
                                "receive_failed",
                                serde_json::json!({ "error": err.to_string() }),
                            );
                            format!("receive failed: {err}")
                        }
                    };
                    if let Some(shelf) = status_slot.borrow().as_ref() {
                        shelf.set_status(&message);
                    }
                })
            },
            save_to: {
                let mount = Rc::clone(&mount);
                let source = Arc::clone(&source);
                let tree = Rc::clone(&tree);
                let log = Arc::clone(&log);
                let status_slot = Rc::clone(&status_slot);
                Rc::new(move |offer, dest| {
                    let result = pilot_materialize(&mount, &source, &tree, &dest);
                    log.log(
                        "save_to",
                        serde_json::json!({
                            "offer": offer.to_string(),
                            "dest": dest.display().to_string(),
                            "error": result.as_ref().err().map(|e| e.to_string()),
                        }),
                    );
                    let message = match result {
                        Ok(_) => format!("Saved to {}", dest.display()),
                        Err(err) => format!("save failed: {err}"),
                    };
                    if let Some(shelf) = status_slot.borrow().as_ref() {
                        shelf.set_status(&message);
                    }
                })
            },
            dismiss: Rc::new(move |_| {}),
            cancel_receive: None,
            retry_receive: None,
            republish: {
                let tree = Rc::clone(&tree);
                let log = Arc::clone(&log);
                let dir = dir.clone();
                let status_slot = Rc::clone(&status_slot);
                Rc::new(move |receipt| {
                    let inbox = dir.join("inbox");
                    let uris: Vec<PathBuf> = tree
                        .manifest
                        .roots()
                        .map(|root| inbox.join(&root.name))
                        .collect();
                    let message = if uris.iter().all(|path| path.exists()) {
                        match helper::publish_completed(&uris, None) {
                            Ok(()) => "Files copied to the clipboard again".to_owned(),
                            Err(err) => format!("clipboard publish failed: {err}"),
                        }
                    } else {
                        "The retained files are gone; receive them again".to_owned()
                    };
                    log.log(
                        "republish",
                        serde_json::json!({ "receipt": receipt, "message": message }),
                    );
                    if let Some(shelf) = status_slot.borrow().as_ref() {
                        shelf.set_status(&message);
                    }
                })
            },
            clear_receipt: {
                let log = Arc::clone(&log);
                let dir = dir.clone();
                let status_slot = Rc::clone(&status_slot);
                let receipts_slot = Rc::clone(&receipts_slot);
                Rc::new(move |receipt| {
                    let inbox = dir.join("inbox");
                    let result = std::fs::remove_dir_all(&inbox);
                    log.log(
                        "clear_receipt",
                        serde_json::json!({
                            "receipt": receipt,
                            "error": result.as_ref().err().map(|e| e.to_string()),
                        }),
                    );
                    receipts_slot.borrow_mut().clear();
                    if let Some(shelf) = status_slot.borrow().as_ref() {
                        shelf.set_receipts(Vec::new());
                        let message = match result {
                            Ok(()) => "Cleared the retained files".to_owned(),
                            Err(err) => format!("clear failed: {err}"),
                        };
                        shelf.set_status(&message);
                    }
                })
            },
            reveal: {
                let dir = dir.clone();
                let status_slot = Rc::clone(&status_slot);
                Rc::new(move |_| {
                    if let Some(shelf) = status_slot.borrow().as_ref() {
                        shelf.reveal(std::slice::from_ref(&dir.join("inbox")));
                    }
                })
            },
        };

        let shelf = Shelf::build("Splice Files Pilot", hooks);
        shelf.set_peers(vec![PeerDesc {
            id: "pilot".into(),
            name: "Pilot (local only)".into(),
        }]);
        shelf.set_offers(vec![offer]);
        shelf.set_status(&format!(
            "Pilot mount at {} — drag the tile into another app",
            mount.mountpoint().display()
        ));
        *status_slot.borrow_mut() = Some(Rc::clone(&shelf));
        let ui = helper::HelperUi {
            shelf,
            clipboard_published: published,
        };
        helper::run_window(&ui);
        Ok(())
    }

    fn stats_json(stats: Option<splice_platform::files::ViewStats>) -> serde_json::Value {
        match stats {
            Some(s) => serde_json::json!({
                "bytes_served": s.bytes_served,
                "reads": s.reads,
                "denied_reads": s.denied_reads,
                "commits": s.commits,
            }),
            None => serde_json::Value::Null,
        }
    }

    fn ensure_pilot_source(dir: &Path) -> Result<()> {
        if dir.is_dir() && std::fs::read_dir(dir)?.next().is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(dir.join("nested"))?;
        std::fs::write(
            dir.join("pilot-sample.txt"),
            b"splice-files pilot sample payload\n",
        )?;
        std::fs::write(dir.join("nested/second.bin"), vec![7u8; 4096])?;
        Ok(())
    }

    fn pilot_materialize(
        mount: &Mount,
        source: &Arc<LocalDirSource>,
        tree: &SourceTree,
        dest: &Path,
    ) -> Result<Vec<PathBuf>> {
        let view = mount.create_view(tree.manifest.clone());
        source.register_view(view, tree);
        let mut written: HashMap<splice_platform::files::EntryId, PathBuf> = HashMap::new();
        for entry in &tree.manifest.entries {
            let parent_path = match entry.parent {
                Some(parent) => written
                    .get(&parent)
                    .cloned()
                    .ok_or_else(|| anyhow!("manifest order: parent missing"))?,
                None => dest.to_path_buf(),
            };
            let path = parent_path.join(&entry.name);
            match entry.kind {
                EntryKind::Dir => {
                    std::fs::create_dir_all(&path)?;
                    written.insert(entry.id, path);
                }
                EntryKind::File => {
                    let cached = source.materialize(view, entry.id)?;
                    std::fs::copy(&cached, &path)?;
                    written.insert(entry.id, path);
                }
                EntryKind::Symlink => {
                    let target = entry
                        .link_target
                        .as_ref()
                        .ok_or_else(|| anyhow!("symlink without target"))?;
                    std::os::unix::fs::symlink(target, &path)?;
                    written.insert(entry.id, path);
                }
            }
        }
        let uris = tree
            .manifest
            .roots()
            .map(|root| dest.join(&root.name))
            .collect();
        Ok(uris)
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    if identity() {
        return;
    }
    eprintln!("splice-files is only supported on Linux");
    std::process::exit(1);
}
