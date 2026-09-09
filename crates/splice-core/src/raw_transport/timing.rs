const WINDOW_US: u64 = 10_000_000;

#[derive(Default)]
pub(super) struct ClockMap {
    start_us: Option<u64>,
    current: Option<i128>,
    previous: Option<i128>,
}

impl ClockMap {
    pub(super) fn observe(&mut self, sent_us: u64, received_us: u64) {
        let start = *self.start_us.get_or_insert(received_us);
        let elapsed = received_us.saturating_sub(start);
        if elapsed >= WINDOW_US {
            self.previous = if elapsed < WINDOW_US * 2 {
                self.current
            } else {
                None
            };
            self.current = None;
            self.start_us = Some(received_us);
        }
        let offset = i128::from(received_us) - i128::from(sent_us);
        self.current = Some(self.current.map_or(offset, |old| old.min(offset)));
    }

    pub(super) fn map(&self, native_us: u64) -> u64 {
        let offset = self
            .current
            .into_iter()
            .chain(self.previous)
            .min()
            .expect("clock sample precedes mapping");
        (i128::from(native_us) + offset).clamp(1, i128::from(u64::MAX)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalls_preserve_native_spacing_and_do_not_create_future_timestamps() {
        for offset in [-80_000_000i64, 80_000_000] {
            let mut clock = ClockMap::default();
            let base = 100_000_000u64;
            let local = |sent: u64, delay: u64| (sent as i64 + offset) as u64 + delay;
            clock.observe(base, local(base, 2000));
            let first = clock.map(base - 500);
            clock.observe(base + 1000, local(base + 1000, 102_000));
            let second = clock.map(base + 500);
            assert_eq!(second - first, 1000);
            let mut random = 123u64;
            for i in 2..35_000 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let sent = base + i * 1000;
                let received = local(sent, 2000 + random % 100_000);
                clock.observe(sent, received);
                assert!(clock.map(sent - 500) <= received - 500);
            }
        }
    }

    #[test]
    fn rolling_samples_track_slow_clock_drift_and_ping_only_idle() {
        let mut clock = ClockMap::default();
        for i in 0..3000 {
            let sent = 100_000_000 + i * 200_000;
            let received = sent + 70_000_000 + i * 20 + 2000;
            clock.observe(sent, received);
            assert!(received - clock.map(sent) <= 2000);
        }
        let sent = 800_000_000;
        let received = sent + 80_000_000;
        clock.observe(sent, received);
        assert_eq!(clock.map(sent), received);
    }

    #[test]
    fn independent_sessions_and_boot_near_zero_are_bounded() {
        let mut clock = ClockMap::default();
        clock.observe(100_000_000, 100);
        assert_eq!(clock.map(1), 1);
        let mut next = ClockMap::default();
        next.observe(200_000_000, 5_000_000);
        assert_eq!(next.map(200_000_000), 5_000_000);
    }
}
