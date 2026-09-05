use crate::raw::shortcut::Stream;
use splice_proto::{InputEvent, raw::RawEvent};
use std::collections::VecDeque;

const DEADLINE_MS: u64 = 500;
const CAPACITY: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Key(u16, bool),
    Button(bool),
    Motion,
}

impl Kind {
    pub fn raw(event: &RawEvent) -> Option<Self> {
        match *event {
            RawEvent::Key { code, pressed } => Some(Self::Key(code, pressed)),
            RawEvent::Button { pressed, .. } => Some(Self::Button(pressed)),
            RawEvent::Motion { x, y } if x != 0 || y != 0 => Some(Self::Motion),
            _ => None,
        }
    }

    pub fn desktop(event: &InputEvent) -> Option<Self> {
        match *event {
            InputEvent::Key { code, pressed } => Some(Self::Key(code as u16, pressed)),
            InputEvent::Button { pressed, .. } => Some(Self::Button(pressed)),
            InputEvent::Motion { dx, dy } if dx != 0.0 || dy != 0.0 => Some(Self::Motion),
            _ => None,
        }
    }

    fn class(self) -> usize {
        match self {
            Self::Key(..) => 0,
            Self::Button(..) => 1,
            Self::Motion => 2,
        }
    }
}

#[derive(Default)]
pub struct Witnesses {
    raw: VecDeque<(Kind, u64)>,
    desktop: VecDeque<(Kind, u64)>,
    missing: [u8; 3],
    overflow: bool,
}

impl Witnesses {
    pub fn observe(&mut self, stream: Stream, kind: Kind, now: u64) {
        while self
            .raw
            .front()
            .is_some_and(|(_, at)| now.saturating_sub(*at) > DEADLINE_MS)
        {
            self.raw.pop_front();
        }
        let (own, other) = match stream {
            Stream::Desktop => (&mut self.desktop, &mut self.raw),
            Stream::Hid => (&mut self.raw, &mut self.desktop),
        };
        if let Some(index) = other.iter().position(|(event, _)| *event == kind) {
            other.remove(index);
            self.missing[kind.class()] = 0;
        } else {
            if own.len() == CAPACITY {
                match stream {
                    Stream::Desktop => self.overflow = true,
                    Stream::Hid => {
                        own.pop_front();
                    }
                }
            }
            if own.len() < CAPACITY {
                own.push_back((kind, now));
            }
        }
    }

    pub fn failure(&mut self, now: u64) -> Option<String> {
        if self.overflow {
            return Some("Wayland input exceeded the raw stream verification queue".into());
        }
        while let Some((kind, at)) = self.desktop.front().copied() {
            if now.saturating_sub(at) <= DEADLINE_MS {
                break;
            }
            self.desktop.pop_front();
            let missing = &mut self.missing[kind.class()];
            *missing = missing.saturating_add(1);
            if matches!(kind, Kind::Key(..) | Kind::Button(..)) || *missing >= 3 {
                return Some("Wayland received input absent from the raw device stream. Release exclusive grabs in keyboard/mouse remappers and use a relative mouse; touchpads and tablets are not raw sources.".into());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn witnesses_match_either_order_and_are_consumed() {
        for first in [Stream::Hid, Stream::Desktop] {
            let second = match first {
                Stream::Hid => Stream::Desktop,
                Stream::Desktop => Stream::Hid,
            };
            let mut witnesses = Witnesses::default();
            witnesses.observe(first, Kind::Key(42, true), 0);
            witnesses.observe(second, Kind::Key(42, true), 450);
            assert!(witnesses.failure(1000).is_none());
            witnesses.observe(Stream::Desktop, Kind::Key(42, true), 1100);
            assert!(witnesses.failure(1700).is_some());
        }
    }

    #[test]
    fn pointer_coalescing_and_idle_are_allowed_but_intercepted_input_fails() {
        let mut witnesses = Witnesses::default();
        for now in 0..100 {
            witnesses.observe(Stream::Hid, Kind::Motion, now);
        }
        witnesses.observe(Stream::Desktop, Kind::Motion, 100);
        assert!(witnesses.failure(1000).is_none());
        for now in 1000..1003 {
            witnesses.observe(Stream::Desktop, Kind::Motion, now);
        }
        witnesses.observe(Stream::Hid, Kind::Key(42, true), 1200);
        assert!(witnesses.failure(1600).is_some());
    }

    #[test]
    fn key_codes_and_releases_cannot_mask_each_other() {
        let mut witnesses = Witnesses::default();
        witnesses.observe(Stream::Desktop, Kind::Key(42, false), 0);
        witnesses.observe(Stream::Hid, Kind::Key(42, true), 100);
        witnesses.observe(Stream::Hid, Kind::Key(54, false), 100);
        assert!(witnesses.failure(501).is_some());
        assert!(Kind::desktop(&InputEvent::ScrollStop { cancel: false }).is_none());
    }
}
