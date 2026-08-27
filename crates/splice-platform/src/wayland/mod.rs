//! Linux Wayland backend: InputCapture portal + reis receiver (capture), RemoteDesktop
//! portal + reis sender (emulation), Clipboard portal on the RemoteDesktop session,
//! evdev read-only monitor (physical activity), ScreenSaver inhibit (keep-awake).
//!
//! READ `docs/research/wayland-input.md` BEFORE IMPLEMENTING. Session-lifecycle rules
//! (never Disable(), never churn, single-use restore tokens) are load-bearing.
//!
//! Structure (implement in these modules):
//!   capture.rs   — InputCapture session, zones→DisplayRect, EdgeSpec→barriers,
//!                  Activated/Deactivated/ZonesChanged handling, reis receiver pump,
//!                  KDE barrier-id fallback, panic-chord detection from captured keys.
//!   emulate.rs   — RemoteDesktop session + reis sender pump, device management,
//!                  frames, scroll-120 accumulation, held-key ledger, release_all,
//!                  ScreenSaver inhibit while entered, screen-lock backoff.
//!   clipboard.rs — Clipboard portal on the RemoteDesktop session: SetSelection /
//!                  SelectionOwnerChanged / SelectionRead / SelectionTransfer+Write.
//!   activity.rs  — evdev read-only monitor with inotify hotplug + graceful degrade.
//!   tokens.rs    — restore-token persistence (atomic write to data_dir/tokens.json).

use crate::{Platform, PlatformError, PlatformOpts, Result};

pub async fn create(_opts: PlatformOpts) -> Result<Platform> {
    Err(PlatformError::Unavailable(
        "wayland backend not yet implemented".into(),
    ))
}
