#[cfg(target_os = "macos")]
pub fn now_us() -> u64 {
    mach_us(unsafe { mach::mach_absolute_time() })
}

#[cfg(target_os = "macos")]
pub(crate) mod mach {
    #[repr(C)]
    pub struct Timebase {
        pub numer: u32,
        pub denom: u32,
    }
    extern "C" {
        pub fn mach_absolute_time() -> u64;
        pub fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn mach_us(ticks: u64) -> u64 {
    static SCALE: std::sync::OnceLock<mach::Timebase> = std::sync::OnceLock::new();
    let scale = SCALE.get_or_init(|| {
        let mut scale = mach::Timebase { numer: 0, denom: 0 };
        assert_eq!(unsafe { mach::mach_timebase_info(&mut scale) }, 0);
        assert_ne!(scale.denom, 0);
        scale
    });
    (u128::from(ticks) * u128::from(scale.numer) / u128::from(scale.denom) / 1000)
        .try_into()
        .expect("monotonic clock exceeds microsecond range")
}

#[cfg(not(target_os = "macos"))]
pub fn now_us() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) },
        0
    );
    u64::try_from(time.tv_sec).expect("negative monotonic clock") * 1_000_000
        + u64::try_from(time.tv_nsec).expect("negative monotonic fraction") / 1000
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_clock_advances_in_microseconds() {
        let before = super::now_us();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let elapsed = super::now_us() - before;
        assert!(before > 0);
        assert!(elapsed >= 1000);
    }
}
