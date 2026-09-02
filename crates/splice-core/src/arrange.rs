//! Arrangement rules for the canvas. Every machine is a body (its display rects in canvas
//! coordinates) that must touch a neighbour along a seam, never overlap another body, and
//! stay part of one connected cluster. Pure geometry; no platform or UI involvement.
//!
//! [`resolve`] finds the nearest position to a desired one where a body touches the fixed
//! bodies along a seam of at least [`Rules::min_seam`] and overlaps none of them. Sliding
//! along a seam, flipping around a corner and pushing through a neighbour all fall out of
//! "nearest valid position". [`drag_step`] builds a whole pointer step on top of it: the
//! dragged body is placed, then every cluster it stopped holding in place is settled.

use splice_platform::EdgeSide;
use splice_proto::{DisplayRect, Vec2I};

/// Shortest seam two displays may share, in canvas units. Leaves a dead corner so
/// displays never meet corner-to-corner and the cursor always has room to cross.
pub const MIN_SEAM: i64 = 160;
/// Fraction of `min_seam` by which the side a body already rests on wins ties, so a
/// card never chatters between two seams when the pointer sits on their bisector.
const STICKINESS: i64 = 8;

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
/// the side the body rests on now; it wins near-ties so corner flips are decisive.
/// None when there is nothing to attach to.
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
    let mut best: Option<(f64, Placement)> = None;
    for body in targets.iter().copied() {
        let Some(neighbour_bounds) = body.bounds() else {
            continue;
        };
        for anchor in &body.rects {
            for rect in &moving.rects {
                for side in SIDES {
                    let Some(delta) = probe.rest(*rect, *anchor, neighbour_bounds, side) else {
                        continue;
                    };
                    let mut score = ((delta.0 - probe.desired.0) as f64)
                        .hypot((delta.1 - probe.desired.1) as f64);
                    if current == Some(side) {
                        score -= stickiness;
                    }
                    if best
                        .as_ref()
                        .is_none_or(|(best_score, _)| score < *best_score)
                    {
                        best = Some((
                            score,
                            Placement {
                                delta: Vec2I {
                                    x: to_i32(delta.0),
                                    y: to_i32(delta.1),
                                },
                                side,
                            },
                        ));
                    }
                }
            }
        }
    }
    best.map(|(_, placement)| placement)
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
    fn rest(
        &self,
        rect: Bounds,
        anchor: Bounds,
        neighbour_bounds: Bounds,
        side: EdgeSide,
    ) -> Option<(i64, i64)> {
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
            for fixed_rect in self.blocking.iter().flat_map(|body| &body.rects) {
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
        if let Some(aligned) = flush
            .into_iter()
            .filter(|&position| {
                (position - along).abs() <= self.rules.align_tolerance && free(position)
            })
            .min_by_key(|&position| (position - along).abs())
        {
            along = aligned;
        }

        Some(if vertical {
            (cross_delta, along)
        } else {
            (along, cross_delta)
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

/// Do two bodies share a seam long enough under `rules`?
pub fn touching(a: &Body, b: &Body, rules: &Rules) -> bool {
    a.rects.iter().any(|rect| {
        b.rects.iter().any(|other| {
            shared_edge(*rect, *other).is_some_and(|(side, _, span)| {
                let (_, rect_along) = rect.spans(side);
                let (_, other_along) = other.spans(side);
                span.len()
                    >= rules
                        .min_seam
                        .min(rect_along.len())
                        .min(other_along.len())
                        .max(1)
            })
        })
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

/// Re-placing the dragged body can strand its riders again; re-place at most this often,
/// always settling after the last move.
const SETTLE_ROUNDS: usize = 3;

/// One pointer step of a drag. Rests body `dragged` at the nearest legal position to
/// `desired` (a translation from where it stands now), then settles everyone else. A
/// cluster the dragged body stopped holding in place slides into the gap it left
/// (`vacated`: its footprint when the drag began) when that reconnects it; otherwise it
/// rides along, keeping its place beside the dragged body; and when that is blocked it
/// reattaches to the stationary cluster with the smallest move. Only the stationary
/// cluster blocks the dragged body, so it can push riders ahead of it. None when there
/// is nothing to rest against.
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

    let placement = {
        let stationary = largest_cluster(&placed, &others, rules);
        let targets: Vec<&Body> = others.iter().map(|&index| &placed[index]).collect();
        let blocking: Vec<&Body> = stationary.iter().map(|&index| &placed[index]).collect();
        attach(
            &placed[dragged],
            desired,
            &targets,
            &blocking,
            rules,
            current,
        )?
    };
    let mut side = placement.side;
    let mut step = placement.delta;
    shift(&mut placed, &mut deltas, dragged, step);

    for round in 0..=SETTLE_ROUNDS {
        settle(&mut placed, &mut deltas, dragged, vacated, step, rules);
        if round == SETTLE_ROUNDS || components(&placed, rules).len() <= 1 {
            break;
        }
        let placement = {
            let stationary = largest_cluster(&placed, &others, rules);
            let anchors: Vec<&Body> = stationary.iter().map(|&index| &placed[index]).collect();
            let remaining = Vec2I {
                x: desired.x - deltas[dragged].x,
                y: desired.y - deltas[dragged].y,
            };
            attach(
                &placed[dragged],
                remaining,
                &anchors,
                &anchors,
                rules,
                Some(side),
            )
        };
        let Some(placement) = placement else {
            break;
        };
        side = placement.side;
        step = placement.delta;
        shift(&mut placed, &mut deltas, dragged, step);
    }
    Some(Step { deltas, side })
}

/// A cluster closes the gap when, after sliding by `delta`, it fills the vacated
/// footprint along the axis it slid on (three quarters of whichever is shorter), rather
/// than merely grazing it.
fn closes_gap(landed: Bounds, vacated: Bounds, delta: Vec2I) -> bool {
    let Some(overlap) = landed.intersection(vacated) else {
        return false;
    };
    let (covered, own, gap) = if delta.x.abs() >= delta.y.abs() {
        (overlap.width(), landed.width(), vacated.width())
    } else {
        (overlap.height(), landed.height(), vacated.height())
    };
    covered * 4 >= own.min(gap) * 3
}

fn shift(placed: &mut [Body], deltas: &mut [Vec2I], index: usize, delta: Vec2I) {
    placed[index] = placed[index].shifted(delta);
    deltas[index].x += delta.x;
    deltas[index].y += delta.y;
}

/// Settle every cluster of non-dragged bodies that is not the stationary one: gap
/// closers first (nearest first), then riders.
fn settle(
    placed: &mut [Body],
    deltas: &mut [Vec2I],
    dragged: usize,
    vacated: Bounds,
    step: Vec2I,
    rules: &Rules,
) {
    let others: Vec<usize> = (0..placed.len())
        .filter(|&index| index != dragged)
        .collect();
    let mut pending = clusters(placed, &others, rules);
    let stationary = largest_cluster(placed, &others, rules);
    let Some(main) = pending.iter().position(|cluster| *cluster == stationary) else {
        return;
    };
    let mut settled = pending.swap_remove(main);

    loop {
        let mut nearest: Option<(usize, f64, Vec2I)> = None;
        for (index, cluster) in pending.iter().enumerate() {
            let merged = Body::merged(cluster.iter().map(|&member| &placed[member]));
            let targets: Vec<&Body> = settled.iter().map(|&member| &placed[member]).collect();
            let blocking: Vec<&Body> = (0..placed.len())
                .filter(|member| !cluster.contains(member))
                .map(|member| &placed[member])
                .collect();
            let Some(placement) =
                attach(&merged, Vec2I::default(), &targets, &blocking, rules, None)
            else {
                continue;
            };
            let closes = merged
                .shifted(placement.delta)
                .bounds()
                .is_some_and(|bounds| closes_gap(bounds, vacated, placement.delta));
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
    }

    for cluster in pending {
        let merged = Body::merged(cluster.iter().map(|&member| &placed[member]));
        let rigid = merged.shifted(step);
        let clear = (0..placed.len())
            .filter(|member| !cluster.contains(member))
            .all(|member| !rigid.overlaps(&placed[member]));
        let delta = if clear && touching(&rigid, &placed[dragged], rules) {
            step
        } else {
            let targets: Vec<&Body> = settled.iter().map(|&member| &placed[member]).collect();
            let blocking: Vec<&Body> = (0..placed.len())
                .filter(|member| !cluster.contains(member))
                .map(|member| &placed[member])
                .collect();
            match attach(&merged, Vec2I::default(), &targets, &blocking, rules, None) {
                Some(placement) => placement.delta,
                None => continue,
            }
        };
        for member in cluster {
            shift(placed, deltas, member, delta);
            settled.push(member);
        }
    }
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

    fn connected(bodies: &[Body], step: &Step) -> bool {
        let placed: Vec<Body> = bodies
            .iter()
            .zip(&step.deltas)
            .map(|(body, delta)| body.shifted(*delta))
            .collect();
        components(&placed, &RULES).len() == 1
            && placed
                .iter()
                .enumerate()
                .all(|(i, a)| placed.iter().skip(i + 1).all(|b| !a.overlaps(b)))
    }

    #[test]
    fn lifting_the_middle_card_out_of_a_row_closes_the_row() {
        let bodies = [square(0, 0), square(100, 0), square(200, 0)];

        let step = step(&bodies, 1, at(-5, -110), Some(EdgeSide::Left));

        assert_eq!(step.deltas, vec![at(0, 0), at(-20, -100), at(-100, 0)]);
        assert_eq!(step.side, EdgeSide::Bottom);
        assert!(connected(&bodies, &step));
    }

    #[test]
    fn pushing_through_a_neighbour_reorders_the_row() {
        let bodies = [square(0, 0), square(100, 0), square(200, 0)];

        let step = step(&bodies, 1, at(160, 0), Some(EdgeSide::Left));

        assert_eq!(step.deltas, vec![at(0, 0), at(100, 0), at(-100, 0)]);
        assert!(connected(&bodies, &step));
    }

    #[test]
    fn a_card_hanging_off_the_dragged_one_rides_along_and_can_be_pushed() {
        let bodies = [square(0, 0), square(100, 0), square(100, 100)];

        let up = step(&bodies, 1, at(0, -30), Some(EdgeSide::Left));
        assert_eq!(up.deltas, vec![at(0, 0), at(0, -30), at(0, -30)]);
        assert!(connected(&bodies, &up));

        let down = step(&bodies, 1, at(0, 30), Some(EdgeSide::Left));
        assert_eq!(down.deltas, vec![at(0, 0), at(0, 30), at(0, 30)]);
        assert!(connected(&bodies, &down));
    }

    #[test]
    fn a_rider_that_would_collide_reattaches_with_the_smallest_move() {
        let bodies = [square(0, 0), square(100, 0), square(100, 100)];

        let step = step(&bodies, 1, at(-5, -110), Some(EdgeSide::Left));

        assert_eq!(step.deltas, vec![at(0, 0), at(-20, -100), at(0, -20)]);
        assert!(connected(&bodies, &step));
    }

    #[test]
    fn the_dragged_card_is_re_rested_when_its_partner_closes_a_gap() {
        let bodies = [
            Body::new(&[display(0, 0, 100, 300)], at(0, 0)),
            square(100, 0),
            square(200, 0),
        ];

        let step = step(&bodies, 1, at(160, 0), Some(EdgeSide::Left));

        assert_eq!(step.deltas, vec![at(0, 0), at(100, 0), at(-100, 0)]);
        assert!(connected(&bodies, &step));
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

#[cfg(test)]
mod throwaway {
    use super::*;

    const RULES: Rules = Rules { min_seam: 20, align_tolerance: 0 };

    fn display(x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
        DisplayRect { id: format!("{x},{y}"), x, y, w, h, scale: 1.0 }
    }
    fn square(x: i32, y: i32) -> Body {
        Body::new(&[display(0, 0, 100, 100)], Vec2I { x, y })
    }
    fn at(x: i32, y: i32) -> Vec2I { Vec2I { x, y } }

    #[test]
    fn tw_small_center_drag() {
        let bodies = [square(0, 0), square(100, 0), square(200, 0)];
        let vacated = bodies[1].bounds().unwrap();
        for dy in [-5, -10, -20, -30, -50, -70, -79] {
            let s = drag_step(&bodies, 1, at(0, dy), vacated, Some(EdgeSide::Left), &RULES).unwrap();
            println!("dy={dy} deltas={:?} side={:?}", s.deltas, s.side);
        }
    }

    #[test]
    fn tw_real_dims() {
        let a = Body::new(&[display(0, 0, 1512, 982)], at(0, 0));
        let b = Body::new(&[display(0, 0, 1920, 1080)], at(1512, 0));
        let c = Body::new(&[display(0, 0, 2560, 1440)], at(3432, 0));
        let rules = Rules { min_seam: MIN_SEAM, align_tolerance: 60 };
        let bodies = [a, b, c];
        let vacated = bodies[1].bounds().unwrap();
        for dy in [-10, -50, -100, -200, -400] {
            let s = drag_step(&bodies, 1, at(0, dy), vacated, Some(EdgeSide::Left), &rules).unwrap();
            println!("dy={dy} deltas={:?} side={:?}", s.deltas, s.side);
        }
    }
}

#[cfg(test)]
mod throwaway_overflow {
    use super::*;
    use splice_proto::{DisplayRect, Vec2I};

    fn d(x: i32, y: i32, w: u32, h: u32) -> DisplayRect {
        DisplayRect { id: "t".into(), x, y, w, h, scale: 1.0 }
    }

    #[test]
    fn huge_display_area() {
        let big = Body::new(&[d(0, 0, u32::MAX, u32::MAX)], Vec2I { x: 0, y: 0 });
        let small = Body::new(&[d(0, 0, 100, 100)], Vec2I { x: -100, y: 0 });
        let bodies = vec![small, big];
        let vacated = bodies[0].bounds().unwrap();
        let step = drag_step(&bodies, 0, Vec2I { x: 5, y: 5 }, vacated, None, &Rules::default());
        println!("step = {step:?}");
    }

    #[test]
    fn realistic_big_wall() {
        let mut rects = Vec::new();
        for i in 0..16 {
            rects.push(d(i * 7680, 0, 7680, 4320));
        }
        let big = Body::new(&rects, Vec2I { x: 0, y: 0 });
        println!("area = {}", big.area());
    }

    #[test]
    fn tmp_probe_small_center_drag() {
        let bodies = [square(0, 0), square(100, 0), square(200, 0)];
        let s = step(&bodies, 1, at(0, -30), Some(EdgeSide::Left));
        println!("SCENARIO1 deltas={:?} side={:?}", s.deltas, s.side);
        println!("SCENARIO1 connected={}", connected(&bodies, &s));

        for dy in [-5, -10, -20, -30, -50, -79, -80, -100] {
            let s = step(&bodies, 1, at(0, dy), Some(EdgeSide::Left));
            println!("dy={dy} deltas={:?} side={:?}", s.deltas, s.side);
        }

        let rules = Rules { min_seam: 160, align_tolerance: 60 };
        let big = [
            Body::new(&[display(0, 0, 1512, 982)], at(0, 0)),
            Body::new(&[display(0, 0, 1920, 1080)], at(1512, 0)),
            Body::new(&[display(0, 0, 2560, 1440)], at(3432, 0)),
        ];
        for dy in [-10, -50, -100, -200, -400] {
            let vac = big[1].bounds().unwrap();
            let s = drag_step(&big, 1, at(0, dy), vac, Some(EdgeSide::Left), &rules).unwrap();
            println!("BIG dy={dy} deltas={:?} side={:?}", s.deltas, s.side);
        }
    }
}
