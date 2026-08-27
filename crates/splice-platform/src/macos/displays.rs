//! Display enumeration in CG global coordinates (points, y-down, origin at the top-left of
//! the main display) plus the reconfiguration callback that republishes them.

use super::MacShared;
use crate::PlatformEvent;
use core_graphics::display::{
    CGDisplay, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDirectDisplayID,
};
use splice_proto::DisplayRect;
use std::ffi::c_void;
use std::sync::Arc;

/// Current active displays, in CG global points.
pub fn snapshot() -> Vec<DisplayRect> {
    let ids = CGDisplay::active_displays().unwrap_or_default();
    ids.into_iter().map(rect_for).collect()
}

fn rect_for(id: CGDirectDisplayID) -> DisplayRect {
    let display = CGDisplay::new(id);
    let bounds = display.bounds();
    // Bounds are points; the mode's pixel width is the backing store. CGDisplayPixelsWide
    // already reports points on Retina displays, so it can't be used for this.
    let scale = display
        .display_mode()
        .filter(|m| m.width() > 0)
        .map(|m| m.pixel_width() as f64 / m.width() as f64)
        .unwrap_or(1.0);
    DisplayRect {
        id: id.to_string(),
        x: bounds.origin.x as i32,
        y: bounds.origin.y as i32,
        w: bounds.size.width as u32,
        h: bounds.size.height as u32,
        scale,
    }
}

/// The corner points of every display rect. Edge hits within the dead-zone radius of one of
/// these are ignored so Splice doesn't fight macOS hot corners.
pub fn corners(displays: &[DisplayRect]) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(displays.len() * 4);
    for d in displays {
        let (x0, y0) = (d.x as f64, d.y as f64);
        let (x1, y1) = (x0 + d.w as f64, y0 + d.h as f64);
        out.extend_from_slice(&[(x0, y0), (x1, y0), (x0, y1), (x1, y1)]);
    }
    out
}

/// Registers the reconfiguration callback. The `Arc` is leaked deliberately: CG has no
/// unregister-on-drop story and the platform lives for the whole process.
pub fn register(shared: Arc<MacShared>) {
    let raw = Arc::into_raw(shared) as *const c_void;
    unsafe { CGDisplayRegisterReconfigurationCallback(on_reconfigure, raw) };
}

unsafe extern "C" fn on_reconfigure(_display: CGDirectDisplayID, flags: u32, user_info: *const c_void) {
    // The callback fires twice per change; the "before" pass still reports the old geometry.
    if CGDisplayChangeSummaryFlags::from_bits_retain(flags)
        .contains(CGDisplayChangeSummaryFlags::kCGDisplayBeginConfigurationFlag)
    {
        return;
    }
    let shared = &*(user_info as *const MacShared);
    let displays = snapshot();
    *shared.displays.write() = displays.clone();
    shared.emit(PlatformEvent::DisplaysChanged { displays });
}
