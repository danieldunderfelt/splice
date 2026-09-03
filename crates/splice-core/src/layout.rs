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

use splice_platform::EdgeSide;
use splice_proto::{DisplayRect, MachineId, MachinePlacement, Vec2, Vec2I};
use std::collections::BTreeMap;

const EDGE_ALIGNMENT_TOLERANCE: i64 = 2;
pub const MIN_EDGE_OVERLAP: i64 = 32;

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

#[derive(Clone, Copy)]
struct RectBounds {
    left: i64,
    right: i64,
    top: i64,
    bottom: i64,
}

impl RectBounds {
    fn from_display(display: &DisplayRect) -> Option<Self> {
        if display.w == 0 || display.h == 0 {
            return None;
        }

        let left = i64::from(display.x);
        let top = i64::from(display.y);
        Some(Self {
            left,
            right: left + i64::from(display.w),
            top,
            bottom: top + i64::from(display.h),
        })
    }

}

#[derive(Clone, Copy)]
struct EdgeSegment {
    side: EdgeSide,
    at: i64,
    start: i64,
    end: i64,
}

impl EdgeSegment {
    fn translated(self, offset: Vec2I) -> Self {
        match self.side {
            EdgeSide::Left | EdgeSide::Right => Self {
                at: self.at + i64::from(offset.x),
                start: self.start + i64::from(offset.y),
                end: self.end + i64::from(offset.y),
                ..self
            },
            EdgeSide::Top | EdgeSide::Bottom => Self {
                at: self.at + i64::from(offset.y),
                start: self.start + i64::from(offset.x),
                end: self.end + i64::from(offset.x),
                ..self
            },
        }
    }
}

fn uncovered_segments(
    start: i64,
    end: i64,
    covered: impl IntoIterator<Item = (i64, i64)>,
) -> Vec<(i64, i64)> {
    let mut covered: Vec<_> = covered
        .into_iter()
        .map(|(covered_start, covered_end)| (covered_start.max(start), covered_end.min(end)))
        .filter(|(covered_start, covered_end)| covered_start < covered_end)
        .collect();
    covered.sort_unstable();

    let mut uncovered = Vec::new();
    let mut cursor = start;
    for (covered_start, covered_end) in covered {
        if cursor < covered_start {
            uncovered.push((cursor, covered_start));
        }
        cursor = cursor.max(covered_end);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        uncovered.push((cursor, end));
    }
    uncovered
}

fn outer_edges(displays: &[DisplayRect]) -> Vec<EdgeSegment> {
    let bounds: Vec<_> = displays.iter().map(RectBounds::from_display).collect();
    let mut edges = Vec::new();

    for (index, rect) in bounds.iter().copied().enumerate() {
        let Some(rect) = rect else {
            continue;
        };

        for side in [
            EdgeSide::Left,
            EdgeSide::Right,
            EdgeSide::Top,
            EdgeSide::Bottom,
        ] {
            let (at, start, end) = match side {
                EdgeSide::Left => (rect.left, rect.top, rect.bottom),
                EdgeSide::Right => (rect.right, rect.top, rect.bottom),
                EdgeSide::Top => (rect.top, rect.left, rect.right),
                EdgeSide::Bottom => (rect.bottom, rect.left, rect.right),
            };
            let covered = bounds
                .iter()
                .copied()
                .enumerate()
                .filter_map(|(other_index, other)| {
                    let other = other?;
                    if other_index == index {
                        return None;
                    }

                    match side {
                        EdgeSide::Left if other.left < at && other.right >= at => {
                            Some((other.top, other.bottom))
                        }
                        EdgeSide::Right if other.left <= at && other.right > at => {
                            Some((other.top, other.bottom))
                        }
                        EdgeSide::Top if other.top < at && other.bottom >= at => {
                            Some((other.left, other.right))
                        }
                        EdgeSide::Bottom if other.top <= at && other.bottom > at => {
                            Some((other.left, other.right))
                        }
                        _ => None,
                    }
                });

            edges.extend(uncovered_segments(start, end, covered).into_iter().map(
                |(start, end)| EdgeSegment {
                    side,
                    at,
                    start,
                    end,
                },
            ));
        }
    }

    edges
}

fn opposite_sides(from: EdgeSide, to: EdgeSide) -> bool {
    matches!(
        (from, to),
        (EdgeSide::Left, EdgeSide::Right)
            | (EdgeSide::Right, EdgeSide::Left)
            | (EdgeSide::Top, EdgeSide::Bottom)
            | (EdgeSide::Bottom, EdgeSide::Top)
    )
}

