//! Splice app: engine + tray + egui arrangement window.
//!
//! Spec: docs/DESIGN.md "UI (crates/splice-app)".
//!   app.rs     — eframe App: arrangement canvas (draggable machine cards, snapping,
//!                green/red edges), side panel, header. Pure function of UiState + Commands.
//!   theme.rs   — custom egui Style (NOT default-looking): accent #5B8DEF, rounded cards,
//!                dark/light follow system.
//!   tray.rs    — cfg-gated tray (ksni / tray-icon), menu wired to Commands.
//!   runtime.rs — UI-side controller; macOS bootstraps the engine in-process.
//!
//! Linux process model (Wayland cannot hide a window, so closing one must not stop the
//! engine): `splice service` owns engine + tray + IPC socket (service.rs, ipc.rs);
//! `splice window` is a closeable client (remote.rs); `splice` opens a window, starting
//! the service if needed; `splice quit` stops everything.

mod app;
#[cfg(target_os = "macos")]
mod edge_indicator;
#[cfg(target_os = "linux")]
mod autostart;
mod drag;
mod diagnostics;
mod updates;
mod input;
#[cfg(target_os = "linux")]
mod ipc;
#[cfg(target_os = "linux")]
mod remote;
mod runtime;
#[cfg(target_os = "linux")]
mod service;
mod theme;
mod tray;

/// Finder/launchd/systemd-started processes have no stderr, so the log goes to
/// `log_name` in the config directory unless a terminal is attached.
fn init_tracing(log_name: &str) {
    use std::io::IsTerminal;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,splice_core=debug".into());
    if std::io::stderr().is_terminal() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return;
    }
    let file = splice_core::config::config_dir()
        .ok()
        .and_then(|dir| std::fs::OpenOptions::new().create(true).append(true).open(dir.join(log_name)).ok());
    match file {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

#[cfg(target_os = "linux")]
fn dispatch(preview: bool) {
    use std::process::exit;

    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("window") => {}
        Some("service") => {
            init_tracing("splice.log");
            if let Err(err) = service::run() {
                tracing::error!("splice service failed: {err:#}");
                exit(1);
            }
            exit(0);
        }
        Some("quit") => match ipc::connect() {
            Ok(mut stream) => {
                if let Err(err) = ipc::write_message(&mut stream, &ipc::ClientMessage::Quit) {
                    eprintln!("cannot stop Splice: {err}");
                    exit(1);
                }
                exit(0);
            }
            Err(_) => {
                println!("Splice is not running.");
                exit(0);
            }
        },
        None if preview => {}
        None => {
            let opened = ipc::ensure_service()
                .and_then(|mut stream| ipc::write_message(&mut stream, &ipc::ClientMessage::Open));
            if let Err(err) = opened {
                eprintln!("cannot start Splice: {err}");
                exit(1);
            }
            exit(0);
        }
        Some(other) => {
            eprintln!("unknown command {other:?}\nusage: splice [window|service|quit]");
            exit(2);
        }
    }
}

fn main() -> eframe::Result<()> {
    match std::env::args().nth(1).as_deref() {
        #[cfg(target_os = "macos")]
        Some("--raw-probe") => {
            let result = std::env::args().nth(2)
                .ok_or_else(|| anyhow::anyhow!("usage: splice --raw-probe SECONDS"))
                .and_then(|s| Ok(s.parse::<u64>()?))
                .and_then(|seconds| splice_platform::macos::probe::inspect(std::time::Duration::from_secs(seconds)));
            match result {
                Ok(report) => println!("{}", serde_json::to_string_pretty(&report).expect("HID probe serializes")),
                Err(error) => { eprintln!("{error:#}"); std::process::exit(1); }
            }
            return Ok(());
        }
        Some("--version-json") => { println!("{}", serde_json::to_string(&splice_proto::BuildInfo::current()).expect("build information serializes")); return Ok(()); }
        Some("--version") => { let b = splice_proto::BuildInfo::current(); println!("Splice {} · {} · protocol {}", b.version, b.commit, b.protocol); return Ok(()); }
        Some("--apply-update") => {
            let result = std::env::args_os().nth(2).ok_or_else(|| anyhow::anyhow!("missing update plan path")).and_then(|p| splice_update::install::apply(std::path::Path::new(&p)));
            if let Err(error) = result { eprintln!("{error:#}"); std::process::exit(1); }
            return Ok(());
        }
        _ => {}
    }
    let preview = std::env::var("SPLICE_UI_PREVIEW").ok().as_deref() == Some("1");
    #[cfg(target_os = "linux")]
    dispatch(preview);
    init_tracing(if cfg!(target_os = "linux") { "splice-window.log" } else { "splice.log" });

    let exit_after = if preview {
        std::env::var("SPLICE_UI_EXIT_AFTER")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|secs| *secs > 0.0)
    } else {
        None
    };

    // Some GPU/VM combinations render blank under wgpu; glow is the escape hatch.
    let renderer = match std::env::var("SPLICE_RENDERER").ok().as_deref() {
        Some("glow") => eframe::Renderer::Glow,
        _ => eframe::Renderer::Wgpu,
    };

    let options = eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_title("Splice")
            .with_app_id("io.github.danieldunderfelt.Splice")
            .with_inner_size([1060.0, 680.0])
            .with_min_inner_size([780.0, 480.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Splice",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            #[cfg(target_os = "macos")]
            tray::set_activation_policy_accessory();
            let ctrl = runtime::start(preview, cc.egui_ctx.clone());
            let (actions_tx, actions_rx) = std::sync::mpsc::channel();
            let tray = tray::Tray::new(&ctrl, actions_tx);
            Ok(Box::new(app::SpliceApp::new(
                ctrl,
                tray,
                actions_rx,
                exit_after,
            )))
        }),
    )
}
