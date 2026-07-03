//! Per-kernel virtual clock.
//!
//! Every [`Kernel`](crate::Kernel) owns one `Clock`, shared with the db and
//! query services (and each database's state).  It backs transaction stamps
//! (`written_at` on appended layers, the coordinate `AS OF` filters on) and
//! TTL cutoffs.
//!
//! By default the clock reads system time.  Pinning it
//! ([`Clock::set_fixed_now_ms`]) makes one kernel fully deterministic without
//! touching any other kernel in the process — deterministic simulations can
//! run in parallel, each driving its own clock.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::model::Timestamp;

/// Millisecond-resolution virtual clock.  `0` means "follow the system clock".
#[derive(Debug, Default)]
pub struct Clock {
    fixed_ms: AtomicI64,
}

impl Clock {
    /// A clock that follows the system wall clock until pinned.
    pub fn system() -> Self {
        Self {
            fixed_ms: AtomicI64::new(0),
        }
    }

    /// A clock pinned at `ms` from birth.
    pub fn fixed(ms: Timestamp) -> Self {
        Self {
            fixed_ms: AtomicI64::new(ms),
        }
    }

    /// Pin "now" (millisecond resolution) for deterministic simulation.
    /// Pass `0` to [`Clock::clear_fixed`] to restore the system clock.
    pub fn set_fixed_now_ms(&self, ms: Timestamp) {
        self.fixed_ms.store(ms, Ordering::Relaxed);
    }

    /// Pin "now" from a seconds value (convenience over [`Clock::set_fixed_now_ms`]).
    pub fn set_fixed_now_secs(&self, secs: Timestamp) {
        self.set_fixed_now_ms(secs.saturating_mul(1000));
    }

    /// Restore the system wall clock.
    pub fn clear_fixed(&self) {
        self.fixed_ms.store(0, Ordering::Relaxed);
    }

    /// Milliseconds since Unix epoch — pinned value or system time.  This is
    /// the transaction timestamp written onto each appended layer.
    pub fn now_ms(&self) -> Timestamp {
        let fixed = self.fixed_ms.load(Ordering::Relaxed);
        if fixed != 0 {
            return fixed;
        }
        system_now_ms()
    }

    /// Seconds since Unix epoch — pinned value (derived from the millisecond
    /// clock) or system time.  TTL cutoffs use this resolution.
    pub fn now_secs(&self) -> Timestamp {
        self.now_ms() / 1000
    }
}

/// Milliseconds since Unix epoch from the system clock (no override).
pub(crate) fn system_now_ms() -> Timestamp {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Timestamp)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_overrides_system() {
        let c = Clock::fixed(1_234_567_000);
        assert_eq!(c.now_ms(), 1_234_567_000);
        assert_eq!(c.now_secs(), 1_234_567);
    }

    #[test]
    fn clear_restores_system_time() {
        let c = Clock::fixed(1);
        c.clear_fixed();
        assert!(c.now_ms() > 1_000_000_000_000, "expected system time");
    }

    #[test]
    fn clocks_are_independent() {
        let a = Clock::fixed(1_000);
        let b = Clock::fixed(2_000);
        a.set_fixed_now_ms(5_000);
        assert_eq!(a.now_ms(), 5_000);
        assert_eq!(b.now_ms(), 2_000);
    }
}
