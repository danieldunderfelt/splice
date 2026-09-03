//! Arrangement rules for the canvas. Every machine is a body (its display rects in canvas
//! coordinates) that must touch a neighbour along a seam, never overlap another body, and
//! stay part of one connected cluster. Pure geometry; no platform or UI involvement.
//!
//! [`resolve`] finds the nearest position to a desired one where a body touches the fixed
//! bodies along a seam of at least [`Rules::min_seam`] and overlaps none of them. Sliding
//! along a seam, flipping around a corner and pushing through a neighbour all fall out of
//! "nearest valid position". [`drag_step`] builds a whole pointer step on top of it: the
//! dragged body is placed, then every cluster it stopped holding in place is settled.
//! [`normalize`] repairs any arrangement (a saved one, or one whose display geometry
//! changed) into a single connected, overlap-free cluster with the smallest moves.

use splice_platform::EdgeSide;
use splice_proto::{DisplayRect, Vec2I};

/// Shortest seam two displays may share, in canvas units. Leaves a dead corner so
/// displays never meet corner-to-corner and the cursor always has room to cross.
pub const MIN_SEAM: i64 = 160;
/// Fraction of `min_seam` by which the side a body already rests on wins ties, so a
/// card never chatters between two seams when the pointer sits on their bisector.
const STICKINESS: i64 = 8;
/// Fraction of `min_seam` by which staying put wins, so pointer tremor cannot flip a
/// body between two near-equal placements.
const INERTIA: i64 = 4;

const SIDES: [EdgeSide; 4] = [
    EdgeSide::Left,
    EdgeSide::Right,
    EdgeSide::Top,
    EdgeSide::Bottom,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    pub min_seam: i64,
    /// Flush edges (both starts, both ends) and centred placement attract within this
    /// distance along a seam.
    pub align_tolerance: i64,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            min_seam: MIN_SEAM,
            align_tolerance: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub left: i64,
    pub top: i64,
    pub right: i64,
    pub bottom: i64,
}

impl Bounds {
    pub fn of(display: &DisplayRect, offset: Vec2I) -> Option<Self> {
        if display.w == 0 || display.h == 0 {
            return None;
        }
        let left = i64::from(display.x) + i64::from(offset.x);
        let top = i64::from(display.y) + i64::from(offset.y);
        Some(Bounds {
            left,
            top,
            right: left + i64::from(display.w),
            bottom: top + i64::from(display.h),
        })
    }

    fn shifted(self, dx: i64, dy: i64) -> Self {
        Bounds {
            left: self.left + dx,
            top: self.top + dy,
            right: self.right + dx,
            bottom: self.bottom + dy,
        }
    }

    fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }

    fn area(self) -> i64 {
        self.width() * self.height()
    }

    fn width(self) -> i64 {
        self.right - self.left
    }

    fn height(self) -> i64 {
        self.bottom - self.top
    }

    fn intersection(self, other: Self) -> Option<Self> {
        self.intersects(other).then(|| Bounds {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        })
    }

    fn union(self, other: Self) -> Self {
        Bounds {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// Extents across the seam axis and along it, for a seam on `side`.
    fn spans(self, side: EdgeSide) -> (Span, Span) {
        let x = Span {
            start: self.left,
            end: self.right,
        };
        let y = Span {
            start: self.top,
            end: self.bottom,
        };
        match side {
            EdgeSide::Left | EdgeSide::Right => (x, y),
            EdgeSide::Top | EdgeSide::Bottom => (y, x),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: i64,
    end: i64,
}

impl Span {
    fn len(self) -> i64 {
        self.end - self.start
    }

    fn shifted(self, by: i64) -> Self {
        Span {
            start: self.start + by,
            end: self.end + by,
        }
    }

    fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    fn overlap(self, other: Self) -> Option<Span> {
        let span = Span {
            start: self.start.max(other.start),
            end: self.end.min(other.end),
        };
        (span.len() > 0).then_some(span)
    }
}

/// A machine's displays in canvas coordinates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Body {
    pub rects: Vec<Bounds>,
}

impl Body {
    pub fn new(displays: &[DisplayRect], offset: Vec2I) -> Self {
        Body {
            rects: displays
                .iter()
                .filter_map(|display| Bounds::of(display, offset))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn shifted(&self, delta: Vec2I) -> Self {
        let (dx, dy) = (i64::from(delta.x), i64::from(delta.y));
        Body {
            rects: self.rects.iter().map(|rect| rect.shifted(dx, dy)).collect(),
        }
    }

    pub fn bounds(&self) -> Option<Bounds> {
        self.rects.iter().copied().reduce(Bounds::union)
    }

    fn overlaps(&self, other: &Body) -> bool {
        self.rects
            .iter()
            .any(|rect| other.rects.iter().any(|other| rect.intersects(*other)))
    }

    fn area(&self) -> i64 {
        self.rects.iter().map(|rect| rect.area()).sum()
    }

    fn merged<'a>(bodies: impl IntoIterator<Item = &'a Body>) -> Self {
        Body {
            rects: bodies
                .into_iter()
                .flat_map(|body| body.rects.iter().copied())
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Translation to apply to the moving body.
    pub delta: Vec2I,
    /// Which of the moving body's own sides rests against a neighbour.
    pub side: EdgeSide,
}

/// Shared boundary segment between two bodies, in canvas coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeamSegment {
    pub a: usize,
    pub b: usize,
    pub from: (i64, i64),
    pub to: (i64, i64),
}

/// Nearest translation to `desired` that rests `moving` against one of `fixed` along a
/// seam of at least `rules.min_seam` without overlapping any fixed body. `current` is
/// the side the body rests on now: it wins near-ties, staying put wins by more, and the
/// perpendicular sides only come into play once the seam the body rests on has run out,
/// so a straight push goes through to the far side rather than over the top. None when
/// there is nothing to attach to.
pub fn resolve(
    moving: &Body,
    desired: Vec2I,
    fixed: &[Body],
    rules: &Rules,
    current: Option<EdgeSide>,
) -> Option<Placement> {
    let fixed: Vec<&Body> = fixed.iter().collect();
    attach(moving, desired, &fixed, &fixed, rules, current)
}

/// [`resolve`] with the bodies to rest against (`targets`) chosen separately from the
/// bodies that block placement (`blocking`); a target absent from `blocking` can be
/// overlapped, which is how the dragged body pushes its riders.
fn attach(
    moving: &Body,
    desired: Vec2I,
    targets: &[&Body],
    blocking: &[&Body],
    rules: &Rules,
    current: Option<EdgeSide>,
) -> Option<Placement> {
    let probe = Probe {
        moving,
        moving_bounds: moving.bounds()?,
        blocking,
        rules,
        desired: (i64::from(desired.x), i64::from(desired.y)),
    };
    let stickiness = (rules.min_seam / STICKINESS) as f64;
    let mut candidates = Vec::new();
    for body in targets.iter().copied() {
        if body.is_empty() {
            continue;
        }
        for anchor in &body.rects {
            for rect in &moving.rects {
                for side in SIDES {
                    if let Some(rest) = probe.rest(*rect, *anchor, body, side) {
                        candidates.push((side, rest));
                    }
                }
            }
        }
    }

    let in_axis = |side: EdgeSide| current.is_none_or(|now| now == side || now == opposite(side));
    let distance = |rest: &Rest| {
        ((rest.delta.0 - probe.desired.0) as f64).hypot((rest.delta.1 - probe.desired.1) as f64)
    };
    let nearest = |sides: &dyn Fn(EdgeSide) -> bool| {
        candidates
            .iter()
            .filter(|(side, _)| sides(*side))
            .min_by(|(_, a), (_, b)| distance(a).total_cmp(&distance(b)))
    };
    let nearest_in_axis = nearest(&|side| Some(side) == current).or_else(|| nearest(&in_axis));
    if nearest_in_axis.is_some_and(|(_, rest)| !rest.exhausted) {
        candidates.retain(|(side, _)| in_axis(*side));
    }

    let mut best: Option<(f64, Placement)> = None;
    for (side, rest) in candidates {
        let (dx, dy) = (
            (rest.delta.0 - probe.desired.0) as f64,
            (rest.delta.1 - probe.desired.1) as f64,
        );
        let mut score = dx.hypot(dy);
        if current == Some(side) {
            score -= stickiness;
        }
        if rest.delta == (0, 0) {
            score -= (rules.min_seam / INERTIA) as f64;
        }
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score < *best_score)
        {
            best = Some((
                score,
                Placement {
                    delta: Vec2I {
                        x: to_i32(rest.delta.0),
                        y: to_i32(rest.delta.1),
                    },
                    side,
                },
            ));
        }
    }
    best.map(|(_, placement)| placement)
}

fn opposite(side: EdgeSide) -> EdgeSide {
    match side {
        EdgeSide::Left => EdgeSide::Right,
        EdgeSide::Right => EdgeSide::Left,
        EdgeSide::Top => EdgeSide::Bottom,
        EdgeSide::Bottom => EdgeSide::Top,
    }
}

/// Where a seam candidate puts the body, and whether the seam ran out (or was blocked)
/// before reaching the desired position along it.
struct Rest {
    delta: (i64, i64),
    exhausted: bool,
}

struct Probe<'a> {
    moving: &'a Body,
    moving_bounds: Bounds,
    blocking: &'a [&'a Body],
    rules: &'a Rules,
    desired: (i64, i64),
}

impl Probe<'_> {
    /// Translation resting `rect`'s `side` against `anchor`, slid along the seam to the
    /// point nearest the desired position that keeps the whole body overlap-free.
    fn rest(&self, rect: Bounds, anchor: Bounds, neighbour: &Body, side: EdgeSide) -> Option<Rest> {
        let neighbour_bounds = neighbour.bounds()?;
        let cross_delta = match side {
            EdgeSide::Left => anchor.right - rect.left,
            EdgeSide::Right => anchor.left - rect.right,
            EdgeSide::Top => anchor.bottom - rect.top,
            EdgeSide::Bottom => anchor.top - rect.bottom,
        };
        let (_, rect_along) = rect.spans(side);
        let (_, anchor_along) = anchor.spans(side);
        let seam = self
            .rules
            .min_seam
            .min(rect_along.len())
            .min(anchor_along.len())
            .max(1);
        let low = anchor_along.start - rect_along.end + seam;
        let high = anchor_along.end - rect_along.start - seam;
        if low > high {
            return None;
        }

        let mut forbidden = Vec::new();
        for moving_rect in &self.moving.rects {
            let (moving_cross, moving_along) = moving_rect.spans(side);
            let moving_cross = moving_cross.shifted(cross_delta);
            for fixed_rect in self
                .blocking
                .iter()
                .flat_map(|body| &body.rects)
                .chain(&neighbour.rects)
            {
                let (fixed_cross, fixed_along) = fixed_rect.spans(side);
                if moving_cross.overlaps(fixed_cross) {
                    forbidden.push((
                        fixed_along.start - moving_along.end,
                        fixed_along.end - moving_along.start,
                    ));
                }
            }
        }
        forbidden.sort_unstable();

        let vertical = matches!(side, EdgeSide::Left | EdgeSide::Right);
        let target = if vertical {
            self.desired.1
        } else {
            self.desired.0
        };
        let mut along = nearest_free(low, high, &forbidden, target)?;
        let exhausted = along != target;

        let (_, moving_along) = self.moving_bounds.spans(side);
        let (_, neighbour_along) = neighbour_bounds.spans(side);
        let flush = [
            neighbour_along.start - moving_along.start,
            neighbour_along.end - moving_along.end,
            (neighbour_along.start + neighbour_along.end - moving_along.start - moving_along.end)
                / 2,
        ];
        let free = |position: i64| {
            (low..=high).contains(&position)
                && !forbidden
                    .iter()
                    .any(|&(start, end)| position > start && position < end)
        };
        let attracts = |position: i64| {
            (position - along).abs() <= self.rules.align_tolerance && free(position)
        };
        if flush.contains(&0) && attracts(0) {
            along = 0;
        } else if let Some(aligned) = flush
            .into_iter()
            .filter(|&position| attracts(position))
            .min_by_key(|&position| (position - along).abs())
        {
            along = aligned;
        }

        Some(Rest {
            delta: if vertical {
                (cross_delta, along)
            } else {
                (along, cross_delta)
            },
            exhausted,
        })
    }
}

