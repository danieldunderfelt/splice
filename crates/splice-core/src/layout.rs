//! Layout & edge math: place machines' display sets on a shared canvas, compute crossable
//! edge links, map crossing positions, clamp cursor motion into display unions.
//!
//! All pure functions — fully unit-testable, no platform involvement.
//! Specified behaviors (see DESIGN.md "Layout model and edge math"):
//! - Per-display rects, never bounding boxes. Outer-boundary tests must respect
//!   non-rectangular unions (dead corners).
//! - Shared edges between machines A and B are the overlapping segments where an outer
//!   edge of A's union touches an outer edge of B's union in canvas coords (after the UI's
//!   snapping; tolerance ±2 px).
//! - Entry mapping preserves the position along the shared segment 1:1 (segments are the
//!   geometric overlap, equal length by construction).

use splice_proto::{DisplayRect, MachineId, MachinePlacement, Vec2, Vec2I};
use std::collections::BTreeMap;

/// Everything layout math needs to know about one machine.
#[derive(Clone, Debug)]
pub struct MachineGeom {
    pub id: MachineId,
    pub displays: Vec<DisplayRect>,
    pub placement: MachinePlacement,
    /// Machine is online + connected (affects `crossable`, not geometry).
    pub reachable: bool,
}

/// Directed crossable edge: leaving `from` through `side` lands on `to`.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeLink {
    pub from: MachineId,
    pub to: MachineId,
    pub side: splice_platform::EdgeSide,
    /// Boundary coordinate on the crossing axis, in FROM-machine local coords.
    pub at: i32,
    /// Overlap segment along the edge, in FROM-machine local coords.
    pub from_range: (i32, i32),
    /// Same segment in TO-machine local coords (equal length; maps 1:1).
    pub to_range: (i32, i32),
    /// Landing boundary coordinate on the crossing axis, in TO-machine local coords.
    pub to_at: i32,
}

/// Compute all directed crossable links between enabled machines.
pub fn compute_links(machines: &BTreeMap<MachineId, MachineGeom>) -> Vec<EdgeLink> {
    let _ = machines;
    todo!("implemented by core agent: pairwise outer-edge overlap in canvas coords")
}

/// Convert an EdgeLink set for `self_id` into capture EdgeSpecs (one per link, id = index).
pub fn edge_specs_for(links: &[EdgeLink], self_id: &MachineId) -> Vec<splice_platform::EdgeSpec> {
    let _ = (links, self_id);
    todo!("implemented by core agent")
}

/// Is `p` inside the union of `displays` (local coords)?
pub fn union_contains(displays: &[DisplayRect], p: Vec2) -> bool {
    displays.iter().any(|d| {
        p.x >= d.x as f64
            && p.x < (d.x as f64 + d.w as f64)
            && p.y >= d.y as f64
            && p.y < (d.y as f64 + d.h as f64)
    })
}

/// Clamp `p` to the nearest point inside the union (dead-zone aware: pick the display
/// whose clamped point is closest).
pub fn clamp_into_displays(displays: &[DisplayRect], p: Vec2) -> Vec2 {
    let _ = (displays, p);
    todo!("implemented by core agent")
}

/// Map a point in machine-local coords into canvas coords.
pub fn to_canvas(placement: &MachinePlacement, p: Vec2) -> Vec2 {
    Vec2 { x: p.x + placement.offset.x as f64, y: p.y + placement.offset.y as f64 }
}

/// Map a canvas point into machine-local coords.
pub fn to_local(placement: &MachinePlacement, p: Vec2) -> Vec2 {
    Vec2 { x: p.x - placement.offset.x as f64, y: p.y - placement.offset.y as f64 }
}

/// Snap a proposed placement offset so near-touching edges (within `tolerance`) become
/// exactly touching. Used by the UI on drag release AND by the engine when adopting a
/// LayoutSync (defensive re-snap).
pub fn snap_offset(
    moving: &[DisplayRect],
    proposed: Vec2I,
    others: &[(&[DisplayRect], Vec2I)],
    tolerance: i32,
) -> Vec2I {
    let _ = (moving, proposed, others, tolerance);
    todo!("implemented by core agent")
}

#[cfg(test)]
mod tests {
    // The core agent adds thorough tests here, including:
    // - two 1920x1080 machines side by side → one link each way, full-height range
    // - partial vertical overlap → range = overlap only
    // - three side by side → middle machine links both ways, outer pair NOT linked
    //   through the middle (edges blocked by adjacency)
    // - non-rectangular union (laptop + taller external) → no links on dead-corner spans
    // - disabled/unreachable machine → no links
    // - snap_offset magnetism within tolerance, no snap beyond
}
