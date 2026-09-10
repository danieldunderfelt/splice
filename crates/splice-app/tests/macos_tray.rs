#![allow(dead_code)]

#[cfg(target_os = "macos")]
#[path = "../src/runtime.rs"]
mod runtime;
#[cfg(target_os = "macos")]
#[path = "../src/tray.rs"]
mod tray;
#[cfg(target_os = "macos")]
#[path = "../src/file_shelf/mod.rs"]
mod file_shelf;

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use splice_core::ui_state::UiConnection;
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let ctx = egui::Context::default();
    let repaint_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let wake_count = repaint_count.clone();
    ctx.set_request_repaint_callback(move |_| {
        wake_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let ctrl = runtime::start(true, ctx.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let tray = tray::macos::MacTray::new(&ctrl, tx, ctx).unwrap();
    let status = tray._tray.ns_status_item().unwrap();
    let original = status.menu(mtm).unwrap();
    let open = original.itemAtIndex(0).unwrap();
    let mut state = ctrl.state();
    for i in 0..100 {
        for machine in &mut state.machines {
            if machine.id != state.self_id {
                machine.connection = UiConnection::Direct {
                    rtt_ms: f64::from(i),
                };
            }
        }
        tray.sync(&state).unwrap();
        let current = status.menu(mtm).unwrap();
        assert!(
            std::ptr::eq(&*original, &*current),
            "RTT updates replaced the native menu"
        );
        assert!(
            std::ptr::eq(&*open, &*current.itemAtIndex(0).unwrap()),
            "Open Splice lost its native menu item"
        );
    }
    for _ in 0..100 {
        original.performActionForItemAtIndex(0);
        assert!(matches!(rx.try_recv().unwrap(), tray::AppAction::Open));
    }
    assert!(repaint_count.load(std::sync::atomic::Ordering::SeqCst) > 0);
    use objc2_app_kit::{NSMenuDidBeginTrackingNotification, NSMenuDidEndTrackingNotification};
    use objc2_foundation::{NSDate, NSNotificationCenter, NSRunLoop};
    let center = NSNotificationCenter::defaultCenter();
    let count = original.numberOfItems();
    unsafe {
        center.postNotificationName_object(NSMenuDidBeginTrackingNotification, Some(&original));
    }
    state.machines.retain(|machine| machine.id == state.self_id);
    tray.sync(&state).unwrap();
    assert_eq!(
        original.numberOfItems(),
        count,
        "membership changed during native menu tracking"
    );
    unsafe {
        center.postNotificationName_object(NSMenuDidEndTrackingNotification, Some(&original));
    }
    tray.sync(&state).unwrap();
    assert_eq!(
        original.numberOfItems(),
        count,
        "membership changed before native click dispatch completed"
    );
    original.performActionForItemAtIndex(0);
    assert!(matches!(rx.try_recv().unwrap(), tray::AppAction::Open));
    NSRunLoop::mainRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.02));
    tray.sync(&state).unwrap();
    assert_eq!(original.numberOfItems(), 7);
    assert!(std::ptr::eq(&*open, &*original.itemAtIndex(0).unwrap()));
    original.performActionForItemAtIndex(0);
    assert!(matches!(rx.try_recv().unwrap(), tray::AppAction::Open));
    println!(
        "Native menu survives 100 RTT updates, 102 Open actions, and deferred membership changes"
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {}
