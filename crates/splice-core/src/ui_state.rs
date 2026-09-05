//! UI-facing state snapshot. `splice-app` renders this and nothing else.

use serde::{Deserialize, Serialize};
use splice_platform::{BackendStatus, HealthReport};
use splice_proto::{DisplayRect, MachineId, Os, Vec2I};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UiConnection {
    /// This machine.
    SelfMachine,
    Direct { rtt_ms: f64 },
    Derp { rtt_ms: f64 },
    Connecting,
    Offline,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UiFocus {
    Local,
    /// Cursor is on the given remote machine (we are the source).
    Remote(MachineId),
    /// A remote source is driving this machine.
    Driven(MachineId),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiCrossing {
    pub from: MachineId,
    pub to: MachineId,
    pub progress: f32,
    pub side: splice_platform::EdgeSide,
    pub position: splice_proto::Vec2,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiState {
    pub crossing_progress: Option<UiCrossing>,
    pub input_settings: crate::input_settings::InputSettings,
    pub input_error: Option<String>,
    pub raw_active: bool,
    pub preparing_input: Option<MachineId>,
    pub updates: std::collections::BTreeMap<MachineId, crate::updates::UiUpdate>,
    pub restart_requested: bool,
    pub build: splice_proto::BuildInfo,
    pub diagnostics: crate::diagnostics::Diagnostics,
    pub self_id: MachineId,
    pub master_enabled: bool,
    pub clipboard_sync: bool,
    pub machines: Vec<UiMachine>,
    pub edges: Vec<UiEdge>,
    /// Which machine currently holds sourceness (None until first physical input).
    pub source: Option<MachineId>,
    pub focus: UiFocus,
    pub health: HealthReport,
    /// Human-readable panic chord, e.g. "Left Shift+Right Shift+Esc".
    pub panic_chord: String,
    /// Per-link sensitivity, keyed by LayoutDoc::link_key.
    pub sensitivity: std::collections::BTreeMap<String, f64>,
    /// True while Tailscale/LocalAPI is unreachable.
    pub tailscale_error: Option<String>,
    pub config_error: Option<String>,
    pub connection_errors: Vec<String>,
    /// Linux: which capture/injection/clipboard implementations are active.
    pub backends: Option<BackendStatus>,
}

impl UiState {
    pub fn initial(self_id: MachineId) -> Self {
        UiState {
            crossing_progress: None,
            input_settings: Default::default(),
            input_error: None,
            raw_active: false,
            preparing_input: None,
            updates: Default::default(),
            restart_requested: false,
            build: splice_proto::BuildInfo::current(),
            diagnostics: Default::default(),
            self_id,
            master_enabled: true,
            clipboard_sync: true,
            machines: Vec::new(),
            edges: Vec::new(),
            source: None,
            focus: UiFocus::Local,
            health: HealthReport::default(),
            panic_chord: "Left Shift+Right Shift+Esc".into(),
            sensitivity: Default::default(),
            tailscale_error: None,
            config_error: None,
            connection_errors: Vec::new(),
            backends: None,
        }
    }
}
