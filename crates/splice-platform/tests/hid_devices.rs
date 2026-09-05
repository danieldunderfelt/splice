use splice_platform::raw::hid::Decoder;
use splice_proto::raw::RawEvent;

#[test]
fn apple_keyboard_keeps_keys_across_its_vendor_and_media_reports() {
    let mut decoder =
        Decoder::new(include_bytes!("fixtures/hid/apple-internal-keyboard.bin")).unwrap();
    assert!(decoder.keyboard);
    assert!(decoder.required_features().is_empty());
    assert_eq!(
        decoder.decode(1, &[2, 0, 4, 0, 0, 0, 0, 0, 0]).unwrap(),
        [
            RawEvent::Key {
                code: 30,
                pressed: true
            },
            RawEvent::Key {
                code: 42,
                pressed: true
            },
        ]
    );
    assert!(decoder.decode(0x3f, &[0xff; 64]).unwrap().is_empty());
    assert_eq!(
        decoder.decode(0x52, &[1]).unwrap(),
        [RawEvent::Key {
            code: 164,
            pressed: true
        }]
    );
    assert_eq!(decoder.snapshot().len(), 3);
    assert_eq!(
        decoder.decode(1, &[0; 9]).unwrap(),
        [
            RawEvent::Key {
                code: 30,
                pressed: false
            },
            RawEvent::Key {
                code: 42,
                pressed: false
            },
        ]
    );
    assert_eq!(
        decoder.decode(0x52, &[0]).unwrap(),
        [RawEvent::Key {
            code: 164,
            pressed: false
        }]
    );
}

#[test]
fn apple_trackpad_standard_relative_report_preserves_signed_counts() {
    let mut decoder =
        Decoder::new(include_bytes!("fixtures/hid/apple-internal-trackpad.bin")).unwrap();
    assert!(decoder.mouse);
    assert!(decoder.required_features().is_empty());
    assert_eq!(
        decoder.decode(2, &[1, 0x81, 0x7f, 0, 0, 0, 0]).unwrap(),
        [
            RawEvent::Button {
                number: 1,
                pressed: true
            },
            RawEvent::Motion { x: -127, y: 127 },
        ]
    );
    assert!(decoder.decode(0x44, &[0xff; 1751]).unwrap().is_empty());
    assert_eq!(
        decoder.decode(2, &[0; 7]).unwrap(),
        [RawEvent::Button {
            number: 1,
            pressed: false
        }]
    );
}

#[test]
fn logitech_keyboard_rollover_is_an_error_that_a_valid_report_can_clear() {
    let mut decoder =
        Decoder::new(include_bytes!("fixtures/hid/logitech-c548-keyboard.bin")).unwrap();
    assert_eq!(
        decoder.decode(0, &[2, 0, 4, 0, 0, 0, 0, 0]).unwrap().len(),
        2
    );
    assert!(decoder.decode(0, &[0, 0, 1, 1, 1, 1, 1, 1]).is_err());
    assert_eq!(decoder.snapshot().len(), 2);
    assert_eq!(decoder.decode(0, &[0; 8]).unwrap().len(), 2);
    assert!(decoder.snapshot().is_empty());
}

#[test]
fn logitech_receiver_unused_buttons_do_not_block_standard_mouse_reports() {
    let mut decoder = Decoder::new(include_bytes!("fixtures/hid/logitech-c548-mouse.bin")).unwrap();
    let report = [0x81, 0, 0x01, 0x80, 0xff, 0x7f, 0xff, 1];
    assert_eq!(
        decoder.decode(2, &report).unwrap(),
        [
            RawEvent::Button {
                number: 1,
                pressed: true
            },
            RawEvent::Button {
                number: 8,
                pressed: true
            },
            RawEvent::Motion {
                x: -32767,
                y: 32767
            },
            RawEvent::Wheel {
                x120: 120,
                y120: -120
            },
        ]
    );
    let held = decoder.snapshot();
    for number in 9..=16 {
        let mut unsupported = [0u8; 8];
        unsupported[1] = 1 << (number - 9);
        assert_eq!(
            decoder.decode(2, &unsupported).unwrap_err().to_string(),
            format!("mouse button {number} is not supported")
        );
        assert_eq!(decoder.snapshot(), held);
    }
    assert_eq!(
        decoder.decode(2, &[0; 8]).unwrap(),
        [
            RawEvent::Button {
                number: 1,
                pressed: false
            },
            RawEvent::Button {
                number: 8,
                pressed: false
            }
        ]
    );
}

#[test]
fn air75_nkro_descriptor_accepts_normal_typing_and_releases_every_key() {
    let mut decoder = Decoder::new(include_bytes!("fixtures/hid/nuphy-air75-v3-ble.bin")).unwrap();
    let mut report = [0u8; 19];
    report[0] = 2;
    for usage in 4..=11 {
        report[1 + usage / 8] |= 1 << (usage % 8);
    }
    let presses = decoder.decode(6, &report).unwrap();
    assert_eq!(presses.len(), 9);
    assert!(presses.contains(&RawEvent::Key {
        code: 42,
        pressed: true
    }));
    assert!(presses.contains(&RawEvent::Key {
        code: 30,
        pressed: true
    }));
    assert_eq!(decoder.decode(6, &[0; 19]).unwrap().len(), 9);
    assert!(decoder.snapshot().is_empty());
}