/// Nearest point to `target` within `[low, high]` outside every open `forbidden`
/// interval (sorted by start).
fn nearest_free(low: i64, high: i64, forbidden: &[(i64, i64)], target: i64) -> Option<i64> {
    let mut best: Option<i64> = None;
    let mut consider = |start: i64, end: i64| {
        if start <= end {
            let point = target.clamp(start, end);
            if best.is_none_or(|current| (point - target).abs() < (current - target).abs()) {
                best = Some(point);
            }
        }
    };
    let mut cursor = low;
    for &(start, end) in forbidden {
        if start >= cursor {
            consider(cursor, start.min(high));
        }
        cursor = cursor.max(end);
        if cursor > high {
            break;
        }
    }
    if cursor <= high {
        consider(cursor, high);
    }
    best
}

fn shared_edge(rect: Bounds, other: Bounds) -> Option<(EdgeSide, i64, Span)> {
    let side = if rect.right == other.left {
        EdgeSide::Right
    } else if rect.left == other.right {
        EdgeSide::Left
    } else if rect.bottom == other.top {
        EdgeSide::Bottom
    } else if rect.top == other.bottom {
        EdgeSide::Top
    } else {
        return None;
    };
    let at = match side {
        EdgeSide::Left => rect.left,
        EdgeSide::Right => rect.right,
        EdgeSide::Top => rect.top,
        EdgeSide::Bottom => rect.bottom,
    };
    let (_, rect_along) = rect.spans(side);
    let (_, other_along) = other.spans(side);
    rect_along.overlap(other_along).map(|span| (side, at, span))
}

/// The side of `rect` sharing a legal seam with `other`, if any.
fn seam_side(rect: Bounds, other: Bounds, rules: &Rules) -> Option<EdgeSide> {
    let (side, _, span) = shared_edge(rect, other)?;
    let (_, rect_along) = rect.spans(side);
    let (_, other_along) = other.spans(side);
    let least = rules
        .min_seam
        .min(rect_along.len())
        .min(other_along.len())
        .max(1);
    (span.len() >= least).then_some(side)
}

/// Do two bodies share a seam long enough under `rules`?
pub fn touching(a: &Body, b: &Body, rules: &Rules) -> bool {
    resting_side(a, std::slice::from_ref(b), rules).is_some()
}

/// The side of `body` resting against any of `others` along a legal seam, if any.
pub fn resting_side(body: &Body, others: &[Body], rules: &Rules) -> Option<EdgeSide> {
    body.rects.iter().find_map(|rect| {
        others
            .iter()
            .flat_map(|other| &other.rects)
            .find_map(|other| seam_side(*rect, *other, rules))
    })
}

/// Connected clusters of bodies (indices), joined by [`touching`]. Bodies without
/// displays belong to no cluster.
pub fn components(bodies: &[Body], rules: &Rules) -> Vec<Vec<usize>> {
    let all: Vec<usize> = (0..bodies.len()).collect();
    clusters(bodies, &all, rules)
}

fn clusters(bodies: &[Body], members: &[usize], rules: &Rules) -> Vec<Vec<usize>> {
    let mut assigned = vec![false; bodies.len()];
    let mut clusters = Vec::new();
    for &seed in members {
        if assigned[seed] || bodies[seed].is_empty() {
            continue;
        }
        assigned[seed] = true;
        let mut cluster = vec![seed];
        let mut cursor = 0;
        while cursor < cluster.len() {
            let current = cluster[cursor];
            cursor += 1;
            for &candidate in members {
                if !assigned[candidate]
                    && !bodies[candidate].is_empty()
                    && touching(&bodies[current], &bodies[candidate], rules)
                {
                    assigned[candidate] = true;
                    cluster.push(candidate);
                }
            }
        }
        clusters.push(cluster);
    }
    clusters
}

/// Clusters for settling: a body overlapping any other is illegal where it stands, so
/// it forms its own cluster (and must move) even if it also shares a seam with something.
/// The `anchor` (a body being dragged) is exempt: whatever it overlaps moves instead.
fn settle_clusters(bodies: &[Body], anchor: Option<usize>, rules: &Rules) -> Vec<Vec<usize>> {
    let overlapping: Vec<usize> = (0..bodies.len())
        .filter(|&index| {
            Some(index) != anchor
                && bodies
                    .iter()
                    .enumerate()
                    .any(|(other, body)| other != index && bodies[index].overlaps(body))
        })
        .collect();
    let clear: Vec<usize> = (0..bodies.len())
        .filter(|index| !overlapping.contains(index))
        .collect();
    let mut clusters = clusters(bodies, &clear, rules);
    clusters.extend(
        overlapping
            .into_iter()
            .filter(|&index| !bodies[index].is_empty())
            .map(|index| vec![index]),
    );
    clusters
}

