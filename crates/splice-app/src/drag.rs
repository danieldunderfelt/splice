//! Constrained card dragging on the arrangement canvas. A drag owns a live copy of the
//! arrangement: the grabbed machine follows the pointer along seams under the
//! `splice_core::arrange` rules, neighbours it disturbs settle in the same step, every
//! snap eases into place instead of popping, and after release the result stays on
//! screen until the engine publishes it.

use egui::Vec2;
use splice_core::arrange::{self, Body, Bounds, Rules};
use splice_core::UiState;
use splice_platform::EdgeSide;
use splice_proto::{DisplayRect, MachineId, Vec2I};

/// Screen px within which flush edges and centres attract.
const ALIGN_PX: f32 = 6.0;
/// Motion the pointer did not ask for beyond this many screen px is a snap, and eases.
const JUMP_PX: f32 = 12.0;
/// Time constant of the snap ease, seconds.
const SNAP_TAU: f32 = 0.05;
/// An ease is over once the remaining distance is below this many screen px.
const SETTLE_PX: f32 = 0.25;
/// A released arrangement stops waiting for the engine's acknowledgement after this.
const ACK_TIMEOUT: f32 = 5.0;

struct Card {
    id: MachineId,
    displays: Vec<DisplayRect>,
    /// Offset the engine last published for this machine.
    origin: Vec2I,
    /// Offset the current gesture started from; `changes` are measured against it.
    start: Vec2I,
    /// Offset last sent to the engine (the start until a release).
    committed: Vec2I,
    offset: Vec2I,
    /// Where the card is drawn, canvas units; trails `offset` only while easing.
    shown: Vec2,
    easing: bool,
}

impl Card {
    fn new(id: &MachineId, displays: &[DisplayRect], offset: Vec2I) -> Self {
        Card {
            id: id.clone(),
            displays: displays.to_vec(),
            origin: offset,
            start: offset,
            committed: offset,
            offset,
            shown: canvas(offset),
            easing: false,
        }
    }

    fn body(&self) -> Body {
        Body::new(&self.displays, self.offset)
    }

    /// The card moved by `delta`; `expected` is the part the pointer asked for. The
    /// drawn position follows that part directly; anything beyond it is a snap that
    /// eases in, on top of any ease still in flight.
    fn follow(&mut self, delta: Vec2I, expected: Vec2, jump: f32) {
        self.offset.x += delta.x;
        self.offset.y += delta.y;
        let step = canvas(delta);
        if delta != Vec2I::default() && (step - expected).length() > jump {
            self.shown += Vec2::new(within(expected.x, step.x), within(expected.y, step.y));
            self.easing = true;
        } else if self.easing {
            self.shown += step;
        } else {
            self.shown = canvas(self.offset);
        }
    }
}

fn canvas(offset: Vec2I) -> Vec2 {
    Vec2::new(offset.x as f32, offset.y as f32)
}

/// `value` limited to the stretch between zero and `bound`.
fn within(value: f32, bound: f32) -> f32 {
    value.clamp(bound.min(0.0), bound.max(0.0))
}

pub struct CardDrag {
    cards: Vec<Card>,
    grabbed: usize,
    /// Pointer travel since the grab, canvas units.
    travel: Vec2,
    side: Option<EdgeSide>,
    /// The grabbed card's footprint when the drag began: the gap neighbours may close.
    vacated: Bounds,
    rules: Rules,
    jump: f32,
    settle: f32,
    /// Seconds since the pointer was released, while waiting for the engine.
    released: Option<f32>,
}

impl CardDrag {
    /// Start dragging `id` at the given canvas→screen `scale`. None when the machine
    /// has no displays or nothing to rest against.
    pub fn begin(state: &UiState, id: &MachineId, scale: f32) -> Option<Self> {
        let cards: Vec<Card> = state
            .machines
            .iter()
            .map(|machine| Card::new(&machine.id, &machine.displays, machine.offset))
            .collect();
        let mut drag = CardDrag {
            cards,
            grabbed: 0,
            travel: Vec2::ZERO,
            side: None,
            vacated: Bounds {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rules: Rules {
                min_seam: arrange::MIN_SEAM,
                align_tolerance: (ALIGN_PX / scale).round() as i64,
            },
            jump: JUMP_PX / scale,
            settle: SETTLE_PX / scale,
            released: None,
        };
        drag.grab(id).then_some(drag)
    }

    /// Make `id` the grabbed card, resting where it stands. False when it has no
    /// displays or there is nothing else to rest against.
    fn grab(&mut self, id: &MachineId) -> bool {
        let Some(grabbed) = self.cards.iter().position(|card| &card.id == id) else {
            return false;
        };
        let body = self.cards[grabbed].body();
        let Some(vacated) = body.bounds() else {
            return false;
        };
        let others: Vec<Body> = self
            .cards
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != grabbed)
            .map(|(_, card)| card.body())
            .collect();
        if others.iter().all(Body::is_empty) {
            return false;
        }
        self.grabbed = grabbed;
        self.vacated = vacated;
        self.side = arrange::resting_side(&body, &others, &self.rules);
        self.travel = Vec2::ZERO;
        true
    }

