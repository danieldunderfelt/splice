use crate::input_settings::CrossingPolicy;
use splice_platform::EdgeSide;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Waiting(f32),
    Cross,
    Cancel,
}

pub struct Gesture {
    policy: CrossingPolicy,
    side: EdgeSide,
    started: Instant,
    updated: Instant,
    distance: f64,
}

impl Gesture {
    pub fn new(policy: CrossingPolicy, side: EdgeSide, now: Instant) -> Self {
        Self {
            policy,
            side,
            started: now,
            updated: now,
            distance: 0.0,
        }
    }

    pub fn outward(side: EdgeSide, dx: f64, dy: f64) -> f64 {
        match side {
            EdgeSide::Left => -dx,
            EdgeSide::Right => dx,
            EdgeSide::Top => -dy,
            EdgeSide::Bottom => dy,
        }
    }

    pub fn update(&mut self, dx: f64, dy: f64, now: Instant) -> Outcome {
        if !dx.is_finite() || !dy.is_finite() || now < self.updated {
            return Outcome::Cancel;
        }
        let outward = Self::outward(self.side, dx, dy);
        if outward < 0.0 {
            return Outcome::Cancel;
        }
        let elapsed = now.duration_since(self.updated).as_secs_f64();
        self.updated = now;
        match self.policy {
            CrossingPolicy::Immediate => Outcome::Cross,
            CrossingPolicy::Dwell { milliseconds } => {
                let progress = now.duration_since(self.started).as_secs_f64() * 1000.0
                    / f64::from(milliseconds);
                if progress >= 1.0 {
                    Outcome::Cross
                } else {
                    Outcome::Waiting(progress as f32)
                }
            }
            CrossingPolicy::Resistance {
                points,
                decay_per_second,
            } => {
                self.distance = (self.distance + outward - elapsed * decay_per_second).max(0.0);
                if self.distance >= points {
                    Outcome::Cross
                } else {
                    Outcome::Waiting((self.distance / points) as f32)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn resistance_uses_only_outward_motion_and_retreat_cancels() {
        let start = Instant::now();
        let mut gesture = Gesture::new(
            CrossingPolicy::Resistance {
                points: 100.0,
                decay_per_second: 20.0,
            },
            EdgeSide::Right,
            start,
        );
        assert_eq!(gesture.update(0.0, 5000.0, start), Outcome::Waiting(0.0));
        assert_eq!(gesture.update(50.0, 5000.0, start), Outcome::Waiting(0.5));
        assert_eq!(
            gesture.update(0.0, 0.0, start + Duration::from_secs(1)),
            Outcome::Waiting(0.3)
        );
        assert_eq!(
            gesture.update(-0.1, 0.0, start + Duration::from_secs(1)),
            Outcome::Cancel
        );
    }

    #[test]
    fn polling_rate_does_not_change_resistance() {
        for hz in [125, 500, 1000, 8000] {
            let start = Instant::now();
            let mut gesture = Gesture::new(
                CrossingPolicy::Resistance {
                    points: 200.0,
                    decay_per_second: 20.0,
                },
                EdgeSide::Bottom,
                start,
            );
            let mut result = Outcome::Waiting(0.0);
            for index in 1..=hz {
                result = gesture.update(
                    10.0 / hz as f64,
                    120.0 / hz as f64,
                    start + Duration::from_secs_f64(index as f64 / hz as f64),
                );
            }
            let Outcome::Waiting(progress) = result else {
                panic!("unexpected gesture outcome")
            };
            assert!((progress - 0.5).abs() < 0.00001, "{hz} Hz: {progress}");
        }
    }

    #[test]
    fn dwell_is_time_based_and_nonfinite_motion_cancels() {
        let start = Instant::now();
        let mut gesture = Gesture::new(
            CrossingPolicy::Dwell { milliseconds: 250 },
            EdgeSide::Top,
            start,
        );
        assert_eq!(
            gesture.update(0.0, 0.0, start + Duration::from_millis(249)),
            Outcome::Waiting(0.996)
        );
        assert_eq!(
            gesture.update(0.0, 0.0, start + Duration::from_millis(250)),
            Outcome::Cross
        );
        assert_eq!(
            gesture.update(f64::NAN, 0.0, start + Duration::from_millis(251)),
            Outcome::Cancel
        );
    }
}
