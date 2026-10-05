//! Real-time scheduling for the threads on the audio path.
//!
//! The processor and the sources' push threads poll with millisecond
//! sleeps. Under macOS App Nap / timer coalescing (an app whose window is
//! hidden, and every process it launched) a 2 ms sleep can take 20+ ms and
//! a 20 ms sleep well over 100 ms, which underruns the small rings between
//! source, processor and device: audible dropouts. Threads with a Mach
//! time-constraint policy — what CoreAudio's own I/O threads use — are
//! exempt and wake on time. Measured on an M1 Air under coalescing:
//! 20 × 20 ms sleeps took 2.7 s on a normal thread (also with the
//! user-interactive QoS class) and 0.40 s with this policy.

/// Ask the OS to schedule the calling thread as a real-time audio thread
/// that runs briefly every `period_ms`. Best effort: returns `false` where
/// unsupported or refused, and the thread keeps its normal priority.
pub fn promote_current_thread(period_ms: f64) -> bool {
    imp::promote(period_ms)
}

#[cfg(target_os = "macos")]
mod imp {
    #[repr(C)]
    struct TimeConstraintPolicy {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: i32,
    }

    #[repr(C)]
    struct TimebaseInfo {
        numer: u32,
        denom: u32,
    }

    const THREAD_TIME_CONSTRAINT_POLICY: i32 = 2;
    const THREAD_TIME_CONSTRAINT_POLICY_COUNT: u32 = 4;

    unsafe extern "C" {
        fn mach_thread_self() -> u32;
        fn mach_timebase_info(info: *mut TimebaseInfo) -> i32;
        fn thread_policy_set(
            thread: u32,
            flavor: i32,
            info: *const TimeConstraintPolicy,
            count: u32,
        ) -> i32;
    }

    pub fn promote(period_ms: f64) -> bool {
        let mut tb = TimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: plain out-parameter call into libSystem.
        if unsafe { mach_timebase_info(&mut tb) } != 0 || tb.numer == 0 {
            return false;
        }
        let abs = |ms: f64| (ms * 1e6 * tb.denom as f64 / tb.numer as f64) as u32;
        // A quarter of each period for work, preemptible: the kernel may
        // still interrupt us for other real-time threads (CoreAudio's).
        let policy = TimeConstraintPolicy {
            period: abs(period_ms),
            computation: abs(period_ms / 4.0),
            constraint: abs(period_ms),
            preemptible: 1,
        };
        // SAFETY: the policy struct matches thread_time_constraint_policy
        // and lives across the call; the port is our own thread.
        let r = unsafe {
            thread_policy_set(
                mach_thread_self(),
                THREAD_TIME_CONSTRAINT_POLICY,
                &policy,
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            )
        };
        if r != 0 {
            tracing::debug!(code = r, "real-time thread policy refused");
        }
        r == 0
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn promote(_period_ms: f64) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn promotion_is_safe_to_call() {
        let ok = std::thread::spawn(|| super::promote_current_thread(11.6))
            .join()
            .unwrap();
        if cfg!(target_os = "macos") {
            assert!(ok, "macOS should grant the time-constraint policy");
        }
    }
}
