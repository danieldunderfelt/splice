//! Constrained card dragging on the arrangement canvas. A drag owns a live copy of the
//! arrangement: the grabbed machine follows the pointer along seams under the
//! `splice_core::arrange` rules, neighbours that lose their connection close ranks, and
//! every snap eases into place instead of popping.

use egui::{vec2, Vec2};
use splice_core::arrange::{self, Body, Bounds, Rules};
use splice_core::UiState;
use splice_platform::EdgeSide;
use splice_proto::{DisplayRect, MachineId, Vec2I};

/// Screen px within which flush edges and centres attract.
const ALIGN_PX: f32 = 6.0;
/// Snaps travelling further than this on screen ease over `SNAP_TAU` instead of popping.
const JUMP_PX: f32 = 12.0;
/// Time constant of the snap ease, seconds.
const SNAP_TAU: f32 = 0.05;
/// An ease is over once the remaining distance is below this many screen px.
const SETTLE_PX: f32 = 0.25;

struct Card {
    id: MachineId,
    displays: Vec<DisplayRect>,
    start: Vec2I,
    offset: Vec2I,
    /// Where the card is drawn, canvas units; trails `offset` only while easing.
    shown: Vec2,
    easing: bool,
}

impl Card {
    fn body(&self) -> Body {
        Body::new(&self.displays, self.offset)
    }

    fn target(&self) -> Vec2 {
        vec2(self.offset.x as f32, self.offset.y as f32)
    }

    fn follow(&mut self, jump: f32) {
        if self.easing {
            return;
        }
        if (self.target() - self.shown).length() > jump {
            self.easing = true;
        } else {
            self.shown = self.target();
        }
    }
}

pub struct CardDrag {
    cards: Vec<Card>,
    grabbed: usize,
    /// Pointer travel since the grab, canvas units.
    desired: Vec2,
    side: Option<EdgeSide>,
    /// The grabbed card's footprint when the drag began: the gap neighbours may close.
    vacated: Bounds,
    rules: Rules,
    jump: f32,
    settle: f32,
}

impl CardDrag {
    /// Start dragging `id` at the given canvas→screen `scale`. None when the machine has
    /// no displays to arrange.
    pub fn begin(state: &UiState, id: &MachineId, scale: f32) -> Option<Self> {
        let cards: Vec<Card> = state
            .machines
            .iter()
            .map(|machine| Card {
                id: machine.id.clone(),
                displays: machine.displays.clone(),
                start: machine.offset,
                offset: machine.offset,
                shown: vec2(machine.offset.x as f32, machine.offset.y as f32),
                easing: false,
            })
            .collect();
        let grabbed = cards.iter().position(|card| &card.id == id)?;
        let vacated = cards[grabbed].body().bounds()?;
        Some(CardDrag {
            cards,
            grabbed,
            desired: Vec2::ZERO,
            side: None,
            vacated,
            rules: Rules {
                min_seam: arrange::MIN_SEAM,
                align_tolerance: (ALIGN_PX / scale).round() as i64,
            },
            jump: JUMP_PX / scale,
            settle: SETTLE_PX / scale,
        })
    }

    pub fn id(&self) -> &MachineId {
        &self.cards[self.grabbed].id
    }

    /// The pointer moved by `delta` screen px: rest the grabbed card at the nearest legal
    /// position and settle every neighbour that step disturbed.
    pub fn drag_by(&mut self, delta: Vec2, scale: f32) {
        self.desired += delta / scale;
        let grabbed = &self.cards[self.grabbed];
        let goal = Vec2I {
            x: grabbed.start.x + self.desired.x.round() as i32 - grabbed.offset.x,
            y: grabbed.start.y + self.desired.y.round() as i32 - grabbed.offset.y,
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
        for (card, delta) in self.cards.iter_mut().zip(step.deltas) {
            card.offset.x += delta.x;
            card.offset.y += delta.y;
            card.follow(self.jump);
        }
    }

    /// Advance snap eases by `dt` seconds. True while any card is still travelling.
    pub fn tick(&mut self, dt: f32) -> bool {
        let blend = 1.0 - (-dt / SNAP_TAU).exp();
        let mut travelling = false;
        for card in self.cards.iter_mut().filter(|card| card.easing) {
            let target = card.target();
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

    /// Canvas offset to draw `id` at, or None for a machine that joined mid-drag.
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

    const SCALE: f32 = 0.1;

    fn machine<'a>(state: &'a UiState, hostname: &str) -> &'a splice_core::ui_state::UiMachine {
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

    #[test]
    fn dragging_a_card_over_the_top_keeps_the_cluster_connected() {
        let state = preview::initial_state();
        let gnome = machine(&state, "fedora-gnome").id.clone();
        let kde = machine(&state, "fedora-kde").id.clone();
        let mut drag = CardDrag::begin(&state, &gnome, SCALE).expect("gnome has displays");

        drag.drag_by(vec2(0.0, -300.0), SCALE);

        assert_eq!(offset_of(&drag, &gnome), Vec2I { x: 3272, y: -1440 });
        assert_eq!(drag.side, Some(EdgeSide::Bottom));
        assert_ne!(offset_of(&drag, &kde), machine(&state, "fedora-kde").offset);
        let bodies = drag.bodies();
        assert_eq!(arrange::components(&bodies, &drag.rules).len(), 1);
        let changed: Vec<MachineId> = drag.changes().into_iter().map(|(id, _)| id).collect();
        assert_eq!(changed, vec![gnome, kde]);
    }

    #[test]
    fn small_moves_track_directly_and_snaps_ease() {
        let state = preview::initial_state();
        let gnome = machine(&state, "fedora-gnome").id.clone();
        let mut drag = CardDrag::begin(&state, &gnome, SCALE).expect("gnome has displays");

        drag.drag_by(vec2(0.0, -10.0), SCALE);
        assert_eq!(drag.shown_offset(&gnome), Some(vec2(3432.0, -100.0)));
        assert!(!drag.tick(0.016));

        drag.drag_by(vec2(0.0, -300.0), SCALE);
        assert_eq!(drag.shown_offset(&gnome), Some(vec2(3432.0, -100.0)));
        assert!(drag.tick(0.016));
        let mid = drag.shown_offset(&gnome).unwrap();
        assert!(mid.y < -100.0 && mid.y > -1440.0, "{mid:?}");
        for _ in 0..60 {
            drag.tick(0.016);
        }
        assert!(!drag.tick(0.016));
        assert_eq!(drag.shown_offset(&gnome), Some(vec2(3272.0, -1440.0)));
    }

    #[test]
    fn machines_without_displays_cannot_be_dragged() {
        let mut state = preview::initial_state();
        state.machines[1].displays.clear();
        let id = state.machines[1].id.clone();
        assert!(CardDrag::begin(&state, &id, SCALE).is_none());
    }
}