#[test]
fn air75_unsupported_key_values_fail_atomically_and_allow_a_valid_retry() {
    let mut decoder = Decoder::new(include_bytes!("fixtures/hid/nuphy-air75-v3-ble.bin")).unwrap();
    let mut report = [0u8; 19];
    report[1] = 1 << 4;
    decoder.decode(6, &report).unwrap();
    let held = decoder.snapshot();
    for usage in [0x82, 0x83, 0x84, 0x86, 0x8d, 0x8e, 0x8f] {
        let mut unsupported = [0u8; 19];
        unsupported[1 + usage / 8] = 1 << (usage % 8);
        assert_eq!(
            decoder.decode(6, &unsupported).unwrap_err().to_string(),
            format!("unsupported HID key 0007:{usage:04x}")
        );
        assert_eq!(decoder.snapshot(), held);
    }
    assert_eq!(
        decoder.decode(6, &[0; 19]).unwrap(),
        [RawEvent::Key {
            code: 30,
            pressed: false
        }]
    );
}

#[test]
fn air75_caps_lock_and_boot_keyboard_reports_keep_their_physical_keys() {
    let mut decoder = Decoder::new(include_bytes!("fixtures/hid/nuphy-air75-v3-ble.bin")).unwrap();
    let mut nkro = [0u8; 19];
    nkro[1 + 0x39 / 8] = 1 << (0x39 % 8);
    assert_eq!(
        decoder.decode(6, &nkro).unwrap(),
        [RawEvent::Key {
            code: 58,
            pressed: true
        }]
    );
    assert_eq!(
        decoder.decode(6, &[0; 19]).unwrap(),
        [RawEvent::Key {
            code: 58,
            pressed: false
        }]
    );
    assert_eq!(
        decoder.decode(1, &[2, 0, 4, 0, 0, 0, 0, 0]).unwrap(),
        [
            RawEvent::Key {
                code: 30,
                pressed: true
            },
            RawEvent::Key {
                code: 42,
                pressed: true
            }
        ]
    );
    assert_eq!(decoder.decode(1, &[0; 8]).unwrap().len(), 2);
}

#[test]
fn unused_keyboard_usages_do_not_prevent_supported_keys_in_a_bitmap() {
    let descriptor = [
        5, 1, 9, 6, 0xa1, 1, 5, 7, 9, 4, 9, 0x82, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 2, 0x81, 2,
        0x75, 6, 0x95, 1, 0x81, 1, 0xc0,
    ];
    let mut decoder = Decoder::new(&descriptor).unwrap();
    assert_eq!(
        decoder.decode(0, &[1]).unwrap(),
        [RawEvent::Key {
            code: 30,
            pressed: true
        }]
    );
    assert_eq!(
        decoder.decode(0, &[0]).unwrap(),
        [RawEvent::Key {
            code: 30,
            pressed: false
        }]
    );
}

#[test]
fn apple_headset_media_button_has_a_matching_release() {
    let mut decoder = Decoder::new(include_bytes!("fixtures/hid/apple-headset.bin")).unwrap();
    let pressed = decoder.decode(0, &[1]).unwrap();
    assert_eq!(pressed.len(), 1);
    let RawEvent::Key {
        code,
        pressed: true,
    } = pressed[0]
    else {
        panic!("expected media key")
    };
    assert_eq!(
        decoder.decode(0, &[0]).unwrap(),
        [RawEvent::Key {
            code,
            pressed: false
        }]
    );
}

#[test]
fn session_reset_does_not_replay_keys_released_while_the_mac_was_asleep() {
    let mut decoder =
        Decoder::new(include_bytes!("fixtures/hid/apple-internal-keyboard.bin")).unwrap();
    decoder.decode(1, &[2, 0, 4, 0, 0, 0, 0, 0, 0]).unwrap();
    decoder.reset();
    assert!(decoder.snapshot().is_empty());
    assert!(decoder.decode(1, &[0; 9]).unwrap().is_empty());
    assert_eq!(
        decoder
            .decode(1, &[2, 0, 4, 0, 0, 0, 0, 0, 0])
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn nkro_error_bits_are_valid_descriptor_fields_and_fail_only_when_set() {
    let descriptor = [
        5, 1, 9, 6, 0xa1, 1, 5, 7, 0x19, 0, 0x29, 7, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 8, 0x81, 2,
        0xc0,
    ];
    let mut decoder = Decoder::new(&descriptor).unwrap();
    assert_eq!(
        decoder.decode(0, &[0x10]).unwrap(),
        [RawEvent::Key {
            code: 30,
            pressed: true
        }]
    );
    assert!(decoder.decode(0, &[2]).is_err());
    assert_eq!(
        decoder.decode(0, &[0]).unwrap(),
        [RawEvent::Key {
            code: 30,
            pressed: false
        }]
    );
}
