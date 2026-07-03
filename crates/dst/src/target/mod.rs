//! **Target** abstraction: same [`crate::op::Op`] workload, different ways to reach libtau.

mod wire;

use libtau::{ExecError, Kernel, Output};

pub use wire::{DST_AUTH_PASS, DST_AUTH_USER, WireClient};

pub trait Target {
    fn exec(&mut self, sql: &str) -> Result<Output, ExecError>;

    fn is_in_transaction(&self) -> bool {
        false
    }

    /// Best-effort rollback when the target still holds an open transaction.
    fn rollback_open_transaction(&mut self) {
        let _ = self.exec("ROLLBACK");
    }
}

/// In-process [`Kernel`] (direct libtau path).  The kernel locks internally,
/// so a shared reference suffices even for mutations.
pub struct DirectKernel<'a>(pub &'a Kernel);

impl Target for DirectKernel<'_> {
    fn exec(&mut self, sql: &str) -> Result<Output, ExecError> {
        let (_, stmt) = libtau::parse(sql).unwrap_or_else(|_| panic!("parse failed: {sql}"));
        self.0.exec(&stmt)
    }

    fn is_in_transaction(&self) -> bool {
        self.0.is_in_transaction()
    }
}