    pub fn id(&self) -> &MachineId {
        &self.cards[self.grabbed].id
    }

    /// Follow `state` when the arrangement changed under the drag. Machines that joined
    /// are added and ones that left are dropped, keeping every card's live position and
    /// the pointer travel so far; displays or offsets changed by someone else rebuild
    /// the whole drag from the published arrangement. False when the grabbed machine is
    /// gone.
    pub fn rebase(&mut self, state: &UiState) -> bool {
        let known = |machine: &splice_core::ui_state::UiMachine| {
            self.cards.iter().find(|card| card.id == machine.id)
        };
        let unchanged = state.machines.iter().all(|machine| {
            known(machine).is_none_or(|card| {
                machine.displays == card.displays
                    && (machine.offset == card.origin || machine.offset == card.committed)
            })
        });
        let same_members = state.machines.len() == self.cards.len()
            && state
                .machines
                .iter()
                .zip(&self.cards)
                .all(|(machine, card)| machine.id == card.id);
        if unchanged && same_members {
            for (machine, card) in state.machines.iter().zip(&mut self.cards) {
                card.origin = machine.offset;
            }
            return true;
        }
        let id = self.id().clone();
        let mut cards: Vec<Card> = Vec::with_capacity(state.machines.len());
        for machine in &state.machines {
            match self.cards.iter().position(|card| card.id == machine.id) {
                Some(index) if unchanged => cards.push(self.cards.swap_remove(index)),
                _ => cards.push(Card::new(&machine.id, &machine.displays, machine.offset)),
            }
        }
        self.cards = cards;
        let (released, travel, vacated) = (self.released, self.travel, self.vacated);
        if !self.grab(&id) {
            return false;
        }
        self.released = released;
        if unchanged {
            self.travel = travel;
            self.vacated = vacated;
        }
        true
    }

    /// The pointer moved by `delta` screen px at the current `scale`: rest the grabbed
    /// card at the nearest legal position and settle every neighbour that step
    /// disturbed.
    pub fn drag_by(&mut self, delta: Vec2, scale: f32) {
        if delta == Vec2::ZERO {
            return;
        }
        let moved = delta / scale;
        self.travel += moved;
        let grabbed = &self.cards[self.grabbed];
        let goal = canvas(grabbed.start) + self.travel - canvas(grabbed.offset);
        let goal = Vec2I {
            x: goal.x.round() as i32,
            y: goal.y.round() as i32,
        };
        let bodies: Vec<Body> = self.cards.iter().map(Card::body).collect();
        let Some(step) = arrange::drag_step(
            &bodies,
            self.grabbed,
            goal,
            self.vacated,
            self.side,
            &self.rules,
        ) else {
            return;
        };
        self.side = Some(step.side);
        let carried = canvas(step.deltas[self.grabbed]);
        for (index, (card, delta)) in self.cards.iter_mut().zip(step.deltas).enumerate() {
            let expected = if index == self.grabbed {
                moved
            } else {
                carried
            };
            card.follow(delta, expected, self.jump);
        }
        let grabbed = &self.cards[self.grabbed];
        let residual = canvas(grabbed.start) + self.travel - canvas(grabbed.offset);
        let band = 2.0
            * (self.vacated.right - self.vacated.left).max(self.vacated.bottom - self.vacated.top)
                as f32;
        self.travel -= residual - residual.clamp(Vec2::splat(-band), Vec2::splat(band));
    }

    /// Advance snap eases and the post-release wait by `dt` seconds. True while any
    /// card is still travelling.
    pub fn tick(&mut self, dt: f32) -> bool {
        if let Some(elapsed) = &mut self.released {
            *elapsed += dt;
        }
        let blend = 1.0 - (-dt / SNAP_TAU).exp();
        let mut travelling = false;
        for card in self.cards.iter_mut().filter(|card| card.easing) {
            let target = canvas(card.offset);
            card.shown += (target - card.shown) * blend;
            if (target - card.shown).length() < self.settle {
                card.shown = target;
                card.easing = false;
            } else {
                travelling = true;
            }
        }
        travelling
    }

    pub fn easing(&self) -> bool {
        self.cards.iter().any(|card| card.easing)
    }

    /// The pointer was released: the arrangement stays on screen until `done`.
    pub fn release(&mut self) {
        for card in &mut self.cards {
            card.committed = card.offset;
        }
        self.released = Some(0.0);
    }

