//! Headless Splice daemon: engine without UI. For servers/debugging; splice-app is the
//! normal way to run Splice.

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,splice_core=debug".into()),
        )
        .init();

    let data_dir = splice_core::config::config_dir().context("resolving config dir")?;
    let cfg = splice_core::config::load(&data_dir);
    let platform = splice_platform::create(splice_platform::PlatformOpts {
        data_dir: data_dir.clone(),
        panic_chord: cfg.panic_chord.clone(),
    })
    .await
    .context("initializing platform backend")?;
    let ts = splice_tailscale::Client::discover()
        .await
        .context("connecting to tailscaled (is Tailscale running?)")?;

    let handle = splice_core::Engine::spawn(platform, ts, data_dir).await?;
    tracing::info!("splice-daemon running");

    // Log state transitions until interrupted.
    let mut state = handle.state();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                handle.send(splice_core::Command::Panic);
                break;
            }
            changed = state.changed() => {
                if changed.is_err() { break; }
                let s = state.borrow().clone();
                tracing::debug!(?s.focus, source = ?s.source, machines = s.machines.len(), "state");
            }
        }
    }
    Ok(())
}