/// The cluster with the most display area among `members`; earliest wins ties, so the
/// caller's first body (this machine) anchors the arrangement.
fn largest_cluster(bodies: &[Body], members: &[usize], rules: &Rules) -> Vec<usize> {
    clusters(bodies, members, rules)
        .into_iter()
        .max_by_key(|cluster| {
            let area: i64 = cluster.iter().map(|&index| bodies[index].area()).sum();
            (area, std::cmp::Reverse(cluster[0]))
        })
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// Translation for every body, index-aligned with the input.
    pub deltas: Vec<Vec2I>,
    /// Side of the dragged body now resting against a neighbour.
    pub side: EdgeSide,
}

/// Placing the dragged body and re-settling the rest repeat until neither moves
/// anything; this bounds the repetitions.
const SETTLE_ROUNDS: usize = 4;

/// One pointer step of a drag. Rests body `dragged` at the nearest legal position to
/// `desired` (a translation from where it stands now), then settles every cluster no
/// longer connected to it: a cluster slides into the gap the dragged body left
/// (`vacated`: its footprint when the drag began) when that reconnects it; otherwise it
/// rides along, keeping its place beside the dragged body; and when that is blocked it
/// reattaches with the smallest move. The largest cluster of other bodies is the frame:
/// it alone blocks the dragged body (so riders can be pushed ahead of it) and it never
/// moves. Later rounds only restore contact with the frame's cluster after neighbours
/// moved; they never chase the pointer again. A step whose net effect would move the
/// dragged body against the pointer is discarded, since it could only be undone by the
/// next step. None when there is nothing to rest against.
pub fn drag_step(
    bodies: &[Body],
    dragged: usize,
    desired: Vec2I,
    vacated: Bounds,
    current: Option<EdgeSide>,
    rules: &Rules,
) -> Option<Step> {
    let mut placed = bodies.to_vec();
    let mut deltas = vec![Vec2I::default(); bodies.len()];
    let others: Vec<usize> = (0..bodies.len())
        .filter(|&index| index != dragged)
        .collect();
    let frame = largest_cluster(&placed, &others, rules);
    let mut side = current;

    for round in 0..SETTLE_ROUNDS {
        let placement = {
            let anchored = components(&placed, rules)
                .into_iter()
                .find(|cluster| cluster.iter().any(|member| frame.contains(member)))
                .unwrap_or_default();
            let reachable: Vec<usize> = if round == 0 || anchored.contains(&dragged) {
                others.clone()
            } else {
                anchored
                    .into_iter()
                    .filter(|&member| member != dragged)
                    .collect()
            };
            let targets: Vec<&Body> = reachable.iter().map(|&index| &placed[index]).collect();
            let blocking: Vec<&Body> = frame.iter().map(|&index| &placed[index]).collect();
            let goal = if round == 0 {
                desired
            } else {
                Vec2I::default()
            };
            attach(&placed[dragged], goal, &targets, &blocking, rules, side)?
        };
        side = Some(placement.side);
        let step = placement.delta;
        shift(&mut placed, &mut deltas, dragged, step);
        let moved = settle(
            &mut placed,
            &mut deltas,
            dragged,
            &frame,
            vacated,
            step,
            rules,
        );
        if step == Vec2I::default() && !moved {
            break;
        }
    }
    let net = deltas[dragged];
    if i64::from(net.x) * i64::from(desired.x) + i64::from(net.y) * i64::from(desired.y) < 0 {
        return Some(Step {
            deltas: vec![Vec2I::default(); bodies.len()],
            side: current.or(side)?,
        });
    }
    Some(Step {
        deltas,
        side: side?,
    })
}

/// A cluster closes the gap when its displays, after the move, fill most of the vacated
/// footprint (three quarters of whichever is smaller) rather than grazing or merely
/// surrounding it.
fn closes_gap(landed: &Body, vacated: Bounds) -> bool {
    let covered: i64 = landed
        .rects
        .iter()
        .filter_map(|rect| rect.intersection(vacated))
        .map(Bounds::area)
        .sum();
    covered * 4 >= landed.area().min(vacated.area()) * 3
}

fn shift(placed: &mut [Body], deltas: &mut [Vec2I], index: usize, delta: Vec2I) {
    placed[index] = placed[index].shifted(delta);
    deltas[index].x += delta.x;
    deltas[index].y += delta.y;
}

/// Reattach every cluster not connected to the frame, the dragged body aside (a later
/// round re-rests it): gap closers first (nearest first), then riders. Riders only ride
/// while the dragged body is anchored to the frame; while it merely rests on riders they
/// wait for it to be re-rested, so a card stuck at a corner cannot ratchet its riders
/// away. True when anything moved.
fn settle(
    placed: &mut [Body],
    deltas: &mut [Vec2I],
    dragged: usize,
    frame: &[usize],
    vacated: Bounds,
    step: Vec2I,
    rules: &Rules,
) -> bool {
    let mut settled: Vec<usize> = Vec::new();
    let mut pending: Vec<Vec<usize>> = Vec::new();
    for cluster in settle_clusters(placed, Some(dragged), rules) {
        if cluster.iter().any(|member| frame.contains(member)) {
            settled.extend(cluster);
        } else {
            let cluster: Vec<usize> = cluster
                .into_iter()
                .filter(|&member| member != dragged)
                .collect();
            if !cluster.is_empty() {
                pending.push(cluster);
            }
        }
    }
    if settled.is_empty() {
        settled.push(dragged);
    }
    let mut moved = false;

    let stationary: Vec<usize> = settled
        .iter()
        .copied()
        .filter(|&member| member != dragged)
        .collect();
    loop {
        let mut nearest: Option<(usize, f64, Vec2I)> = None;
        for (index, cluster) in pending.iter().enumerate() {
            let merged = Body::merged(cluster.iter().map(|&member| &placed[member]));
            let Some(placement) = reattach(&merged, cluster, &stationary, placed, rules) else {
                continue;
            };
            let closes = closes_gap(&merged.shifted(placement.delta), vacated);
            let cost = f64::from(placement.delta.x).hypot(f64::from(placement.delta.y));
            if closes && nearest.is_none_or(|(_, best, _)| cost < best) {
                nearest = Some((index, cost, placement.delta));
            }
        }
        let Some((index, _, delta)) = nearest else {
            break;
        };
        for member in pending.swap_remove(index) {
            shift(placed, deltas, member, delta);
            settled.push(member);
        }
        moved = true;
    }

    let anchored = settled.contains(&dragged);
    for cluster in pending {
        let merged = Body::merged(cluster.iter().map(|&member| &placed[member]));
        let clear = |body: &Body| {
            (0..placed.len())
                .filter(|member| !cluster.contains(member))
                .all(|member| !body.overlaps(&placed[member]))
        };
        let rigid = merged.shifted(step);
        let delta = if anchored && clear(&rigid) && touching(&rigid, &placed[dragged], rules) {
            step
        } else if !anchored && clear(&merged) {
            Vec2I::default()
        } else {
            match reattach(&merged, &cluster, &settled, placed, rules) {
                Some(placement) => placement.delta,
                None => continue,
            }
        };
        for member in cluster {
            shift(placed, deltas, member, delta);
            settled.push(member);
        }
        moved |= delta != Vec2I::default();
    }
    moved
}

