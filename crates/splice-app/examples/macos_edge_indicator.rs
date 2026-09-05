#[cfg(target_os = "macos")]
#[path = "../src/edge_indicator.rs"]
mod edge_indicator;

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::{NSDate, NSRunLoop};
    use splice_core::{
        ui_state::{UiConnection, UiCrossing, UiMachine},
        UiState,
    };
    use splice_platform::EdgeSide;
    use splice_proto::{MachineId, Os, Vec2, Vec2I};

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let displays = splice_platform::macos::displays::snapshot();
    let d = displays
        .iter()
        .find(|d| d.x == 0 && d.y == 0)
        .expect("main display")
        .clone();
    let local = MachineId("mac".into());
    let target = MachineId("linux".into());
    let mut state = UiState::initial(local.clone());
    for (id, hostname, os) in [
        (local.clone(), "Mac", Os::Macos),
        (target.clone(), "Linux test destination", Os::Linux),
    ] {
        state.machines.push(UiMachine {
            id,
            hostname: hostname.into(),
            os,
            displays: displays.clone(),
            offset: Vec2I { x: 0, y: 0 },
            enabled: true,
            connection: UiConnection::SelfMachine,
            is_source: os == Os::Macos,
        });
    }
    let indicator = edge_indicator::EdgeIndicator::new();
    for (side, position) in [
        (
            EdgeSide::Right,
            Vec2 {
                x: f64::from(d.w),
                y: f64::from(d.h) / 2.0,
            },
        ),
        (
            EdgeSide::Bottom,
            Vec2 {
                x: f64::from(d.w) / 2.0,
                y: f64::from(d.h),
            },
        ),
        (
            EdgeSide::Left,
            Vec2 {
                x: 0.0,
                y: f64::from(d.h) / 2.0,
            },
        ),
        (
            EdgeSide::Top,
            Vec2 {
                x: f64::from(d.w) / 2.0,
                y: 0.0,
            },
        ),
    ] {
        for step in 0..=20 {
            state.crossing_progress = Some(UiCrossing {
                from: local.clone(),
                to: target.clone(),
                progress: step as f32 / 20.0,
                side,
                position,
            });
            indicator.sync(&state);
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.1));
            assert!(app.keyWindow().is_none());
            let windows = app.windows();
            assert!(windows
                .iter()
                .any(|w| w.isVisible() && w.ignoresMouseEvents()));
        }
        state.crossing_progress = None;
        indicator.sync(&state);
        assert!(app.windows().iter().all(|w| !w.isVisible()));
        println!("{side:?}: shown, updated, and hidden without taking focus");
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos_edge_indicator requires macOS");
    std::process::exit(1);
}