fn edge_link(
    from: &MachineGeom,
    from_edge: EdgeSegment,
    to: &MachineGeom,
    to_edge: EdgeSegment,
) -> Option<EdgeLink> {
    if !opposite_sides(from_edge.side, to_edge.side)
        || (from_edge.at - to_edge.at).abs() > EDGE_ALIGNMENT_TOLERANCE
    {
        return None;
    }

    let overlap_start = from_edge.start.max(to_edge.start);
    let overlap_end = from_edge.end.min(to_edge.end);
    if overlap_end - overlap_start < MIN_EDGE_OVERLAP {
        return None;
    }

    let vertical = matches!(from_edge.side, EdgeSide::Left | EdgeSide::Right);
    let from_cross_offset = if vertical {
        from.placement.offset.x
    } else {
        from.placement.offset.y
    };
    let to_cross_offset = if vertical {
        to.placement.offset.x
    } else {
        to.placement.offset.y
    };
    let from_along_offset = if vertical {
        from.placement.offset.y
    } else {
        from.placement.offset.x
    };
    let to_along_offset = if vertical {
        to.placement.offset.y
    } else {
        to.placement.offset.x
    };

    Some(EdgeLink {
        from: from.id.clone(),
        to: to.id.clone(),
        side: from_edge.side,
        at: i32::try_from(from_edge.at - i64::from(from_cross_offset)).ok()?,
        from_range: (
            i32::try_from(overlap_start - i64::from(from_along_offset)).ok()?,
            i32::try_from(overlap_end - i64::from(from_along_offset)).ok()?,
        ),
        to_range: (
            i32::try_from(overlap_start - i64::from(to_along_offset)).ok()?,
            i32::try_from(overlap_end - i64::from(to_along_offset)).ok()?,
        ),
        to_at: i32::try_from(to_edge.at - i64::from(to_cross_offset)).ok()?,
    })
}

/// Compute all directed crossable links between enabled machines.
pub fn compute_links(machines: &BTreeMap<MachineId, MachineGeom>) -> Vec<EdgeLink> {
    let placed: Vec<_> = machines
        .values()
        .filter(|machine| machine.placement.enabled && machine.reachable)
        .map(|machine| {
            let edges = outer_edges(&machine.displays)
                .into_iter()
                .map(|edge| edge.translated(machine.placement.offset))
                .collect::<Vec<_>>();
            (machine, edges)
        })
        .collect();
    let mut links = Vec::new();

    for (from_index, (from, from_edges)) in placed.iter().enumerate() {
        for (to_index, (to, to_edges)) in placed.iter().enumerate() {
            if from_index == to_index {
                continue;
            }

            for &from_edge in from_edges {
                for &to_edge in to_edges {
                    if let Some(link) = edge_link(from, from_edge, to, to_edge) {
                        links.push(link);
                    }
                }
            }
        }
    }

    links
}

/// Convert an EdgeLink set for `self_id` into capture EdgeSpecs (one per link, id = index).
pub fn edge_specs_for(links: &[EdgeLink], self_id: &MachineId) -> Vec<splice_platform::EdgeSpec> {
    links
        .iter()
        .filter(|link| &link.from == self_id)
        .enumerate()
        .map(|(index, link)| splice_platform::EdgeSpec {
            id: index as u32,
            side: link.side,
            at: link.at,
            from: link.from_range.0,
            to: link.from_range.1,
        })
        .collect()
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
    let mut nearest = None;

    for display in displays {
        if display.w == 0 || display.h == 0 {
            continue;
        }

        let left = f64::from(display.x);
        let right = left + f64::from(display.w);
        let top = f64::from(display.y);
        let bottom = top + f64::from(display.h);
        let candidate = Vec2 {
            x: p.x.clamp(left, right - 1.0),
            y: p.y.clamp(top, bottom - 1.0),
        };
        let distance_squared = (candidate.x - p.x).powi(2) + (candidate.y - p.y).powi(2);

        if nearest.is_none_or(|(_, nearest_distance)| distance_squared < nearest_distance) {
            nearest = Some((candidate, distance_squared));
        }
    }

    nearest.map_or(p, |(candidate, _)| candidate)
}

/// Map a point in machine-local coords into canvas coords.
pub fn to_canvas(placement: &MachinePlacement, p: Vec2) -> Vec2 {
    Vec2 {
        x: p.x + placement.offset.x as f64,
        y: p.y + placement.offset.y as f64,
    }
}