    /// Grab `id` while the previous drop is still awaiting the engine: the new drag
    /// starts from the arrangement on screen, which the engine is about to echo.
    pub fn regrab(&mut self, id: &MachineId) -> bool {
        for card in &mut self.cards {
            card.start = card.committed;
            card.shown = canvas(card.offset);
            card.easing = false;
        }
        if !self.grab(id) {
            return false;
        }
        self.released = None;
        true
    }

    pub fn released(&self) -> bool {
        self.released.is_some()
    }

    /// After release, once every card has settled and `state` carries what this drag
    /// committed (or the engine answered with something else), or the engine has had
    /// long enough that it will not.
    pub fn done(&self, state: &UiState) -> bool {
        let Some(elapsed) = self.released else {
            return false;
        };
        elapsed > ACK_TIMEOUT
            || (!self.easing()
                && self.cards.iter().all(|card| {
                    state.machines.iter().any(|machine| {
                        machine.id == card.id
                            && (machine.offset == card.committed || machine.offset != card.origin)
                    })
                }))
    }

    /// Canvas offset to draw `id` at, or None for a machine unknown to the drag.
    pub fn shown_offset(&self, id: &MachineId) -> Option<Vec2> {
        self.cards
            .iter()
            .find(|card| &card.id == id)
            .map(|card| card.shown)
    }

    /// The arrangement as it stands, for seam preview.
    pub fn bodies(&self) -> Vec<Body> {
        self.cards.iter().map(Card::body).collect()
    }

