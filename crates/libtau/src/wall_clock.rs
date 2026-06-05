//! Wall-clock seconds for TTL and related queries.
//!
//! [`set_fixed_now_secs`] overrides the clock (used by DST). When unset (`0`),
//! queries use the system clock.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::model::Timestamp;

static FIXED_NOW_SECS: AtomicI64 = AtomicI64::new(0);

/// Pin "now" for deterministic simulation (pass `0` to [`clear_fixed_now`]).
pub fn set_fixed_now_secs(secs: Timestamp) {
    FIXED_NOW_SECS.store(secs, Ordering::Relaxed);
}

/// Restore system wall clock.
pub fn clear_fixed_now() {
    FIXED_NOW_SECS.store(0, Ordering::Relaxed);
}

/// Seconds since Unix epoch — fixed override or system time.
pub fn now_secs() -> Timestamp {
    let fixed = FIXED_NOW_SECS.load(Ordering::Relaxed);
    if fixed != 0 {
        return fixed;
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
        clear_fixed_now();
    }
}
