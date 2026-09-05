use anyhow::{Result, bail, ensure};
use evdev::{EventType, InputEvent, RelativeAxisCode as Rel, SynchronizationCode as Syn};
use splice_proto::raw::{MAX_EVENTS, RawEvent, keyboard_code};
use std::collections::BTreeSet;

pub struct Reports {
    pub held: BTreeSet<u16>,
    pending: Vec<RawEvent>,
    high_resolution: [bool; 2],
}

pub fn key(code: u16, pressed: bool) -> Result<RawEvent> {
    match code {
        0x110..=0x117 => Ok(RawEvent::Button {
            number: (code - 0x110 + 1) as u8,
            pressed,
        }),
        code if keyboard_code(code) => Ok(RawEvent::Key { code, pressed }),
        _ => bail!("unsupported raw input key/button {code}"),
    }
}

impl Reports {
    pub fn new(held: BTreeSet<u16>, high_resolution: [bool; 2]) -> Self {
        Self {
            held,
            pending: Vec::new(),
            high_resolution,
        }
    }

    pub fn snapshot(&self) -> Result<Vec<RawEvent>> {
        ensure!(
            self.pending.is_empty(),
            "raw device report is incomplete at handoff"
        );
        self.held.iter().map(|code| key(*code, true)).collect()
    }

    pub fn push(&mut self, event: InputEvent) -> Result<Option<Vec<RawEvent>>> {
        match event.event_type() {
            EventType::SYNCHRONIZATION => match Syn(event.code()) {
                Syn::SYN_REPORT => {
                    let pending = std::mem::take(&mut self.pending);
                    for event in &pending {
                        let (code, pressed) = match *event {
                            RawEvent::Key { code, pressed } => (code, pressed),
                            RawEvent::Button { number, pressed } => {
                                (0x110 + u16::from(number) - 1, pressed)
                            }
                            _ => continue,
                        };
                        if pressed {
                            self.held.insert(code);
                        } else {
                            self.held.remove(&code);
                        }
                    }
                    return Ok(Some(pending));
                }
                Syn::SYN_DROPPED => {
                    bail!("kernel raw input queue overflowed; lost counts cannot be recovered")
                }
                _ => bail!("unsupported raw input synchronization event"),
            },
            EventType::KEY => match event.value() {
                0 | 1 => self.pending.push(key(event.code(), event.value() == 1)?),
                2 => {}
                _ => bail!("invalid raw key transition"),
            },
            EventType::RELATIVE => {
                let value = event.value();
                let raw = match Rel(event.code()) {
                    Rel::REL_X => RawEvent::Motion { x: value, y: 0 },
                    Rel::REL_Y => RawEvent::Motion { x: 0, y: value },
                    Rel::REL_HWHEEL_HI_RES => RawEvent::Wheel {
                        x120: value,
                        y120: 0,
                    },
                    Rel::REL_WHEEL_HI_RES => RawEvent::Wheel {
                        x120: 0,
                        y120: value,
                    },
                    Rel::REL_HWHEEL if self.high_resolution[0] => return Ok(None),
                    Rel::REL_WHEEL if self.high_resolution[1] => return Ok(None),
                    Rel::REL_HWHEEL => RawEvent::Wheel {
                        x120: value
                            .checked_mul(120)
                            .ok_or_else(|| anyhow::anyhow!("raw horizontal wheel overflow"))?,
                        y120: 0,
                    },
                    Rel::REL_WHEEL => RawEvent::Wheel {
                        x120: 0,
                        y120: value
                            .checked_mul(120)
                            .ok_or_else(|| anyhow::anyhow!("raw vertical wheel overflow"))?,
                    },
                    _ => bail!("unsupported relative input axis {}", event.code()),
                };
                self.pending.push(raw);
            }
            EventType::MISC | EventType::LED | EventType::REPEAT => {}
            _ => bail!("unsupported raw input event type {}", event.event_type().0),
        }
        ensure!(
            self.pending.len() <= MAX_EVENTS,
            "raw device report exceeds event limit"
        );
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ev(kind: EventType, code: u16, value: i32) -> InputEvent {
        InputEvent::new(kind.0, code, value)
    }
    fn end() -> InputEvent {
        ev(EventType::SYNCHRONIZATION, Syn::SYN_REPORT.0, 0)
    }

    #[test]
    fn physical_counts_and_order_survive_report_boundaries() {
        let mut reports = Reports::new(BTreeSet::new(), [false; 2]);
        for e in [
            ev(EventType::RELATIVE, 0, -32768),
            ev(EventType::KEY, 42, 1),
            ev(EventType::RELATIVE, 1, 32767),
            ev(EventType::KEY, 0x117, 1),
        ] {
            assert!(reports.push(e).unwrap().is_none());
        }
        assert_eq!(
            reports.push(end()).unwrap().unwrap(),
            vec![
                RawEvent::Motion { x: -32768, y: 0 },
                RawEvent::Key {
                    code: 42,
                    pressed: true
                },
                RawEvent::Motion { x: 0, y: 32767 },
                RawEvent::Button {
                    number: 8,
                    pressed: true
                }
            ]
        );
        assert_eq!(reports.snapshot().unwrap().len(), 2);
        reports.push(ev(EventType::KEY, 42, 2)).unwrap();
        assert!(reports.push(end()).unwrap().unwrap().is_empty());
    }

    #[test]
    fn high_resolution_wheels_do_not_also_forward_emulated_detents() {
        let mut reports = Reports::new(BTreeSet::new(), [true; 2]);
        for e in [
            ev(EventType::RELATIVE, Rel::REL_WHEEL.0, 1),
            ev(EventType::RELATIVE, Rel::REL_WHEEL_HI_RES.0, 15),
            ev(EventType::RELATIVE, Rel::REL_HWHEEL.0, -1),
            ev(EventType::RELATIVE, Rel::REL_HWHEEL_HI_RES.0, -30),
        ] {
            reports.push(e).unwrap();
        }
        assert_eq!(
            reports.push(end()).unwrap().unwrap(),
            [
                RawEvent::Wheel { x120: 0, y120: 15 },
                RawEvent::Wheel { x120: -30, y120: 0 }
            ]
        );
        let mut low = Reports::new(BTreeSet::new(), [false; 2]);
        low.push(ev(EventType::RELATIVE, Rel::REL_WHEEL.0, -2))
            .unwrap();
        assert_eq!(
            low.push(end()).unwrap().unwrap(),
            [RawEvent::Wheel {
                x120: 0,
                y120: -240
            }]
        );
    }

    #[test]
    fn lost_events_fail_instead_of_inventing_motion_or_held_state() {
        let mut reports = Reports::new(BTreeSet::new(), [false; 2]);
        reports.push(ev(EventType::KEY, 42, 1)).unwrap();
        assert!(
            reports
                .push(ev(EventType::SYNCHRONIZATION, Syn::SYN_DROPPED.0, 0))
                .is_err()
        );
        assert!(reports.snapshot().is_err());
        assert!(
            Reports::new(BTreeSet::new(), [false; 2])
                .push(ev(EventType::RELATIVE, Rel::REL_WHEEL.0, i32::MAX))
                .is_err()
        );
    }
}