    /// Offsets that differ from where the drag started, to commit on drop.
    pub fn changes(&self) -> Vec<(MachineId, Vec2I)> {
        self.cards
            .iter()
            .filter(|card| card.offset != card.start)
            .map(|card| (card.id.clone(), card.offset))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::preview;
    use egui::vec2;
    use splice_core::ui_state::UiMachine;

    const SCALE: f32 = 0.1;

    fn machine<'a>(state: &'a UiState, hostname: &str) -> &'a UiMachine {
        state
            .machines
            .iter()
            .find(|machine| machine.hostname == hostname)
            .expect("preview machine")
    }

    fn offset_of(drag: &CardDrag, id: &MachineId) -> Vec2I {
        drag.cards
            .iter()
            .find(|card| &card.id == id)
            .unwrap()
            .offset
    }

    fn grab(state: &UiState, hostname: &str) -> (CardDrag, MachineId) {
        let machine = machine(state, hostname);
        let drag = CardDrag::begin(state, &machine.id, SCALE).expect("draggable");
        (drag, machine.id.clone())
    }

    fn legal(drag: &CardDrag) -> bool {
        arrange::components(&drag.bodies(), &drag.rules).len() == 1
    }

    #[test]
    fn dragging_a_card_over_the_top_keeps_the_cluster_connected() {
        let state = preview::initial_state();
        let (mut drag, gnome) = grab(&state, "fedora-gnome");
        let kde = machine(&state, "fedora-kde").id.clone();
        assert_eq!(drag.side, Some(EdgeSide::Left));

        drag.drag_by(vec2(0.0, -300.0), SCALE);

        assert_eq!(offset_of(&drag, &gnome), Vec2I { x: 3272, y: -1440 });
        assert_eq!(drag.side, Some(EdgeSide::Bottom));
        assert_ne!(offset_of(&drag, &kde), machine(&state, "fedora-kde").offset);
        assert!(legal(&drag));
        let changed: Vec<MachineId> = drag.changes().into_iter().map(|(id, _)| id).collect();
        assert_eq!(changed, vec![gnome, kde]);
    }

    #[test]
    fn pointer_driven_motion_tracks_directly_and_snaps_ease() {
        let state = preview::initial_state();
        let (mut drag, gnome) = grab(&state, "fedora-gnome");
        let kde = machine(&state, "fedora-kde").id.clone();

        drag.drag_by(vec2(0.0, -50.0), SCALE);
        assert_eq!(drag.shown_offset(&gnome), Some(vec2(3432.0, -500.0)));
        assert_eq!(drag.shown_offset(&kde), Some(vec2(3432.0, 940.0)));
        assert!(!drag.easing());

        drag.drag_by(vec2(0.0, -300.0), SCALE);
        assert_eq!(drag.shown_offset(&gnome), Some(vec2(3432.0, -1440.0)));
        assert!(drag.easing());
        assert!(drag.tick(0.016));
        let mid = drag.shown_offset(&gnome).unwrap();
        assert!(mid.x < 3432.0 && mid.x > 3272.0, "{mid:?}");

        drag.drag_by(vec2(-40.0, 0.0), SCALE);
        let slid = drag.shown_offset(&gnome).unwrap();
        assert!(
            (slid.x - (mid.x - 240.0)).abs() < 0.5,
            "{slid:?} vs {mid:?}"
        );
        for _ in 0..80 {
            drag.tick(0.016);
        }
        assert!(!drag.easing());
        assert_eq!(drag.shown_offset(&gnome), Some(vec2(3032.0, -1440.0)));
    }

    #[test]
    fn a_released_drag_waits_for_the_engine() {
        let mut state = preview::initial_state();
        let (mut drag, gnome) = grab(&state, "fedora-gnome");
        drag.drag_by(vec2(0.0, -50.0), SCALE);
        drag.release();
        assert!(drag.released() && !drag.done(&state));

        for (id, offset) in drag.changes() {
            state
                .machines
                .iter_mut()
                .find(|m| m.id == id)
                .unwrap()
                .offset = offset;
        }
        assert!(drag.done(&state));
        assert!(drag.rebase(&state));
        assert_eq!(offset_of(&drag, &gnome), Vec2I { x: 3432, y: -500 });

        let (mut drag, gnome) = grab(&preview::initial_state(), "fedora-gnome");
        drag.drag_by(vec2(0.0, -350.0), SCALE);
        drag.release();
        let mut acked = preview::initial_state();
        for (id, offset) in drag.changes() {
            acked
                .machines
                .iter_mut()
                .find(|m| m.id == id)
                .unwrap()
                .offset = offset;
        }
        assert!(drag.easing() && !drag.done(&acked));
        for _ in 0..80 {
            drag.tick(0.016);
        }
        assert!(drag.done(&acked));

        let stale = preview::initial_state();
        assert!(drag.rebase(&stale));
        assert!(drag.regrab(&gnome));
        assert!(!drag.released() && drag.rebase(&stale) && drag.rebase(&acked));
        drag.drag_by(Vec2::ZERO, SCALE);
        assert!(drag.changes().is_empty());
    }

    #[test]
    fn a_click_during_the_acknowledgement_wait_keeps_the_overlay() {
        let state = preview::initial_state();
        let (mut drag, gnome) = grab(&state, "fedora-gnome");
        drag.drag_by(vec2(0.0, -50.0), SCALE);
        drag.release();
        assert!(drag.regrab(&gnome));
        drag.release();
        assert!(drag.changes().is_empty());
        assert!(!drag.done(&state));
        assert_eq!(drag.shown_offset(&gnome), Some(vec2(3432.0, -500.0)));
    }

    #[test]
    fn a_drag_keeps_its_positions_when_a_machine_joins_and_rebuilds_when_offsets_change() {
        let mut state = preview::initial_state();
        let (mut drag, gnome) = grab(&state, "fedora-gnome");
        drag.drag_by(vec2(0.0, -50.0), SCALE);

        let mut joined = state.machines[3].clone();
        joined.id = MachineId("n500new".into());
        joined.offset = Vec2I { x: 7272, y: 0 };
        state.machines.push(joined);
        assert!(drag.rebase(&state));
        assert_eq!(drag.cards.len(), 5);
        assert_eq!(offset_of(&drag, &gnome), Vec2I { x: 3432, y: -500 });
        drag.drag_by(vec2(0.0, -20.0), SCALE);
        assert_eq!(offset_of(&drag, &gnome), Vec2I { x: 3432, y: -700 });

        state.machines[0].offset = Vec2I { x: 10, y: 10 };
        assert!(drag.rebase(&state));
        assert_eq!(offset_of(&drag, &gnome), Vec2I { x: 3432, y: 0 });
        drag.drag_by(vec2(0.0, -50.0), SCALE);
        assert!(legal(&drag));

        state.machines.retain(|machine| machine.id != gnome);
        assert!(!drag.rebase(&state));
    }

    #[test]
    fn a_still_pointer_moves_nothing_and_the_residual_is_bounded() {
        let state = preview::initial_state();
        let (mut drag, gnome) = grab(&state, "fedora-gnome");
        drag.drag_by(vec2(0.0, -300.0), SCALE);
        let before: Vec<Vec2I> = drag.cards.iter().map(|card| card.offset).collect();
        for _ in 0..10 {
            drag.drag_by(Vec2::ZERO, SCALE);
        }
        let after: Vec<Vec2I> = drag.cards.iter().map(|card| card.offset).collect();
        assert_eq!(before, after);

        drag.drag_by(vec2(0.0, -2000.0), SCALE);
        let banked = drag.travel.y - offset_of(&drag, &gnome).y as f32;
        assert!(banked.abs() <= 2.0 * 2560.0, "{banked}");
    }

    #[test]
    fn machines_without_displays_or_company_cannot_be_dragged() {
        let mut state = preview::initial_state();
        state.machines[1].displays.clear();
        let id = state.machines[1].id.clone();
        assert!(CardDrag::begin(&state, &id, SCALE).is_none());

        let mut alone = preview::initial_state();
        alone.machines.truncate(1);
        let id = alone.machines[0].id.clone();
        assert!(CardDrag::begin(&alone, &id, SCALE).is_none());
    }
}
