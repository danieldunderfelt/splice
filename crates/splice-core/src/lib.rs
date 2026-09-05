//! Splice engine: peer sessions, source arbitration, focus FSM, layout/edge math,
//! held-input safety, clipboard broker, config persistence.
//!
//! The engine is a single tokio task owning all mutable state (message-passing, no shared
//! locks on the hot path), driven by:
//!   - platform events ([`splice_platform::PlatformEvent`])
//!   - per-peer session tasks (frames in/out over TCP)
//!   - discovery ticks (tailscale status)
//!   - UI commands ([`Command`])
//!
//! See docs/DESIGN.md — the FSM, arbitration, and safety rules are specified there.

pub mod config;
pub mod diagnostics;
pub mod updates;
mod clipboard;
pub mod engine;
pub mod arrange;
pub mod layout;
pub mod ledger;
pub mod net;
pub mod ui_state;

pub use config::Config;
pub use engine::{Command, Engine, EngineHandle};
pub use ui_state::*;
