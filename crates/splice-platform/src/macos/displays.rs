//! Display enumeration in CG global coordinates (points, y-down, origin at the top-left of
//! the main display) plus the reconfiguration callback that republishes them.

use super::MacShared;
use crate::PlatformEvent;
use core_graphics::display::{
    CGDisplay, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback, CGDirectDisplayID,
};
use splice_proto::DisplayRect;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

static NEXT_REGISTRATION: AtomicUsize = AtomicUsize::new(1);
static REGISTRATIONS: LazyLock<parking_lot::Mutex<HashMap<usize, Arc<MacShared>>>> =
    LazyLock::new(Default::default);

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

pub struct Registration(usize);

pub fn register(shared: Arc<MacShared>) -> crate::Result<Registration> {
    let id = NEXT_REGISTRATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| crate::PlatformError::Unavailable("display registration IDs exhausted".into()))?;
    REGISTRATIONS.lock().insert(id, shared);
    let result = unsafe {
        CGDisplayRegisterReconfigurationCallback(on_reconfigure, id as *const c_void)
    };
    if result != 0 {
        REGISTRATIONS.lock().remove(&id);
        return Err(crate::PlatformError::Unavailable(format!(
            "cannot monitor macOS display changes: CoreGraphics error {result}"
        )));
    }
    Ok(Registration(id))
}

impl Drop for Registration {
    fn drop(&mut self) {
        REGISTRATIONS.lock().remove(&self.0);
        unsafe {
            CGDisplayRemoveReconfigurationCallback(on_reconfigure, self.0 as *const c_void);
        }
    }
}

unsafe extern "C" fn on_reconfigure(_display: CGDirectDisplayID, flags: u32, user_info: *const c_void) {
    // The callback fires twice per change; the "before" pass still reports the old geometry.
    if CGDisplayChangeSummaryFlags::from_bits_retain(flags)
        .contains(CGDisplayChangeSummaryFlags::kCGDisplayBeginConfigurationFlag)
    {
        return;
    }
    let Some(shared) = REGISTRATIONS.lock().get(&(user_info as usize)).cloned() else {
        return;
    };
    let displays = snapshot();
    *shared.displays.write() = displays.clone();
    shared.emit(PlatformEvent::DisplaysChanged { displays });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unregistering_releases_display_observer_state() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(MacShared {
            tx,
            displays: parking_lot::RwLock::new(Vec::new()),
            health: parking_lot::Mutex::new(Default::default()),
        });
        let weak = Arc::downgrade(&shared);
        let registration = register(shared).unwrap();
        let id = registration.0;
        drop(registration);
        unsafe { on_reconfigure(0, 0, id as *const c_void) };
        assert!(weak.upgrade().is_none());
    }
}
