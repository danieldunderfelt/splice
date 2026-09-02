//! Splice app: engine + tray + egui arrangement window in one process.
//!
//! Spec: docs/DESIGN.md "UI (crates/splice-app)".
//!   app.rs    — eframe App: arrangement canvas (draggable machine cards, snapping,
//!               green/red edges), side panel, header. Pure function of UiState + Commands.
//!   theme.rs  — custom egui Style (NOT default-looking): accent #5B8DEF, rounded cards,
//!               dark/light follow system.
//!   tray.rs   — cfg-gated tray (ksni / tray-icon), menu wired to Commands; window
//!               opened on demand; app continues with window closed.
//!   runtime.rs— tokio runtime thread + engine bootstrap; macOS ActivationPolicy::Accessory.

mod app;
mod drag;
mod runtime;
mod theme;
mod tray;

#[cfg(target_os = "linux")]
fn restore_dumpability_after_group_activation() {
    // `sg` is setgid, so Linux marks its descendants non-dumpable. Desktop
    // portals inspect /proc/<pid>/root using a ptrace-style access check and
    // reject the session while that flag is clear. Splice now has ordinary
    // user credentials again, so restore the normal exec-time value.
    let result = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
    if result != 0 {
        eprintln!(
            "warning: could not restore process dumpability after input-group activation: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Finder/launchd-started apps have no stderr, so the log goes to `splice.log` in the
/// config directory unless a terminal is attached.
fn init_tracing() {
    use std::io::IsTerminal;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,splice_core=debug".into());
    if std::io::stderr().is_terminal() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return;
    }
    let file = splice_core::config::config_dir()
        .ok()
        .and_then(|dir| std::fs::File::create(dir.join("splice.log")).ok());
    match file {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "linux")]
    restore_dumpability_after_group_activation();

    init_tracing();

    let preview = std::env::var("SPLICE_UI_PREVIEW").ok().as_deref() == Some("1");
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
            .with_app_id("splice")
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
            let (tray, tray_actions) = tray::Tray::new(&ctrl);
            Ok(Box::new(app::SpliceApp::new(
                ctrl,
                tray,
                tray_actions,
                exit_after,
            )))
        }),
    )
}