/// Map a canvas point into machine-local coords.
pub fn to_local(placement: &MachinePlacement, p: Vec2) -> Vec2 {
    Vec2 {
        x: p.x - placement.offset.x as f64,
        y: p.y - placement.offset.y as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine_id(value: &str) -> MachineId {
        MachineId(value.to_owned())
    }

    fn display(id: &str, x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
        DisplayRect {
            id: id.to_owned(),
            x,
            y,
            w,
            h,
            scale: 1.0,
        }
    }

    fn machine(
        id: &str,
        displays: Vec<DisplayRect>,
        offset: (i32, i32),
    ) -> (MachineId, MachineGeom) {
        let id = machine_id(id);
        (
            id.clone(),
            MachineGeom {
                id,
                displays,
                placement: MachinePlacement {
                    offset: Vec2I {
                        x: offset.0,
                        y: offset.1,
                    },
                    enabled: true,
                },
                reachable: true,
            },
        )
    }

    fn layout(
        machines: impl IntoIterator<Item = (MachineId, MachineGeom)>,
    ) -> BTreeMap<MachineId, MachineGeom> {
        machines.into_iter().collect()
    }

    fn find_link<'a>(links: &'a [EdgeLink], from: &str, to: &str) -> &'a EdgeLink {
        links
            .iter()
            .find(|link| link.from.0 == from && link.to.0 == to)
            .unwrap_or_else(|| panic!("missing link {from} -> {to}"))
    }

    #[test]
    fn two_machines_side_by_side_link_in_both_directions() {
        let machines = layout([
            machine("a", vec![display("a-display", 0, 0, 1920, 1080)], (0, 0)),
            machine("b", vec![display("b-display", 0, 0, 1920, 1080)], (1920, 0)),
        ]);

        let links = compute_links(&machines);

        assert_eq!(links.len(), 2);
        assert_eq!(
            find_link(&links, "a", "b"),
            &EdgeLink {
                from: machine_id("a"),
                to: machine_id("b"),
                side: EdgeSide::Right,
                at: 1920,
                from_range: (0, 1080),
                to_range: (0, 1080),
                to_at: 0,
            }
        );
        assert_eq!(
            find_link(&links, "b", "a"),
            &EdgeLink {
                from: machine_id("b"),
                to: machine_id("a"),
                side: EdgeSide::Left,
                at: 0,
                from_range: (0, 1080),
                to_range: (0, 1080),
                to_at: 1920,
            }
        );
    }

    #[test]
    fn partial_vertical_overlap_uses_only_the_shared_range() {
        let machines = layout([
            machine("a", vec![display("a-display", 0, 0, 100, 100)], (0, 0)),
            machine("b", vec![display("b-display", 0, 0, 100, 50)], (100, 25)),
        ]);

        let links = compute_links(&machines);

        assert_eq!(links.len(), 2);
        let a_to_b = find_link(&links, "a", "b");
        assert_eq!(a_to_b.from_range, (25, 75));
        assert_eq!(a_to_b.to_range, (0, 50));
        let b_to_a = find_link(&links, "b", "a");
        assert_eq!(b_to_a.from_range, (0, 50));
        assert_eq!(b_to_a.to_range, (25, 75));
    }

    #[test]
    fn horizontal_links_convert_canvas_overlap_to_each_local_space() {
        let machines = layout([
            machine("a", vec![display("a-display", -50, 10, 100, 80)], (20, 30)),
            machine("b", vec![display("b-display", 0, 0, 60, 40)], (-10, 120)),
        ]);

        let links = compute_links(&machines);

        assert_eq!(links.len(), 2);
        assert_eq!(
            find_link(&links, "a", "b"),
            &EdgeLink {
                from: machine_id("a"),
                to: machine_id("b"),
                side: EdgeSide::Bottom,
                at: 90,
                from_range: (-30, 30),
                to_range: (0, 60),
                to_at: 0,
            }
        );
        assert_eq!(
            find_link(&links, "b", "a"),
            &EdgeLink {
                from: machine_id("b"),
                to: machine_id("a"),
                side: EdgeSide::Top,
                at: 0,
                from_range: (0, 60),
                to_range: (-30, 30),
                to_at: 90,
            }
        );
    }

    #[test]
    fn three_side_by_side_only_link_adjacent_machines() {
        let machines = layout([
            machine("a", vec![display("a-display", 0, 0, 100, 100)], (0, 0)),
            machine("b", vec![display("b-display", 0, 0, 100, 100)], (100, 0)),
            machine("c", vec![display("c-display", 0, 0, 100, 100)], (200, 0)),
        ]);

        let links = compute_links(&machines);

        assert_eq!(links.len(), 4);
        assert_eq!(find_link(&links, "b", "a").side, EdgeSide::Left);
        assert_eq!(find_link(&links, "b", "c").side, EdgeSide::Right);
        assert!(!links.iter().any(|link| {
            (link.from.0 == "a" && link.to.0 == "c") || (link.from.0 == "c" && link.to.0 == "a")
        }));
    }

    #[test]
    fn non_rectangular_union_excludes_dead_corner_spans() {
        let machines = layout([
            machine(
                "a",
                vec![
                    display("laptop", 0, 50, 100, 50),
                    display("external", 100, 0, 100, 150),
                ],
                (0, 0),
            ),
            machine("b", vec![display("b-display", 0, 0, 100, 150)], (-100, 0)),
        ]);

        let links = compute_links(&machines);

        assert_eq!(links.len(), 2);
        assert_eq!(find_link(&links, "a", "b").from_range, (50, 100));
        assert_eq!(find_link(&links, "b", "a").from_range, (50, 100));
    }

    #[test]
    fn disabled_or_unreachable_machines_do_not_link() {
        let (a_id, a) = machine("a", vec![display("a-display", 0, 0, 100, 100)], (0, 0));
        let (b_id, mut b) = machine("b", vec![display("b-display", 0, 0, 100, 100)], (100, 0));
        b.placement.enabled = false;
        let mut machines = layout([(a_id, a), (b_id.clone(), b)]);

        assert!(compute_links(&machines).is_empty());

        let b = machines.get_mut(&b_id).unwrap();
        b.placement.enabled = true;
        b.reachable = false;
        assert!(compute_links(&machines).is_empty());
    }

    #[test]
    fn alignment_tolerance_accepts_two_pixels_and_rejects_slivers() {
        let machines = layout([
            machine("a", vec![display("a-display", 0, 0, 100, 100)], (0, 0)),
            machine("b", vec![display("b-display", 0, 0, 100, 32)], (102, 0)),
        ]);
        assert_eq!(compute_links(&machines).len(), 2);

        let machines = layout([
            machine("a", vec![display("a-display", 0, 0, 100, 100)], (0, 0)),
            machine("b", vec![display("b-display", 0, 0, 100, 31)], (102, 0)),
        ]);
        assert!(compute_links(&machines).is_empty());

        let machines = layout([
            machine("a", vec![display("a-display", 0, 0, 100, 100)], (0, 0)),
            machine("b", vec![display("b-display", 0, 0, 100, 100)], (103, 0)),
        ]);
        assert!(compute_links(&machines).is_empty());
    }

    #[test]
    fn edge_specs_filter_outgoing_links_and_number_the_result() {
        let links = vec![
            EdgeLink {
                from: machine_id("other"),
                to: machine_id("self"),
                side: EdgeSide::Left,
                at: 0,
                from_range: (0, 50),
                to_range: (10, 60),
                to_at: 100,
            },
            EdgeLink {
                from: machine_id("self"),
                to: machine_id("right"),
                side: EdgeSide::Right,
                at: 100,
                from_range: (20, 80),
                to_range: (0, 60),
                to_at: 0,
            },
            EdgeLink {
                from: machine_id("self"),
                to: machine_id("top"),
                side: EdgeSide::Top,
                at: 0,
                from_range: (30, 90),
                to_range: (5, 65),
                to_at: 100,
            },
        ];

        assert_eq!(
            edge_specs_for(&links, &machine_id("self")),
            vec![
                splice_platform::EdgeSpec {
                    id: 0,
                    side: EdgeSide::Right,
                    at: 100,
                    from: 20,
                    to: 80,
                },
                splice_platform::EdgeSpec {
                    id: 1,
                    side: EdgeSide::Top,
                    at: 0,
                    from: 30,
                    to: 90,
                },
            ]
        );
    }

    #[test]
    fn clamp_uses_the_nearest_display_and_first_display_for_ties() {
        let displays = vec![
            display("left", 0, 0, 100, 100),
            display("right", 200, 0, 100, 100),
        ];

        assert_eq!(
            clamp_into_displays(&displays, Vec2 { x: 149.5, y: 120.0 }),
            Vec2 { x: 99.0, y: 99.0 }
        );
        assert_eq!(
            clamp_into_displays(&displays, Vec2 { x: 240.5, y: 30.25 }),
            Vec2 { x: 240.5, y: 30.25 }
        );
        assert_eq!(
            clamp_into_displays(&[], Vec2 { x: 3.0, y: 4.0 }),
            Vec2 { x: 3.0, y: 4.0 }
        );
    }

    #[test]
    fn to_canvas_and_to_local_roundtrip() {
        let placement = MachinePlacement {
            offset: Vec2I { x: -137, y: 82 },
            enabled: true,
        };
        let local = Vec2 { x: 12.75, y: -99.5 };

        assert_eq!(to_local(&placement, to_canvas(&placement, local)), local);
    }
}
