//! Held-input ledger: tracks keys/buttons believed down, so any anomaly can release
//! everything. Kept on BOTH sides (source: what we forwarded; target: what we injected).

use splice_proto::{InputEvent, PointerButton};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default)]
pub struct HeldLedger {
    keys: BTreeSet<u32>,
    buttons: BTreeSet<PointerButtonKey>,
}

/// PointerButton isn't Ord (Other(u8)); wrap for the set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PointerButtonKey(u8);

fn button_key(b: PointerButton) -> PointerButtonKey {
    PointerButtonKey(match b {
        PointerButton::Left => 0,
        PointerButton::Right => 1,
        PointerButton::Middle => 2,
        PointerButton::Back => 3,
        PointerButton::Forward => 4,
        PointerButton::Other(n) => 8u8.saturating_add(n),
    })
}

fn key_button(k: PointerButtonKey) -> PointerButton {
    match k.0 {
        0 => PointerButton::Left,
        1 => PointerButton::Right,
        2 => PointerButton::Middle,
        3 => PointerButton::Back,
        4 => PointerButton::Forward,
        n => PointerButton::Other(n - 8),
    }
}

impl HeldLedger {
    /// Observe an event passing through; track press/release state.
    pub fn observe(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key { code, pressed: true } => {
                self.keys.insert(code);
            }
            InputEvent::Key { code, pressed: false } => {
                self.keys.remove(&code);
            }
            InputEvent::Button { button, pressed: true } => {
                self.buttons.insert(button_key(button));
            }
            InputEvent::Button { button, pressed: false } => {
                self.buttons.remove(&button_key(button));
            }
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Drain into the release events that undo everything held (keys then buttons).
    pub fn drain_releases(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(self.keys.len() + self.buttons.len());
        for code in std::mem::take(&mut self.keys) {
            out.push(InputEvent::Key { code, pressed: false });
        }
        for b in std::mem::take(&mut self.buttons) {
            out.push(InputEvent::Button { button: key_button(b), pressed: false });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_and_drains() {
        let mut l = HeldLedger::default();
        l.observe(&InputEvent::Key { code: 30, pressed: true });
        l.observe(&InputEvent::Key { code: 42, pressed: true });
        l.observe(&InputEvent::Key { code: 30, pressed: false });
        l.observe(&InputEvent::Button { button: PointerButton::Left, pressed: true });
        assert!(!l.is_empty());
        let rel = l.drain_releases();
        assert_eq!(rel.len(), 2);
        assert!(rel.contains(&InputEvent::Key { code: 42, pressed: false }));
        assert!(rel.contains(&InputEvent::Button { button: PointerButton::Left, pressed: false }));
        assert!(l.is_empty());
    }
}
