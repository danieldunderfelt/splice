//! Keycode translation between evdev (wire format) and macOS virtual keycodes.
//!
//! The wire always carries raw evdev codes (`linux/input-event-codes.h`). Linux backends
//! pass them through untouched. macOS translates via the `keycode` crate
//! (Chromium-derived tables).

/// evdev keycodes we reference by name (subset; values from linux/input-event-codes.h).
pub mod ev {
    pub const KEY_ESC: u32 = 1;
    pub const KEY_LEFTCTRL: u32 = 29;
    pub const KEY_LEFTSHIFT: u32 = 42;
    pub const KEY_RIGHTSHIFT: u32 = 54;
    pub const KEY_LEFTALT: u32 = 56;
    pub const KEY_CAPSLOCK: u32 = 58;
    pub const KEY_RIGHTCTRL: u32 = 97;
    pub const KEY_RIGHTALT: u32 = 100;
    pub const KEY_LEFTMETA: u32 = 125;
    pub const KEY_RIGHTMETA: u32 = 126;
    pub const KEY_UP: u32 = 103;
    pub const KEY_LEFT: u32 = 105;
    pub const KEY_RIGHT: u32 = 106;
    pub const KEY_DOWN: u32 = 108;
    pub const KEY_DELETE: u32 = 111;
}

/// Is this evdev code a modifier key?
pub fn is_modifier(code: u32) -> bool {
    matches!(
        code,
        ev::KEY_LEFTCTRL
            | ev::KEY_RIGHTCTRL
            | ev::KEY_LEFTSHIFT
            | ev::KEY_RIGHTSHIFT
            | ev::KEY_LEFTALT
            | ev::KEY_RIGHTALT
            | ev::KEY_LEFTMETA
            | ev::KEY_RIGHTMETA
            | ev::KEY_CAPSLOCK
    )
}

/// Arrow/nav keys that need NumericPad|SecondaryFn flags when injected on macOS.
pub fn is_nav_key(code: u32) -> bool {
    matches!(code, ev::KEY_UP | ev::KEY_DOWN | ev::KEY_LEFT | ev::KEY_RIGHT)
}

/// evdev → macOS virtual keycode. None if the key has no macOS equivalent.
pub fn evdev_to_mac(code: u32) -> Option<u16> {
    use keycode::{KeyMap, KeyMapping};
    let map = KeyMap::from_key_mapping(KeyMapping::Evdev(code as u16)).ok()?;
    // keycode crate uses 0xFFFF for "no mapping".
    if map.mac == 0xFFFF {
        None
    } else {
        Some(map.mac)
    }
}

/// macOS virtual keycode → evdev. None if unmapped.
pub fn mac_to_evdev(vk: u16) -> Option<u32> {
    use keycode::{KeyMap, KeyMapping};
    let map = KeyMap::from_key_mapping(KeyMapping::Mac(vk)).ok()?;
    if map.evdev == 0xFFFF {
        None
    } else {
        Some(map.evdev as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_core_keys() {
        // KEY_A=30 <-> kVK_ANSI_A=0
        assert_eq!(evdev_to_mac(30), Some(0));
        assert_eq!(mac_to_evdev(0), Some(30));
        // Modifiers map both ways.
        for code in [
            ev::KEY_LEFTCTRL,
            ev::KEY_LEFTSHIFT,
            ev::KEY_LEFTALT,
            ev::KEY_LEFTMETA,
            ev::KEY_RIGHTSHIFT,
            ev::KEY_RIGHTALT,
            ev::KEY_RIGHTMETA,
        ] {
            let mac = evdev_to_mac(code).expect("modifier maps to mac");
            assert_eq!(mac_to_evdev(mac), Some(code), "roundtrip for {code}");
        }
    }

    #[test]
    fn nav_and_modifier_classification() {
        assert!(is_modifier(ev::KEY_LEFTCTRL));
        assert!(!is_modifier(30));
        assert!(is_nav_key(ev::KEY_UP));
        assert!(!is_nav_key(30));
    }
}
