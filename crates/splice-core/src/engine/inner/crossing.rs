use super::*;
use crate::{
    edge_policy::{Gesture, Outcome},
    input_settings::CrossingPolicy,
};

pub(super) struct Crossing {
    pub link: EdgeLink,
    pub pos: Vec2,
    pub local_edge: Option<u32>,
    pub gesture: Gesture,
    pub progress: f32,
}

impl Inner {
    pub(super) async fn contact_gesture(&mut self, link: &EdgeLink, edge: u32, along: f64) -> bool {
        if self.raw.settings.crossing == CrossingPolicy::Immediate {
            return false;
        }
        if self.self_info.os != Os::Macos {
            self.raw.error = Some("Dwell and resistance need passive edge observations, currently available on Mac sources. Select Immediate crossing on Linux.".into());
            self.reject_edge_hit(
                "passive edge observations unavailable",
                Some(self.last_local_pos),
            )
            .await;
            self.touch_ui();
            return true;
        }
        self.crossing = Some(Crossing {
            link: link.clone(),
            pos: edge_position(link, along),
            local_edge: Some(edge),
            gesture: Gesture::new(self.raw.settings.crossing, link.side, Instant::now()),
            progress: 0.0,
        });
        self.touch_ui();
        true
    }

    pub(super) async fn edge_motion(&mut self, edge: u32, along: f64, dx: f64, dy: f64) {
        if self.focus != Focus::Local || self.raw.preparing.is_some() {
            return;
        }
        if self.crossing.is_none() {
            let Some(link) = self.armed.get(edge as usize) else {
                return;
            };
            if self.raw.settings.crossing != CrossingPolicy::Immediate
                && Gesture::outward(link.side, dx, dy) > 0.0
            {
                self.on_edge_hit(edge, along).await;
            }
            return;
        }
        let Some(crossing) = &mut self.crossing else {
            return;
        };
        if crossing.local_edge != Some(edge) {
            self.crossing = None;
            self.touch_ui();
            return;
        }
        crossing.pos = edge_position(&crossing.link, along);
        let outcome = crossing.gesture.update(dx, dy, Instant::now());
        self.crossing_outcome(outcome).await;
    }

    pub(super) async fn crossing_tick(&mut self) {
        let Some(crossing) = &mut self.crossing else {
            return;
        };
        let outcome = crossing.gesture.update(0.0, 0.0, Instant::now());
        self.crossing_outcome(outcome).await;
    }

    async fn crossing_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Waiting(progress) => {
                if let Some(crossing) = &mut self.crossing {
                    crossing.progress = progress;
                }
            }
            Outcome::Cancel => {
                self.crossing = None;
            }
            Outcome::Cross => {
                let Some(crossing) = self.crossing.take() else {
                    return;
                };
                if let Some(edge) = crossing.local_edge {
                    let along = match crossing.link.side {
                        EdgeSide::Left | EdgeSide::Right => crossing.pos.y,
                        _ => crossing.pos.x,
                    };
                    self.begin_edge(edge, along, true).await;
                } else {
                    self.cross_link(crossing.link, crossing.pos).await;
                }
            }
        }
        self.touch_ui();
    }

    pub(super) async fn remote_gesture_motion(&mut self, dx: f64, dy: f64) -> bool {
        let Some(crossing) = &mut self.crossing else {
            return false;
        };
        if crossing.local_edge.is_some() {
            return false;
        }
        let next = Vec2 {
            x: self.virtual_pos.x + dx * self.active_sensitivity,
            y: self.virtual_pos.y + dy * self.active_sensitivity,
        };
        let along = match crossing.link.side {
            EdgeSide::Left | EdgeSide::Right => next.y,
            _ => next.x,
        };
        let dz = f64::from(self.cfg.corner_dead_zone)
            .min(f64::from(crossing.link.from_range.1 - crossing.link.from_range.0) / 4.0);
        if along <= f64::from(crossing.link.from_range.0) + dz
            || along >= f64::from(crossing.link.from_range.1) - dz
        {
            self.crossing = None;
            self.touch_ui();
            return false;
        }
        crossing.pos = edge_position(&crossing.link, along);
        let outcome = crossing.gesture.update(dx, dy, Instant::now());
        if outcome == Outcome::Cancel {
            self.crossing = None;
            self.touch_ui();
            return false;
        }
        if matches!(outcome, Outcome::Waiting(_)) {
            self.move_to_boundary(next);
        }
        self.crossing_outcome(outcome).await;
        true
    }

    pub(super) fn start_remote_gesture(&mut self, link: EdgeLink, pos: Vec2) -> bool {
        if self.raw.settings.crossing == CrossingPolicy::Immediate {
            return false;
        }
        self.crossing = Some(Crossing {
            gesture: Gesture::new(self.raw.settings.crossing, link.side, Instant::now()),
            link,
            pos,
            local_edge: None,
            progress: 0.0,
        });
        self.move_to_boundary(pos);
        self.touch_ui();
        true
    }

    fn move_to_boundary(&mut self, next: Vec2) {
        let Focus::Remote(target) = &self.focus else {
            return;
        };
        let pos = layout::clamp_into_displays(self.display_slice_of(target), next);
        let ev = InputEvent::Motion {
            dx: pos.x - self.virtual_pos.x,
            dy: pos.y - self.virtual_pos.y,
        };
        self.virtual_pos = pos;
        if let Some(net) = &self.net {
            net.send_to(
                target,
                Frame::Input {
                    session: self.active_session,
                    ev,
                },
            );
        }
    }
}

fn edge_position(link: &EdgeLink, along: f64) -> Vec2 {
    match link.side {
        EdgeSide::Left | EdgeSide::Right => Vec2 {
            x: f64::from(link.at),
            y: along,
        },
        _ => Vec2 {
            x: along,
            y: f64::from(link.at),
        },
    }
}
