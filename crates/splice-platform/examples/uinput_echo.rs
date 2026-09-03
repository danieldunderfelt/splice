//! Diagnostic: inject a keystroke and a click through the Linux uinput backend and
//! report whether the physical-activity monitor mistook them for real input (a
//! keyboard remapper such as keyd re-emits our keystrokes on its own device).
//!
//!   cargo run -p splice-platform --example uinput_echo
//!
//! Stop the Splice service first; the run creates its own virtual devices.

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use splice_platform::{BackendPrefs, InjectPref, PlatformEvent, PlatformOpts};
    use splice_proto::{InputEvent, PointerButton, Vec2};
    use std::time::Duration;

    tracing_subscriber::fmt()
        .with_env_filter("info,splice_platform=debug")
        .init();
    let data_dir = directories::ProjectDirs::from("", "", "splice")
        .map(|d| d.config_dir().to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    let mut platform = splice_platform::create(PlatformOpts {
        data_dir,
        panic_chord: Vec::new(),
        backends: BackendPrefs { inject: InjectPref::Uinput, ..Default::default() },
    })
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    while platform.events.try_recv().is_ok() {}

    platform.emulate.enter(Vec2 { x: 200.0, y: 200.0 }).await?;
    for _ in 0..3 {
        platform.emulate.inject(InputEvent::Key { code: 42, pressed: true }).await?;
        platform.emulate.inject(InputEvent::Key { code: 42, pressed: false }).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        platform.emulate.inject(InputEvent::Motion { dx: 5.0, dy: 0.0 }).await?;
        platform.emulate.inject(InputEvent::Button { button: PointerButton::Middle, pressed: true }).await?;
        platform.emulate.inject(InputEvent::Button { button: PointerButton::Middle, pressed: false }).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    platform.emulate.leave().await?;

    let mut physical = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, platform.events.recv()).await {
        if matches!(ev, PlatformEvent::PhysicalActivity) {
            physical += 1;
        }
    }
    println!("physical-activity events during injection: {physical}");
    if physical > 0 {
        anyhow::bail!("injected input was counted as physical");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {}
