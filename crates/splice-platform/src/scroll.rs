use splice_proto::InputEvent;

pub fn mac_pixels_to_wire(dx: f64, dy: f64) -> InputEvent {
    InputEvent::ScrollPixels { dx: -dx, dy: -dy }
}

pub fn mac_lines_to_wire(dx: i64, dy: i64) -> InputEvent {
    InputEvent::Scroll120 {
        dx: dx
            .saturating_mul(-120)
            .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        dy: dy
            .saturating_mul(-120)
            .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    }
}

pub fn wire_pixels_to_mac(dx: f64, dy: f64) -> (f64, f64) {
    (-dx, -dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_round_trip_keeps_both_axes_and_fractional_motion() {
        for (dx, dy) in [(-1.25, 2.75), (1.0, -2.0), (0.0, 0.0)] {
            let InputEvent::ScrollPixels { dx: wx, dy: wy } = mac_pixels_to_wire(dx, dy) else {
                panic!()
            };
            assert_eq!(wire_pixels_to_mac(wx, wy), (dx, dy));
        }
        assert_eq!(
            mac_lines_to_wire(i64::MIN, i64::MAX),
            InputEvent::Scroll120 {
                dx: i32::MAX,
                dy: i32::MIN
            }
        );
    }

    #[test]
    fn mac_scroll_matches_wayland_direction_with_natural_scrolling_off() {
        assert_eq!(
            mac_lines_to_wire(-1, -2),
            InputEvent::Scroll120 { dx: 120, dy: 240 }
        );
        assert_eq!(
            mac_pixels_to_wire(-0.5, -2.25),
            InputEvent::ScrollPixels { dx: 0.5, dy: 2.25 }
        );
    }

    #[test]
    fn mac_scroll_preserves_the_direction_already_chosen_by_the_source() {
        assert_eq!(
            mac_lines_to_wire(1, 2),
            InputEvent::Scroll120 { dx: -120, dy: -240 }
        );
        assert_eq!(
            mac_pixels_to_wire(0.5, 2.25),
            InputEvent::ScrollPixels {
                dx: -0.5,
                dy: -2.25
            }
        );
    }
}
