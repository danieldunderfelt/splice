//! UI-facing state snapshot. `splice-app` renders this and nothing else.

use serde::Serialize;
use splice_platform::HealthReport;
use splice_proto::{DisplayRect, MachineId, Os, Vec2I};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum UiConnection {
    /// This machine.
    SelfMachine,
    Direct { rtt_ms: f64 },
    Derp { rtt_ms: f64 },
    Connecting,
    Offline,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UiMachine {
    pub id: MachineId,
    pub hostname: String,
    pub os: Os,
    pub displays: Vec<DisplayRect>,
    /// Placement of this machine's coordinate space in the shared canvas.
    pub offset: Vec2I,
    pub enabled: bool,
    pub connection: UiConnection,
    pub is_source: bool,
}

/// A shared-edge strip between two machines, for canvas rendering.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UiEdge {
    pub a: MachineId,
    pub b: MachineId,
    /// Canvas coords of the shared segment (vertical edge: x1==x2).
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    /// True when the cursor can actually cross here (both enabled + online + connected).
    pub crossable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum UiFocus {
    Local,
    /// Cursor is on the given remote machine (we are the source).
    Remote(MachineId),
    /// A remote source is driving this machine.
    Driven(MachineId),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UiState {
    pub self_id: MachineId,
    pub master_enabled: bool,
    pub clipboard_sync: bool,
    pub machines: Vec<UiMachine>,
    pub edges: Vec<UiEdge>,
    /// Which machine currently holds sourceness (None until first physical input).
    pub source: Option<MachineId>,
    pub focus: UiFocus,
    pub health: HealthReport,
    /// Human-readable panic chord, e.g. "Ctrl+Alt+Shift+Esc".
    pub panic_chord: String,
    /// Per-link sensitivity, keyed by LayoutDoc::link_key.
    pub sensitivity: std::collections::BTreeMap<String, f64>,
    /// True while Tailscale/LocalAPI is unreachable.
    pub tailscale_error: Option<String>,
}

impl UiState {
    pub fn initial(self_id: MachineId) -> Self {
        UiState {
            self_id,
            master_enabled: true,
            clipboard_sync: true,
            machines: Vec::new(),
            edges: Vec::new(),
            source: None,
            focus: UiFocus::Local,
            health: HealthReport::default(),
            panic_chord: "Ctrl+Alt+Shift+Esc".into(),
            sensitivity: Default::default(),
            tailscale_error: None,
        }
    }
}
