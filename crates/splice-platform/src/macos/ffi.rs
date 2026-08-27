//! Hand-written declarations for the macOS APIs the Rust bindings don't cover, plus the
//! raw CGEvent entry points injection needs (the `core-graphics` safe wrappers hide the
//! event pointer, and `CGEventCreateScrollWheelEvent2` is behind a feature we can't enable
//! from this crate's manifest).

#![allow(non_snake_case, non_upper_case_globals)]

use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionaryRef;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::event::{CGEventFlags, CGEventField, CGEventTapLocation, CGEventType, CGKeyCode};
use core_graphics::event_source::CGEventSourceStateID;
use core_graphics::geometry::CGPoint;
use core_graphics::sys::{CGEventRef, CGEventSourceRef};
use std::ffi::{c_char, c_int, c_void};

/// `kCGEventSourceUserData` value stamped on every event Splice injects. The capture tap
/// checks field 42 against this to tell our own injections from physical input.
pub const SPLICE_MAGIC: i64 = 0x53504C43;

/// Event fields the `core-graphics` `EventField` struct doesn't name.
pub const FIELD_SCROLL_PHASE: CGEventField = 99;
pub const FIELD_SCROLL_MOMENTUM_PHASE: CGEventField = 123;

/// `kCGScrollPhaseEnded`.
pub const SCROLL_PHASE_ENDED: i64 = 4;
/// `kCGScrollPhaseCancelled`.
pub const SCROLL_PHASE_CANCELLED: i64 = 8;

pub const kCGScrollEventUnitPixel: u32 = 0;
pub const kCGScrollEventUnitLine: u32 = 1;

pub type IOPMAssertionID = u32;
pub const kIOPMAssertionLevelOn: u32 = 255;
pub const kIOPMUserActiveLocal: c_int = 0;

#[cfg_attr(target_os = "macos", link(name = "CoreGraphics", kind = "framework"))]
extern "C" {
    pub fn CGEventSourceCreate(state_id: CGEventSourceStateID) -> CGEventSourceRef;
    pub fn CGEventSourceCounterForEventType(state_id: CGEventSourceStateID, event_type: u32)
        -> u32;
    pub fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: CGEventType,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        keycode: CGKeyCode,
        keydown: bool,
    ) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    pub fn CGEventSetType(event: CGEventRef, event_type: CGEventType);
    pub fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);
    pub fn CGEventSetIntegerValueField(event: CGEventRef, field: CGEventField, value: i64);
    pub fn CGEventSetDoubleValueField(event: CGEventRef, field: CGEventField, value: f64);
    pub fn CGEventPost(location: CGEventTapLocation, event: CGEventRef);

    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    pub fn CGEventTapIsEnabled(tap: CFMachPortRef) -> bool;

    pub fn CGPreflightPostEventAccess() -> bool;
    pub fn CGPreflightListenEventAccess() -> bool;
    pub fn CGRequestPostEventAccess() -> bool;

    /// Session dictionary; `kCGSSessionSecureInputPID` is present only while Secure Input
    /// is active.
    pub fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
}

#[cfg_attr(target_os = "macos", link(name = "ApplicationServices", kind = "framework"))]
extern "C" {
    /// Returns `Boolean` (unsigned char), not a C `int` — bind as u8 and normalize.
    pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    pub static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[cfg_attr(target_os = "macos", link(name = "Carbon", kind = "framework"))]
extern "C" {
    pub fn IsSecureEventInputEnabled() -> u8;
}

#[cfg_attr(target_os = "macos", link(name = "IOKit", kind = "framework"))]
extern "C" {
    pub fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        level: u32,
        name: CFStringRef,
        id: *mut IOPMAssertionID,
    ) -> c_int;
    pub fn IOPMAssertionRelease(id: IOPMAssertionID) -> c_int;
    pub fn IOPMAssertionDeclareUserActivity(
        name: CFStringRef,
        user_type: c_int,
        id: *mut IOPMAssertionID,
    ) -> c_int;
}

extern "C" {
    /// libproc, in libSystem — no extra link directive needed.
    pub fn proc_name(pid: c_int, buffer: *mut c_char, buffersize: u32) -> c_int;
}

/// RAII wrapper for a `CGEventRef` so injection paths can't leak events.
pub struct Event(pub CGEventRef);

impl Event {
    /// Returns `None` when the OS refused to create the event (missing post access).
    pub fn new(raw: CGEventRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Event(raw))
        }
    }

    pub fn set_int(&self, field: CGEventField, value: i64) {
        unsafe { CGEventSetIntegerValueField(self.0, field, value) }
    }

    pub fn set_double(&self, field: CGEventField, value: f64) {
        unsafe { CGEventSetDoubleValueField(self.0, field, value) }
    }

    pub fn set_flags(&self, flags: CGEventFlags) {
        unsafe { CGEventSetFlags(self.0, flags) }
    }

    /// Stamps the Splice magic and posts at the HID tap.
    pub fn post(&self) {
        self.set_int(core_graphics::event::EventField::EVENT_SOURCE_USER_DATA, SPLICE_MAGIC);
        unsafe { CGEventPost(CGEventTapLocation::HID, self.0) }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as *mut c_void) }
    }
}

/// The SkyLight SPI that lets a background (non-foreground) process own the cursor state.
/// Resolved through `dlsym` because SkyLight is private and may drop the symbol.
pub fn set_cursor_in_background(enable: bool) {
    type DefaultConnection = unsafe extern "C" fn() -> u32;
    type SetConnectionProperty =
        unsafe extern "C" fn(u32, u32, CFStringRef, CFTypeRef) -> c_int;

    unsafe {
        let default_conn: Option<DefaultConnection> =
            std::mem::transmute(dlsym_global(c"_CGSDefaultConnection"));
        let set_prop: Option<SetConnectionProperty> =
            std::mem::transmute(dlsym_global(c"CGSSetConnectionProperty"));
        let (Some(default_conn), Some(set_prop)) = (default_conn, set_prop) else {
            return;
        };
        let cid = default_conn();
        let key = CFString::new("SetsCursorInBackground");
        let value = if enable {
            core_foundation::boolean::CFBoolean::true_value()
        } else {
            core_foundation::boolean::CFBoolean::false_value()
        };
        set_prop(cid, cid, key.as_concrete_TypeRef(), value.as_CFTypeRef());
    }
}

fn dlsym_global(name: &std::ffi::CStr) -> *mut c_void {
    unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }
}

/// Process name for a pid via libproc; `None` if the pid is gone.
pub fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0i8; 256];
    let n = unsafe { proc_name(pid, buf.as_mut_ptr(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let bytes: Vec<u8> = buf[..n as usize].iter().map(|&c| c as u8).collect();
    String::from_utf8(bytes).ok()
}
