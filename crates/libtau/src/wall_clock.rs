//! Virtual wall clock shared by TTL, `AS OF` transaction stamps, and layer
//! `written_at`.
//!
//! A single millisecond-resolution override backs both [`now_ms`] (the
//! transaction timestamp stamped onto every appended layer) and [`now_secs`]
//! (TTL cutoffs). [`set_fixed_now_ms`] / [`set_fixed_now_secs`] pin it for
//! deterministic simulation; when unset (`0`) both fall through to the system
//! clock. Pinning the *millisecond* clock is what lets DST advance transaction
//! time deterministically and exercise `AT ... AS OF` across compaction.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::model::Timestamp;

static FIXED_NOW_MS: AtomicI64 = AtomicI64::new(0);

/// Pin "now" (millisecond resolution) for deterministic simulation. Pass `0` to
/// [`clear_fixed_now`] to restore the system clock.
pub fn set_fixed_now_ms(ms: Timestamp) {
    FIXED_NOW_MS.store(ms, Ordering::Relaxed);
}

/// Pin "now" from a seconds value (convenience over [`set_fixed_now_ms`]).
pub fn set_fixed_now_secs(secs: Timestamp) {
    set_fixed_now_ms(secs.saturating_mul(1000));
}

/// Restore system wall clock.
pub fn clear_fixed_now() {
    FIXED_NOW_MS.store(0, Ordering::Relaxed);
}

/// Milliseconds since Unix epoch — fixed override or system time. This is the
/// transaction timestamp written onto each appended layer.
pub fn now_ms() -> Timestamp {
    let fixed = FIXED_NOW_MS.load(Ordering::Relaxed);
    if fixed != 0 {
        return fixed;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Timestamp)
        .unwrap_or(0)
}

/// Seconds since Unix epoch — fixed override (derived from the millisecond
/// clock) or system time.
pub fn now_secs() -> Timestamp {
    let fixed = FIXED_NOW_MS.load(Ordering::Relaxed);
    if fixed != 0 {
        return fixed / 1000;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as Timestamp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_now_overrides_system() {
        set_fixed_now_secs(1_234_567);
        assert_eq!(now_secs(), 1_234_567);
        assert_eq!(now_ms(), 1_234_567_000);
        clear_fixed_now();
    }

    #[test]
    fn fixed_ms_clock_drives_both_resolutions() {
        set_fixed_now_ms(1_700_000_000_500);
        assert_eq!(now_ms(), 1_700_000_000_500);
        assert_eq!(now_secs(), 1_700_000_000);
        clear_fixed_now();
    }
}
