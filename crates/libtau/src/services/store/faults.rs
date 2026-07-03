//! Deterministic I/O fault injection at the storage syscall boundary.
//!
//! Every [`Kernel`](crate::Kernel) owns one [`FaultInjector`], shared into
//! each database's WAL.  Arming it makes a chosen upcoming WAL write fail
//! with an injected `io::Error` — exercising the WAL-first invariant (a
//! failed WAL write must leave the in-memory store untouched and surface as
//! a clean [`ExecError::Io`](crate::ExecError::Io), never a panic or a
//! half-applied statement) without touching any file on disk.
//!
//! The injector is per-kernel: simulations arming faults in parallel do not
//! interfere with each other.

use std::io;
use std::sync::atomic::{AtomicI64, Ordering};

/// Message carried by every injected write failure, so tests can assert the
/// error came from the injector and not a real I/O problem.
pub const INJECTED_WAL_WRITE_ERROR: &str = "injected fault: WAL write failed";

/// Countdown-armed fault source.  Disarmed by default; re-arms only when
/// asked, so a fired fault never leaks into later operations.
#[derive(Debug, Default)]
pub struct FaultInjector {
    /// WAL writes remaining until one fails: `-1` disarmed, `1` means the
    /// very next write fails.  Decremented on each write; firing disarms.
    wal_writes_until_failure: AtomicI64,
}

impl FaultInjector {
    pub fn new() -> Self {
        Self {
            wal_writes_until_failure: AtomicI64::new(-1),
        }
    }

    /// Arm a failure on the `nth` upcoming WAL write (`1` = the very next).
    /// The injector disarms itself after firing.
    pub fn arm_wal_write_failure(&self, nth: u64) {
        self.wal_writes_until_failure
            .store(nth.max(1) as i64, Ordering::SeqCst);
    }

    /// Cancel any armed fault.
    pub fn disarm(&self) {
        self.wal_writes_until_failure.store(-1, Ordering::SeqCst);
    }

    /// Whether a WAL-write fault is currently armed.
    pub fn is_armed(&self) -> bool {
        self.wal_writes_until_failure.load(Ordering::SeqCst) > 0
    }

    /// Called by the WAL before each write.  Counts down and, when the armed
    /// write is reached, disarms and returns the injected error.
    pub(crate) fn check_wal_write(&self) -> io::Result<()> {
        let fired = self
            .wal_writes_until_failure
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
                n if n < 0 => None, // disarmed: no update
                1 => Some(-1),      // firing write: disarm
                n => Some(n - 1),   // count down
            })
            .map(|prev| prev == 1)
            .unwrap_or(false);
        if fired {
            Err(io::Error::other(INJECTED_WAL_WRITE_ERROR))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disarmed_never_fires() {
        let f = FaultInjector::new();
        for _ in 0..100 {
            assert!(f.check_wal_write().is_ok());
        }
    }

    #[test]
    fn fires_on_nth_write_then_disarms() {
        let f = FaultInjector::new();
        f.arm_wal_write_failure(3);
        assert!(f.check_wal_write().is_ok());
        assert!(f.check_wal_write().is_ok());
        let err = f.check_wal_write().expect_err("third write must fail");
        assert!(err.to_string().contains("injected"));
        assert!(!f.is_armed());
        assert!(f.check_wal_write().is_ok());
    }

    #[test]
    fn disarm_cancels() {
        let f = FaultInjector::new();
        f.arm_wal_write_failure(1);
        f.disarm();
        assert!(f.check_wal_write().is_ok());
    }
}
