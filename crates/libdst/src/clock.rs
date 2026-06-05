//! Virtual monotonic clock for deterministic simulation.
//!
//! A [`Clock`] holds an `i64` "now" value that advances only when the simulation
//! calls [`Clock::set`] or [`Clock::advance`]. Pass a reference to any code that
//! calls `now()` so time is fully deterministic across seeds.

use std::sync::atomic::{AtomicI64, Ordering};

/// Deterministic virtual clock — `now_secs()` only changes when you say so.
pub struct Clock {
    now_secs: AtomicI64,
}

impl Clock {
    pub const fn new(secs: i64) -> Self {
        Self {
            now_secs: AtomicI64::new(secs),
        }
    }

    pub fn now_secs(&self) -> i64 {
        self.now_secs.load(Ordering::Relaxed)
    }

    pub fn set(&self, secs: i64) {
        self.now_secs.store(secs, Ordering::Relaxed);
    }

    /// Increment by `delta_secs` and return the new value.
    pub fn advance(&self, delta_secs: i64) -> i64 {
        self.now_secs.fetch_add(delta_secs, Ordering::Relaxed) + delta_secs
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Clone for Clock {
    fn clone(&self) -> Self {
        Self::new(self.now_secs())
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clock")
            .field("now_secs", &self.now_secs())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_read() {
        let c = Clock::new(100);
        assert_eq!(c.now_secs(), 100);
        c.set(200);
        assert_eq!(c.now_secs(), 200);
    }

    #[test]
    fn advance_returns_new_value() {
        let c = Clock::new(0);
        assert_eq!(c.advance(10), 10);
        assert_eq!(c.now_secs(), 10);
        assert_eq!(c.advance(5), 15);
    }

    #[test]
    fn clone_captures_current_value_independently() {
        let c = Clock::new(50);
        let c2 = c.clone();
        c.advance(10);
        assert_eq!(c2.now_secs(), 50, "clone must not share the atomic");
    }

    #[hegel::test]
    fn pbt_advance_accumulates(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let a = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
        let b = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
        let c = Clock::new(0);
        c.advance(a);
        c.advance(b);
        assert_eq!(c.now_secs(), a + b);
    }
}