/// Smallest move resting `merged` (the bodies in `cluster`) against `settled`, blocked
/// by every body outside the cluster.
fn reattach(
    merged: &Body,
    cluster: &[usize],
    settled: &[usize],
    placed: &[Body],
    rules: &Rules,
) -> Option<Placement> {
    let targets: Vec<&Body> = settled.iter().map(|&member| &placed[member]).collect();
    let blocking: Vec<&Body> = (0..placed.len())
        .filter(|member| !cluster.contains(member))
        .map(|member| &placed[member])
        .collect();
    attach(merged, Vec2I::default(), &targets, &blocking, rules, None)
}

/// Translation per body that turns any arrangement into one connected, overlap-free
/// cluster: the largest cluster stays put and every other cluster joins it, nearest
/// first, each with its smallest legal move.
pub fn normalize(bodies: &[Body], rules: &Rules) -> Vec<Vec2I> {
    let mut placed = bodies.to_vec();
    let mut deltas = vec![Vec2I::default(); bodies.len()];
    let mut pending = settle_clusters(&placed, None, rules);
    let Some(largest) = pending.iter().enumerate().max_by_key(|(_, cluster)| {
        let area: i64 = cluster.iter().map(|&index| bodies[index].area()).sum();
        (area, std::cmp::Reverse(cluster[0]))
    }) else {
        return deltas;
    };
    let mut settled = pending.swap_remove(largest.0);

    while !pending.is_empty() {
        let mut nearest: Option<(usize, f64, Vec2I)> = None;
        for (index, cluster) in pending.iter().enumerate() {
            let merged = Body::merged(cluster.iter().map(|&member| &placed[member]));
            let Some(placement) = reattach(&merged, cluster, &settled, &placed, rules) else {
                continue;
            };
            let cost = f64::from(placement.delta.x).hypot(f64::from(placement.delta.y));
            if nearest.is_none_or(|(_, best, _)| cost < best) {
                nearest = Some((index, cost, placement.delta));
            }
        }
        let Some((index, _, delta)) = nearest else {
            break;
        };
        for member in pending.swap_remove(index) {
            shift(&mut placed, &mut deltas, member, delta);
            settled.push(member);
        }
    }
    deltas
}

/// Every shared boundary segment of at least `min_len` between two bodies.
pub fn seams(bodies: &[Body], min_len: i64) -> Vec<SeamSegment> {
    let mut segments = Vec::new();
    for (a, body) in bodies.iter().enumerate() {
        for (b, other) in bodies.iter().enumerate().skip(a + 1) {
            for rect in &body.rects {
                for other_rect in &other.rects {
                    let Some((side, at, span)) = shared_edge(*rect, *other_rect) else {
                        continue;
                    };
                    if span.len() < min_len {
                        continue;
                    }
                    let (from, to) = match side {
                        EdgeSide::Left | EdgeSide::Right => ((at, span.start), (at, span.end)),
                        EdgeSide::Top | EdgeSide::Bottom => ((span.start, at), (span.end, at)),
                    };
                    segments.push(SeamSegment { a, b, from, to });
                }
            }
        }
    }
    segments
}

