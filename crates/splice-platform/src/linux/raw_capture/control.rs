use crate::raw::shortcut::{Stream, SwitchShortcut};
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub struct Control {
    pub active: AtomicBool,
    pub raw_hold: AtomicBool,
    pub switching: AtomicBool,
    shortcut: Mutex<SwitchShortcut>,
    desktop_keys: Mutex<BTreeSet<u16>>,
    desktop_buttons: Mutex<Vec<splice_proto::PointerButton>>,
    pub buttons_changed: tokio::sync::Notify,
    witnesses: Mutex<Option<super::witness::Witnesses>>,
}

impl Control {
    pub fn activate(&self) {
        self.switching.store(false, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);
    }

    pub fn release(&self) {
        self.active.store(false, Ordering::SeqCst);
        self.raw_hold.store(false, Ordering::SeqCst);
        self.switching.store(false, Ordering::SeqCst);
        self.desktop_keys.lock().clear();
        self.desktop_buttons.lock().clear();
        *self.shortcut.lock() = Default::default();
        self.raw_end();
        self.buttons_changed.notify_one();
    }

    pub fn key(
        &self,
        stream: Stream,
        code: u16,
        pressed: bool,
        chord: bool,
        now_ms: u64,
    ) -> (bool, bool) {
        let mut shortcut = self.shortcut.lock();
        let switched = self.active.load(Ordering::SeqCst)
            && code == 88
            && pressed
            && chord
            && shortcut.press(stream, now_ms);
        if switched {
            self.switching.store(true, Ordering::SeqCst);
            self.raw_end();
        }
        let suppressed = shortcut.suppressed(stream, code);
        if !pressed {
            shortcut.release(stream, code);
        }
        (switched, suppressed)
    }

    pub fn raw_key(
        &self,
        code: u16,
        pressed: bool,
        held: &BTreeSet<u16>,
        now_ms: u64,
    ) -> (bool, bool) {
        if !pressed && held.contains(&code) {
            return (false, self.suppressed(code));
        }
        self.key(
            Stream::Hid,
            code,
            pressed,
            held.contains(&29) && held.contains(&56),
            now_ms,
        )
    }

    pub fn desktop_event(&self, event: &splice_proto::InputEvent, now_ms: u64) -> (bool, bool) {
        if let splice_proto::InputEvent::Button { button, pressed } = *event {
            let mut buttons = self.desktop_buttons.lock();
            let was_pressed = buttons.contains(&button);
            if pressed {
                if !was_pressed {
                    buttons.push(button);
                }
            } else {
                buttons.retain(|held| *held != button);
            }
            if pressed != was_pressed {
                self.buttons_changed.notify_one();
            }
        }
        let mut repeated = false;
        let result = if let splice_proto::InputEvent::Key { code, pressed } = *event {
            let mut keys = self.desktop_keys.lock();
            if pressed {
                repeated = !keys.insert(code as u16);
            } else {
                keys.remove(&(code as u16));
            }
            self.key(
                Stream::Desktop,
                code as u16,
                pressed,
                keys.contains(&29) && keys.contains(&56),
                now_ms,
            )
        } else {
            (false, false)
        };
        if !result.1 && !repeated {
            if let Some(kind) = super::witness::Kind::desktop(event) {
                self.observe(Stream::Desktop, kind, now_ms);
            }
        }
        result
    }

    pub fn desktop_snapshot(
        &self,
        keys: BTreeSet<u32>,
        physical_button_held: bool,
    ) -> Option<Vec<splice_proto::InputEvent>> {
        let buttons = self.desktop_buttons.lock();
        if physical_button_held == buttons.is_empty() {
            return None;
        }
        *self.desktop_keys.lock() = keys.iter().map(|code| *code as u16).collect();
        let shortcut = self.shortcut.lock();
        let mut events = crate::keymap::held_key_presses(
            keys.into_iter()
                .filter(|code| !shortcut.suppressed(Stream::Desktop, *code as u16)),
        );
        events.extend(
            buttons
                .iter()
                .map(|button| splice_proto::InputEvent::Button {
                    button: *button,
                    pressed: true,
                }),
        );
        Some(events)
    }

    pub fn raw_begin(&self) {
        *self.witnesses.lock() = Some(Default::default());
    }

    pub fn raw_end(&self) {
        *self.witnesses.lock() = None;
    }

    pub fn observe(&self, stream: Stream, kind: super::witness::Kind, now_ms: u64) {
        if let Some(witnesses) = &mut *self.witnesses.lock() {
            witnesses.observe(stream, kind, now_ms);
        }
    }

    pub fn failure(&self, now_ms: u64) -> Option<String> {
        self.witnesses
            .lock()
            .as_mut()
            .and_then(|w| w.failure(now_ms))
    }

    pub fn suppressed(&self, code: u16) -> bool {
        self.shortcut.lock().suppressed(Stream::Hid, code)
    }
}
