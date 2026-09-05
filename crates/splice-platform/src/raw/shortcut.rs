#[derive(Clone, Copy)]
pub enum Stream {
    Desktop,
    Hid,
}

#[derive(Default)]
pub struct SwitchShortcut {
    held: [bool; 2],
    suppressed: [[bool; 3]; 2],
    last_switch: Option<u64>,
}

impl SwitchShortcut {
    pub fn press(&mut self, stream: Stream, now_ms: u64) -> bool {
        let already_held = self.held.iter().any(|held| *held);
        let already_suppressed = self.suppressed(stream, 88);
        self.held[stream as usize] = true;
        if already_suppressed
            || already_held
            || self
                .last_switch
                .is_some_and(|last| now_ms.saturating_sub(last) < 300)
        {
            return false;
        }
        self.last_switch = Some(now_ms);
        self.suppressed = [[true; 3]; 2];
        true
    }

    pub fn suppressed(&self, stream: Stream, code: u16) -> bool {
        Self::index(code).is_some_and(|index| self.suppressed[stream as usize][index])
    }

    pub fn release(&mut self, stream: Stream, code: u16) {
        if let Some(index) = Self::index(code) {
            self.suppressed[stream as usize][index] = false;
        }
        if code == 88 {
            self.held[stream as usize] = false;
        }
    }

    fn index(code: u16) -> Option<usize> {
        match code {
            29 => Some(0),
            56 => Some(1),
            88 => Some(2),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_callback_can_win_without_leaking_the_chord_or_switching_twice() {
        for (first, second) in [
            (Stream::Desktop, Stream::Hid),
            (Stream::Hid, Stream::Desktop),
        ] {
            let mut shortcut = SwitchShortcut::default();
            assert!(shortcut.press(first, 0));
            for stream in [first, second] {
                for code in [29, 56, 88] {
                    assert!(shortcut.suppressed(stream, code));
                }
            }
            assert!(!shortcut.press(second, 1000));
            shortcut.release(first, 88);
            assert!(!shortcut.press(second, 2000));
            shortcut.release(second, 88);
            for code in [29, 56] {
                shortcut.release(first, code);
                assert!(shortcut.suppressed(second, code));
                shortcut.release(second, code);
                assert!(!shortcut.suppressed(second, code));
            }
            assert!(shortcut.press(second, 3000));
        }
    }

    #[test]
    fn release_before_the_other_callback_does_not_make_it_a_second_switch() {
        for (first, delayed) in [
            (Stream::Desktop, Stream::Hid),
            (Stream::Hid, Stream::Desktop),
        ] {
            let mut shortcut = SwitchShortcut::default();
            assert!(shortcut.press(first, 0));
            for code in [29, 56, 88] {
                shortcut.release(first, code);
            }
            assert!(!shortcut.press(delayed, 1000));
            assert!(shortcut.suppressed(delayed, 88));
            for code in [29, 56, 88] {
                shortcut.release(delayed, code);
            }
            assert!(shortcut.press(first, 2000));
        }
    }

    #[test]
    fn desktop_shortcuts_work_without_a_hid_monitor() {
        let mut shortcut = SwitchShortcut::default();
        assert!(shortcut.press(Stream::Desktop, 0));
        for code in [29, 56, 88] {
            shortcut.release(Stream::Desktop, code);
        }
        assert!(shortcut.press(Stream::Desktop, 1000));
    }
}