fn to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: Rules = Rules {
        min_seam: 20,
        align_tolerance: 0,
    };

    fn display(x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
        DisplayRect {
            id: format!("{x},{y}"),
            x,
            y,
            w,
            h,
            scale: 1.0,
        }
    }

    fn square(x: i32, y: i32) -> Body {
        Body::new(&[display(0, 0, 100, 100)], Vec2I { x, y })
    }

    fn at(x: i32, y: i32) -> Vec2I {
        Vec2I { x, y }
    }

    fn place(
        moving: &Body,
        desired: Vec2I,
        fixed: &[Body],
        current: Option<EdgeSide>,
    ) -> Placement {
        resolve(moving, desired, fixed, &RULES, current).expect("attachable")
    }

    #[test]
    fn slides_along_the_seam_and_stops_at_the_dead_corner() {
        let moving = square(100, 0);
        let fixed = [square(0, 0)];

        assert_eq!(place(&moving, at(0, 30), &fixed, None).delta, at(0, 30));
        assert_eq!(place(&moving, at(0, 95), &fixed, None).delta, at(0, 80));
        assert_eq!(place(&moving, at(0, -95), &fixed, None).delta, at(0, -80));
        assert_eq!(place(&moving, at(-30, 30), &fixed, None).delta, at(0, 30));
    }

    #[test]
    fn flips_to_the_top_once_the_pointer_is_past_the_corner() {
        let moving = square(100, 0);
        let fixed = [square(0, 0)];
        let resting = Some(EdgeSide::Left);

        let placement = place(&moving, at(0, -110), &fixed, resting);
        assert_eq!(placement.delta, at(-20, -100));
        assert_eq!(placement.side, EdgeSide::Bottom);

        let placement = place(&moving, at(0, -95), &fixed, resting);
        assert_eq!(placement.delta, at(0, -80));
        assert_eq!(placement.side, EdgeSide::Left);

        let placement = place(&moving, at(-120, 0), &fixed, resting);
        assert_eq!(placement.delta, at(-200, 0));
        assert_eq!(placement.side, EdgeSide::Right);
    }

    #[test]
    fn the_corner_itself_is_never_a_valid_position() {
        let moving = square(100, 0);
        let fixed = [square(0, 0)];

        let delta = place(&moving, at(0, -100), &fixed, None).delta;
        assert!(delta == at(0, -80) || delta == at(-20, -100), "{delta:?}");
    }

    #[test]
    fn slides_across_the_top_of_two_neighbours() {
        let moving = square(0, -100);
        let fixed = [square(0, 0), square(100, 0)];

        assert_eq!(place(&moving, at(-50, 0), &fixed, None).delta, at(-50, 0));
        assert_eq!(place(&moving, at(50, 0), &fixed, None).delta, at(50, 0));
        assert_eq!(place(&moving, at(150, 0), &fixed, None).delta, at(150, 0));
        assert_eq!(place(&moving, at(190, 0), &fixed, None).delta, at(180, 0));
        assert_eq!(place(&moving, at(250, 0), &fixed, None).delta, at(200, 20));
    }

    #[test]
    fn never_overlaps_and_fills_a_gap_it_fits_into() {
        let moving = square(100, 0);
        let fixed = [square(0, 0), square(200, 0)];

        assert_eq!(place(&moving, at(50, 0), &fixed, None).delta, at(0, 0));
        assert_eq!(place(&moving, at(150, 0), &fixed, None).delta, at(200, 0));
        assert_eq!(place(&moving, at(0, 50), &fixed, None).delta, at(0, 50));
        assert_eq!(place(&moving, at(0, 90), &fixed, None).delta, at(0, 80));
    }

    #[test]
    fn flush_edges_and_centres_attract_within_tolerance() {
        let rules = Rules {
            min_seam: 20,
            align_tolerance: 5,
        };
        let moving = square(100, 0);
        let fixed = [Body::new(&[display(0, 0, 100, 200)], at(0, 0))];

        let snap = |dy: i32| {
            resolve(&moving, at(0, dy), &fixed, &rules, None)
                .unwrap()
                .delta
        };
        assert_eq!(snap(4), at(0, 0));
        assert_eq!(snap(7), at(0, 7));
        assert_eq!(snap(53), at(0, 50));
        assert_eq!(snap(97), at(0, 100));
    }

    #[test]
    fn alignment_is_sticky_once_flush() {
        let rules = Rules {
            min_seam: 20,
            align_tolerance: 30,
        };
        let fixed = [Body::new(&[display(0, 0, 100, 200)], at(0, 0))];
        let top = square(100, 0);
        let centred = square(100, 50);

        assert_eq!(
            resolve(&top, at(0, 28), &fixed, &rules, None)
                .unwrap()
                .delta,
            at(0, 0)
        );
        assert_eq!(
            resolve(&top, at(0, 32), &fixed, &rules, None)
                .unwrap()
                .delta,
            at(0, 50)
        );
        assert_eq!(
            resolve(&centred, at(0, -22), &fixed, &rules, None)
                .unwrap()
                .delta,
            at(0, 0)
        );
        assert_eq!(
            resolve(&centred, at(0, -32), &fixed, &rules, None)
                .unwrap()
                .delta,
            at(0, -50)
        );
    }

    #[test]
    fn a_step_never_moves_the_dragged_card_against_the_pointer() {
        let rules = Rules {
            min_seam: MIN_SEAM,
            align_tolerance: 60,
        };
        let bodies = [
            Body::new(&[display(0, 0, 1920, 1080)], at(0, -64)),
            Body::new(&[display(0, 0, 3440, 1440)], at(1920, 856)),
            Body::new(&[display(0, 0, 1080, 1920)], at(5360, 67)),
        ];
        let vacated = Bounds {
            left: 1920,
            top: 10,
            right: 5360,
            bottom: 1450,
        };
        let mut placed = bodies.to_vec();
        let mut side = Some(EdgeSide::Left);
        let mut goal = at(1759, 361);
        for _ in 0..12 {
            let step = drag_step(&placed, 1, goal, vacated, side, &rules).unwrap();
            let net = step.deltas[1];
            let along = i64::from(net.x) * i64::from(goal.x) + i64::from(net.y) * i64::from(goal.y);
            assert!(along >= 0, "{net:?} against {goal:?}");
            placed = applied(&placed, &step.deltas);
            assert!(legal(&placed, &rules));
            side = Some(step.side);
            goal = at(goal.x + 160 - net.x, goal.y + 110 - net.y);
        }
    }

    #[test]
    fn multi_display_bodies_keep_every_display_clear() {
        let moving = square(0, 0);
        let fixed = [Body::new(
            &[display(0, 50, 100, 50), display(100, 0, 100, 150)],
            at(0, 0),
        )];

        assert_eq!(place(&moving, at(0, -20), &fixed, None).delta, at(0, -50));
    }

    #[test]
    fn nothing_to_attach_to_yields_none() {
        assert!(resolve(&square(0, 0), at(5, 5), &[], &RULES, None).is_none());
        assert!(resolve(&Body::default(), at(5, 5), &[square(0, 0)], &RULES, None).is_none());
    }

    fn step(bodies: &[Body], dragged: usize, desired: Vec2I, current: Option<EdgeSide>) -> Step {
        let vacated = bodies[dragged].bounds().expect("dragged body has displays");
        drag_step(bodies, dragged, desired, vacated, current, &RULES).expect("attachable")
    }

    fn applied(bodies: &[Body], deltas: &[Vec2I]) -> Vec<Body> {
        bodies
            .iter()
            .zip(deltas)
            .map(|(body, delta)| body.shifted(*delta))
            .collect()
    }

    fn legal(bodies: &[Body], rules: &Rules) -> bool {
        components(bodies, rules).len() == 1
            && bodies
                .iter()
                .enumerate()
                .all(|(i, a)| bodies.iter().skip(i + 1).all(|b| !a.overlaps(b)))
    }

    #[test]
    fn lifting_the_middle_card_out_of_a_row_closes_the_row() {
        let bodies = [square(0, 0), square(100, 0), square(200, 0)];

        let step = step(&bodies, 1, at(-5, -110), Some(EdgeSide::Left));

        assert_eq!(step.deltas, vec![at(0, 0), at(-20, -100), at(-100, 0)]);
        assert_eq!(step.side, EdgeSide::Bottom);
        assert!(legal(&applied(&bodies, &step.deltas), &RULES));
    }

    #[test]
    fn sliding_the_middle_card_moves_nothing_else() {
        let bodies = [square(0, 0), square(100, 0), square(200, 0)];

        let step = step(&bodies, 1, at(0, -30), Some(EdgeSide::Left));

        assert_eq!(step.deltas, vec![at(0, 0), at(0, -30), at(0, 0)]);

        let real = Rules {
            min_seam: MIN_SEAM,
            align_tolerance: 60,
        };
        let row = [
            Body::new(&[display(0, 0, 1512, 982)], at(0, 0)),
            Body::new(&[display(0, 0, 1920, 1080)], at(1512, 0)),
            Body::new(&[display(0, 0, 2560, 1440)], at(3432, 0)),
        ];
        let vacated = row[1].bounds().unwrap();
        for dy in [-200, -400, -700] {
            let step = drag_step(&row, 1, at(0, dy), vacated, Some(EdgeSide::Left), &real).unwrap();
            assert_eq!(step.deltas, vec![at(0, 0), at(0, dy), at(0, 0)], "dy {dy}");
        }
    }

    #[test]
    fn pushing_through_a_neighbour_reorders_the_row() {
        let bodies = [square(0, 0), square(100, 0), square(200, 0)];

        let step = step(&bodies, 1, at(160, 0), Some(EdgeSide::Left));

        assert_eq!(step.deltas, vec![at(0, 0), at(100, 0), at(-100, 0)]);
        assert!(legal(&applied(&bodies, &step.deltas), &RULES));
    }

    #[test]
    fn a_straight_push_goes_through_a_larger_neighbour_not_over_it() {
        let rules = Rules {
            min_seam: MIN_SEAM,
            align_tolerance: 60,
        };
        let big = Body::new(&[display(0, 0, 2560, 1440)], at(0, 0));
        let small = Body::new(&[display(0, 0, 1920, 1080)], at(2560, 0));
        let mut sides = Vec::new();
        for dx in (1..=40).map(|i| -80 * i) {
            let placement = resolve(
                &small,
                at(dx, 0),
                std::slice::from_ref(&big),
                &rules,
                Some(EdgeSide::Left),
            )
            .unwrap();
            sides.push(placement.side);
            assert_eq!(
                placement.delta.y, 0,
                "dx {dx} detoured to {:?}",
                placement.delta
            );
        }
        assert_eq!(sides.last(), Some(&EdgeSide::Right));
    }

    #[test]
    fn a_card_hanging_off_the_dragged_one_rides_along_and_can_be_pushed() {
        let bodies = [square(0, 0), square(100, 0), square(100, 100)];

        let up = step(&bodies, 1, at(0, -30), Some(EdgeSide::Left));
        assert_eq!(up.deltas, vec![at(0, 0), at(0, -30), at(0, -30)]);
        assert!(legal(&applied(&bodies, &up.deltas), &RULES));

        let down = step(&bodies, 1, at(0, 30), Some(EdgeSide::Left));
        assert_eq!(down.deltas, vec![at(0, 0), at(0, 30), at(0, 30)]);
        assert!(legal(&applied(&bodies, &down.deltas), &RULES));
    }

    #[test]
    fn a_rider_that_would_collide_reattaches_with_the_smallest_move() {
        let bodies = [square(0, 0), square(100, 0), square(100, 100)];

        let step = step(&bodies, 1, at(-5, -110), Some(EdgeSide::Left));

        assert_eq!(step.deltas[1], at(-20, -100));
        assert!(
            step.deltas[2] == at(0, -20) || step.deltas[2] == at(-20, 0),
            "{:?}",
            step.deltas
        );
        assert!(legal(&applied(&bodies, &step.deltas), &RULES));
    }

    #[test]
    fn a_step_is_a_fixed_point() {
        let bodies = [
            Body::new(&[display(0, 0, 100, 300)], at(0, 0)),
            square(100, 0),
            square(200, 0),
            square(200, 100),
        ];

        let first = step(&bodies, 1, at(160, 0), Some(EdgeSide::Left));
        let moved = applied(&bodies, &first.deltas);
        assert!(legal(&moved, &RULES));
        let again = drag_step(
            &moved,
            1,
            at(0, 0),
            bodies[1].bounds().unwrap(),
            Some(first.side),
            &RULES,
        )
        .unwrap();
        assert!(
            again.deltas.iter().all(|delta| *delta == at(0, 0)),
            "{:?}",
            again.deltas
        );
    }

    #[test]
    fn the_frame_never_moves_when_a_card_is_pushed_through_riders() {
        let rules = Rules {
            min_seam: MIN_SEAM,
            align_tolerance: 60,
        };
        let bodies = [
            Body::new(&[display(0, 0, 3840, 2160)], at(0, 0)),
            Body::new(&[display(0, 0, 1512, 982)], at(3840, 98)),
            Body::new(
                &[display(0, 0, 1920, 1080), display(1920, 0, 1920, 1080)],
                at(5352, 0),
            ),
            Body::new(&[display(0, 0, 1512, 982)], at(9192, 0)),
        ];
        let vacated = bodies[1].bounds().unwrap();

        let step = drag_step(
            &bodies,
            1,
            at(1160, 34),
            vacated,
            Some(EdgeSide::Right),
            &rules,
        )
        .unwrap();

        assert_eq!(step.deltas[0], at(0, 0));
        assert!(
            legal(&applied(&bodies, &step.deltas), &rules),
            "{:?}",
            step.deltas
        );
        let moved = applied(&bodies, &step.deltas);
        let again = drag_step(&moved, 1, at(0, 0), vacated, Some(step.side), &rules).unwrap();
        assert!(
            again.deltas.iter().all(|delta| *delta == at(0, 0)),
            "{:?}",
            again.deltas
        );
    }

    #[test]
    fn a_push_through_a_row_never_spends_the_residual_on_another_seam() {
        let rules = Rules {
            min_seam: MIN_SEAM,
            align_tolerance: 60,
        };
        let row = [
            Body::new(&[display(0, 0, 1512, 982)], at(0, 0)),
            Body::new(&[display(0, 0, 1920, 1080)], at(1512, 0)),
            Body::new(&[display(0, 0, 2560, 1440)], at(3432, 0)),
        ];
        let vacated = row[1].bounds().unwrap();

        let step = drag_step(&row, 1, at(-1750, 0), vacated, Some(EdgeSide::Left), &rules).unwrap();

        assert_eq!(step.deltas[1].y, 0, "{:?}", step.deltas);
        assert!(
            legal(&applied(&row, &step.deltas), &rules),
            "{:?}",
            step.deltas
        );
    }

    #[test]
    fn resting_side_reports_the_seam_a_body_sits_on() {
        let others = [square(0, 0)];
        assert_eq!(
            resting_side(&square(100, 30), &others, &RULES),
            Some(EdgeSide::Left)
        );
        assert_eq!(
            resting_side(&square(-20, -100), &others, &RULES),
            Some(EdgeSide::Bottom)
        );
        assert_eq!(resting_side(&square(100, 90), &others, &RULES), None);
        assert_eq!(resting_side(&square(300, 0), &others, &RULES), None);
    }

    #[test]
    fn a_pair_has_nothing_to_settle() {
        let bodies = [square(0, 0), square(100, 0)];

        let step = step(&bodies, 1, at(-120, 0), Some(EdgeSide::Left));

        assert_eq!(step.deltas, vec![at(0, 0), at(-200, 0)]);
        assert!(drag_step(
            &bodies[1..],
            0,
            at(1, 1),
            bodies[1].bounds().unwrap(),
            None,
            &RULES
        )
        .is_none());
    }

    #[test]
    fn normalize_repairs_gaps_overlaps_and_leaves_legal_arrangements_alone() {
        let row = [square(0, 0), square(100, 0), square(200, 0)];
        assert!(normalize(&row, &RULES)
            .iter()
            .all(|delta| *delta == at(0, 0)));

        let scattered = [
            square(0, 0),
            square(400, 300),
            square(50, 50),
            Body::default(),
        ];
        let deltas = normalize(&scattered, &RULES);
        assert_eq!(deltas[0], at(0, 0));
        assert_eq!(deltas[3], at(0, 0));
        assert!(legal(&applied(&scattered[..3], &deltas[..3]), &RULES));

        let tangled = [square(0, 0), square(100, 0), square(100, 50)];
        let deltas = normalize(&tangled, &RULES);
        assert_eq!(deltas[0], at(0, 0));
        assert!(legal(&applied(&tangled, &deltas), &RULES));
    }

    #[test]
    fn a_pushed_rider_still_attached_elsewhere_is_moved_out_of_the_way() {
        let bodies = [
            Body::new(&[display(0, 0, 100, 200)], at(0, -100)),
            square(100, 0),
            square(100, 100),
            square(200, 50),
        ];

        let step = step(&bodies, 1, at(0, 40), Some(EdgeSide::Left));

        assert_eq!(&step.deltas[..3], &[at(0, 0), at(0, 40), at(0, 40)]);
        assert!(
            legal(&applied(&bodies, &step.deltas), &RULES),
            "{:?}",
            step.deltas
        );
    }

    #[test]
    fn components_ignore_bodies_without_displays() {
        let bodies = [
            square(0, 0),
            Body::default(),
            square(100, 0),
            square(300, 0),
        ];

        assert_eq!(components(&bodies, &RULES), vec![vec![0, 2], vec![3]]);
    }

    #[test]
    fn seams_report_shared_segments_above_the_minimum() {
        let bodies = [square(0, 0), square(100, 30), square(0, 100)];

        assert_eq!(
            seams(&bodies, 20),
            vec![
                SeamSegment {
                    a: 0,
                    b: 1,
                    from: (100, 30),
                    to: (100, 100),
                },
                SeamSegment {
                    a: 0,
                    b: 2,
                    from: (0, 100),
                    to: (100, 100),
                },
                SeamSegment {
                    a: 1,
                    b: 2,
                    from: (100, 100),
                    to: (100, 130),
                },
            ]
        );
        assert_eq!(seams(&bodies, 80).len(), 1);
    }
}
