use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_DEVICES: usize = 64;
pub const MAX_EVENTS: usize = 768;
pub const QUEUE_REPORTS: usize = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    #[default]
    Desktop,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawEvent {
    Motion { x: i32, y: i32 },
    Wheel { x120: i32, y120: i32 },
    Key { code: u16, pressed: bool },
    Button { number: u8, pressed: bool },
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawReport {
    pub device: u64,
    pub sequence: u64,
    pub captured_us: u64,
    pub events: Vec<RawEvent>,
}

pub fn keyboard_code(code: u16) -> bool {
    (1..=0x2bf).contains(&code) && !(0x100..=0x15f).contains(&code)
}

impl RawReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.device == 0 || self.events.is_empty() || self.events.len() > MAX_EVENTS {
            return Err("invalid raw report identity or event count");
        }
        for event in &self.events {
            match *event {
                RawEvent::Key { code, .. } if !keyboard_code(code) => {
                    return Err("unsupported keyboard code")
                }
                RawEvent::Button { number, .. } if !(1..=8).contains(&number) => {
                    return Err("unsupported mouse button")
                }
                RawEvent::Removed if self.events.len() != 1 => {
                    return Err("device removal must be a separate report")
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    Key(u16),
    Button(u8),
}

impl Held {
    fn event(self, pressed: bool) -> RawEvent {
        match self {
            Self::Key(code) => RawEvent::Key { code, pressed },
            Self::Button(number) => RawEvent::Button { number, pressed },
        }
    }
}

#[derive(Default)]
pub struct RawLedger {
    devices: BTreeMap<u64, BTreeSet<Held>>,
    sequence: u64,
    captured_us: u64,
}

impl RawLedger {
    pub fn apply(&mut self, report: &RawReport) -> Result<Vec<RawEvent>, &'static str> {
        report.validate()?;
        if report.sequence != self.sequence || report.captured_us < self.captured_us {
            return Err("raw reports arrived out of order");
        }
        if !self.devices.contains_key(&report.device) && self.devices.len() == MAX_DEVICES {
            return Err("too many raw input devices");
        }
        let next_sequence = self
            .sequence
            .checked_add(1)
            .ok_or("raw sequence exhausted")?;
        if report.events != [RawEvent::Removed] {
            self.devices.entry(report.device).or_default();
        }
        let mut output = Vec::with_capacity(report.events.len());
        for event in &report.events {
            let (held, pressed) = match *event {
                RawEvent::Key { code, pressed } => (Held::Key(code), pressed),
                RawEvent::Button { number, pressed } => (Held::Button(number), pressed),
                RawEvent::Removed => {
                    if let Some(keys) = self.devices.remove(&report.device) {
                        for key in keys {
                            if !self.devices.values().any(|held| held.contains(&key)) {
                                output.push(key.event(false));
                            }
                        }
                    }
                    continue;
                }
                other => {
                    output.push(other);
                    continue;
                }
            };
            let was_held = self.devices.values().any(|keys| keys.contains(&held));
            let keys = self.devices.entry(report.device).or_default();
            if pressed {
                keys.insert(held);
            } else {
                keys.remove(&held);
            }
            let is_held = self.devices.values().any(|keys| keys.contains(&held));
            if was_held != is_held {
                output.push(held.event(is_held));
            }
        }
        self.sequence = next_sequence;
        self.captured_us = report.captured_us;
        Ok(output)
    }

    pub fn release(&mut self) -> Vec<RawEvent> {
        let keys: BTreeSet<_> = self.devices.values().flatten().copied().collect();
        *self = Self::default();
        keys.into_iter().map(|key| key.event(false)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(device: u64, sequence: u64, events: Vec<RawEvent>) -> RawReport {
        RawReport {
            device,
            sequence,
            captured_us: sequence * 1000,
            events,
        }
    }

    #[test]
    fn devices_share_keys_until_the_last_holder_releases() {
        let mut ledger = RawLedger::default();
        let down = RawEvent::Key {
            code: 42,
            pressed: true,
        };
        let up = RawEvent::Key {
            code: 42,
            pressed: false,
        };
        assert_eq!(ledger.apply(&report(1, 0, vec![down])).unwrap(), vec![down]);
        assert!(ledger.apply(&report(2, 1, vec![down])).unwrap().is_empty());
        assert!(ledger
            .apply(&report(1, 2, vec![RawEvent::Removed]))
            .unwrap()
            .is_empty());
        assert_eq!(
            ledger
                .apply(&report(2, 3, vec![RawEvent::Removed]))
                .unwrap(),
            vec![up]
        );
        assert!(ledger.release().is_empty());
    }

    #[test]
    fn ordering_is_strict_and_motion_is_exact() {
        let mut ledger = RawLedger::default();
        let motion = RawEvent::Motion {
            x: i32::MIN,
            y: i32::MAX,
        };
        let first = report(1, 0, vec![motion]);
        assert_eq!(ledger.apply(&first).unwrap(), vec![motion]);
        assert!(ledger.apply(&first).is_err());
        assert!(ledger.apply(&report(1, 2, vec![motion])).is_err());
        assert_eq!(
            ledger.apply(&report(1, 1, vec![motion])).unwrap(),
            vec![motion]
        );
    }

    #[test]
    fn keyboard_rollover_is_not_limited_to_six_keys() {
        let mut ledger = RawLedger::default();
        let keys: Vec<_> = (1..=100)
            .map(|code| RawEvent::Key {
                code,
                pressed: true,
            })
            .collect();
        assert_eq!(ledger.apply(&report(1, 0, keys.clone())).unwrap(), keys);
        assert_eq!(ledger.release().len(), 100);
        assert!(ledger.release().is_empty());
    }

    #[test]
    fn invalid_batch_does_not_partly_change_held_state() {
        let mut ledger = RawLedger::default();
        assert!(ledger
            .apply(&report(
                1,
                0,
                vec![
                    RawEvent::Key {
                        code: 30,
                        pressed: true
                    },
                    RawEvent::Key {
                        code: 0,
                        pressed: true
                    }
                ]
            ))
            .is_err());
        assert!(ledger.release().is_empty());
    }
}
