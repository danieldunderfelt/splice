//! Splice wire protocol.
//!
//! Length-prefixed (u32 BE) postcard-encoded [`Frame`]s over TCP (port 41717) between
//! tailnet peers. WireGuard provides transport encryption; Tailscale WhoIs provides
//! authentication — there is no crypto at this layer.
//!
//! # Evolution rules (postcard is positional, NOT self-describing)
//! - Never remove, reorder, or change the meaning of existing enum variants or struct fields.
//! - New frames/fields are ADDED at the end and gated on negotiated capabilities
//!   ([`Hello::caps`]); a peer must never send a frame the other side didn't advertise.
//! - `PROTO_VERSION` bumps only for incompatible framing changes (avoid forever).

pub mod framing;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Protocol version spoken by this build. Peers negotiate min(theirs, ours) and refuse
/// only if ranges are disjoint ("nobody gets turned away" — additions ride on caps instead).
pub const PROTO_VERSION: u16 = 1;
/// Well-known Splice TCP port on the tailnet.
pub const SPLICE_PORT: u16 = 41717;
/// Hard cap on a single frame (framing layer enforces).
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;
/// Clipboard payload chunk size.
pub const CLIP_CHUNK: usize = 256 * 1024;
/// Clipboard total size cap.
pub const CLIP_MAX_TOTAL: usize = 16 * 1024 * 1024;
/// Text at or below this length is inlined directly in `ClipOffer`.
pub const CLIP_INLINE_TEXT_MAX: usize = 64 * 1024;

/// Capability strings advertised in `Hello`. Constants so call sites can't typo them.
pub mod caps {
    /// Base input relay (motion/button/scroll/key, enter/leave, source claims).
    pub const INPUT_V1: &str = "input-v1";
    /// Clipboard offers + lazy fetch.
    pub const CLIPBOARD_V1: &str = "clipboard-v1";
    /// Layout replication.
    pub const LAYOUT_V1: &str = "layout-v1";
}

/// Stable machine identity = Tailscale `Node.StableID`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MachineId(pub String);

impl fmt::Debug for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MachineId({})", self.0)
    }
}
impl fmt::Display for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lamport-clocked value with writer tiebreak. Larger wins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub lamport: u64,
    pub writer: MachineId,
}

impl PartialOrd for Stamp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Stamp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.lamport, &self.writer.0).cmp(&(other.lamport, &other.writer.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Vec2I {
    pub x: i32,
    pub y: i32,
}

/// One display, in the machine's own logical coordinate space (macOS: CG points;
/// Linux: portal-zone logical pixels).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayRect {
    /// Stable-ish per-machine display identifier (CGDirectDisplayID / zone index).
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Backing scale factor (informational; coords are already logical).
    pub scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    Macos,
    Linux,
    Other,
}

/// Identity + runtime facts a machine shares about itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineInfo {
    pub id: MachineId,
    pub hostname: String,
    pub os: Os,
    pub displays: Vec<DisplayRect>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub proto_min: u16,
    pub proto_max: u16,
    pub machine: MachineInfo,
    pub caps: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    /// Chosen protocol version.
    pub proto: u16,
    pub machine: MachineInfo,
    /// Capabilities in effect = intersection.
    pub caps: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u8),
}

/// Input events. Motion deltas are source-accelerated logical px (f64), scaled by the
/// per-link sensitivity at the SOURCE before sending. Keys are raw evdev keycodes
/// (`linux/input-event-codes.h`; KEY_A=30) — never +8, never characters.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    Motion { dx: f64, dy: f64 },
    Button { button: PointerButton, pressed: bool },
    /// Smooth scroll in logical px, device direction (target applies its own natural-scroll).
    ScrollPixels { dx: f64, dy: f64 },
    /// Discrete scroll in value-120 units (one detent = 120), device direction.
    Scroll120 { dx: i32, dy: i32 },
    /// End of a scroll gesture; `cancel=false` → target may start kinetic scrolling.
    ScrollStop { cancel: bool },
    /// Raw evdev keycode; autorepeat is filtered at capture, regenerated at target.
    Key { code: u32, pressed: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaveReason {
    /// Cursor crossed back or onward; normal transition.
    Crossed,
    /// Local panic chord / disconnect-all on the source.
    Panic,
    /// Source lost sourceness (another machine claimed it).
    SourceChanged,
    /// Capture backend broke; session cannot continue.
    CaptureLost,
    /// Peer disabled / layout changed such that the session is invalid.
    Reconfigured,
}

/// Where a machine's own coordinate space sits in the shared arrangement canvas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachinePlacement {
    pub offset: Vec2I,
    pub enabled: bool,
}

