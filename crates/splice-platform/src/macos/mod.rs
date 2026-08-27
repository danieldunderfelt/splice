//! macOS backend: CGEventTap capture, CGEventPost emulation, NSPasteboard clipboard,
//! physical-activity via event-source tagging, Secure Input + permission health.
//!
//! READ `docs/research/macos-input.md` BEFORE IMPLEMENTING. Every rule in it exists
//! because a shipping KVM got it wrong.
//!
//! Structure (implement in these modules):
//!   tap.rs      — event tap thread (CFRunLoop), edge tests, capture swallow/forward,
//!                 disable-event handling, health poll, wake/session notifications,
//!                 panic-chord detection, physical-vs-injected discrimination (field 42).
//!   inject.rs   — CGEventPost emulation: modifiers as FlagsChanged + cumulative flags,
//!                 click-state & event numbers, scroll (line + pixel), key-repeat synth,
//!                 IOPMAssertion keep-awake, release_all ledger.
//!   cursor.rs   — hide/show + (dis)associate + warp, crash-safe re-association guard
//!                 (signal handlers + atexit + watchdog).
//!   pasteboard.rs — changeCount poll, offer normalization (TIFF→PNG), promised items
//!                 via NSPasteboardItemDataProvider, loop guard.
//!   displays.rs — CGGetActiveDisplayList → DisplayRect, reconfiguration callback.
//!   ffi.rs      — small extern "C" decls: IOPMAssertion*, CGS SetsCursorInBackground,
//!                 IsSecureEventInputEnabled, kCGSSessionSecureInputPID lookup.

use crate::{Platform, PlatformError, Result};
use std::path::PathBuf;

pub async fn create(_data_dir: PathBuf) -> Result<Platform> {
    Err(PlatformError::Unavailable(
        "macos backend not yet implemented".into(),
    ))
}
