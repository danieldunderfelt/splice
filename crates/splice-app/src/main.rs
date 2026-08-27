//! Splice app: engine + tray + egui arrangement window in one process.
//!
//! Spec: docs/DESIGN.md "UI (crates/splice-app)". Implemented by the UI agent:
//!   app.rs    — eframe App: arrangement canvas (draggable machine cards, snapping,
//!               green/red edges), side panel, header. Pure function of UiState + Commands.
//!   theme.rs  — custom egui Style (NOT default-looking): accent #5B8DEF, rounded cards,
//!               dark/light follow system.
//!   tray.rs   — cfg-gated tray (ksni / tray-icon), menu wired to Commands; window
//!               opened on demand; app continues with window closed.
//!   runtime.rs— tokio runtime thread + engine bootstrap; macOS ActivationPolicy::Accessory.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,splice_core=debug".into()),
        )
        .init();
    eprintln!("splice-app: UI not yet implemented (see docs/DESIGN.md)");
    Ok(())
}