/// Replicated arrangement document. Last-writer-wins by `stamp`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutDoc {
    pub stamp: Stamp,
    pub machines: BTreeMap<MachineId, MachinePlacement>,
    /// Per-link sensitivity factors, keyed by "smallerId|largerId" (unordered pair).
    pub sensitivity: BTreeMap<String, f64>,
}

impl LayoutDoc {
    /// Key for the unordered pair of machines in `sensitivity`.
    pub fn link_key(a: &MachineId, b: &MachineId) -> String {
        if a.0 <= b.0 {
            format!("{}|{}", a.0, b.0)
        } else {
            format!("{}|{}", b.0, a.0)
        }
    }
}

/// All frames on the wire. See module docs for evolution rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    Hello(Hello),
    Welcome(Welcome),
    /// Liveness + RTT. Echo `nonce` and `t_us` back in `Pong` untouched.
    Ping { nonce: u64, t_us: u64 },
    Pong { nonce: u64, t_us: u64 },
    /// "I have physical input; I am the source now." Highest stamp wins cluster-wide.
    SourceClaim { stamp: Stamp },
    /// Replicated layout; adopt iff `stamp` is newer than local.
    LayoutSync(LayoutDoc),
    /// Refreshed self-description (display hotplug etc.).
    MachineUpdate(MachineInfo),
    /// Source → target: session start. `pos` is in the TARGET's local logical coords.
    Enter { session: u64, pos: Vec2 },
    /// Source → target: input for the current session (stale sessions are dropped).
    Input { session: u64, ev: InputEvent },
    /// Source → target: session end.
    Leave { session: u64, reason: LeaveReason },
    /// Unconditional safety: receiver releases every held key/button it injected.
    ReleaseAll,
    /// Local clipboard changed. `mimes` in preference order. Small text rides inline.
    ClipOffer {
        id: u64,
        stamp: Stamp,
        mimes: Vec<String>,
        inline_text: Option<String>,
    },
    /// Ask the offering machine for one representation.
    ClipRequest { id: u64, mime: String },
    /// Chunked response; `last` marks the final chunk. Empty+last = representation gone.
    ClipChunk {
        id: u64,
        mime: String,
        data: Vec<u8>,
        last: bool,
    },
    /// Offer/request cannot be served (expired, over cap, converted away).
    ClipAbort { id: u64, reason: String },
    /// Graceful shutdown notice.
    Bye { reason: String },
}

/// Errors shared by framing and session-level protocol handling.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame exceeds MAX_FRAME_LEN: {0} bytes")]
    FrameTooLarge(u32),
    #[error("postcard: {0}")]
    Codec(#[from] postcard::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("peer speaks no compatible protocol version ({min}..={max})")]
    IncompatibleVersion { min: u16, max: u16 },
    #[error("expected Hello/Welcome, got another frame")]
    BadHandshake,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_ordering_prefers_higher_lamport_then_writer() {
        let a = Stamp { lamport: 2, writer: MachineId("aaa".into()) };
        let b = Stamp { lamport: 1, writer: MachineId("zzz".into()) };
        assert!(a > b);
        let c = Stamp { lamport: 2, writer: MachineId("bbb".into()) };
        assert!(c > a);
    }

    #[test]
    fn frame_roundtrip() {
        let f = Frame::Input {
            session: 7,
            ev: InputEvent::Motion { dx: -3.25, dy: 0.5 },
        };
        let bytes = postcard::to_allocvec(&f).unwrap();
        let back: Frame = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn link_key_is_unordered() {
        let a = MachineId("aa".into());
        let b = MachineId("bb".into());
        assert_eq!(LayoutDoc::link_key(&a, &b), LayoutDoc::link_key(&b, &a));
    }
}
