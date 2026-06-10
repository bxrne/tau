//! **Target** abstraction: same [`crate::op::Op`] workload, different ways to reach libtau.

mod wire;

use libtau::{ExecError, Executor, Output};

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

/// In-process [`Executor`] (direct libtau path).
pub struct DirectExecutor<'a>(pub &'a mut Executor);

impl Target for DirectExecutor<'_> {
    fn exec(&mut self, sql: &str) -> Result<Output, ExecError> {
        let (_, stmt) = libtau::parse(sql).unwrap_or_else(|_| panic!("parse failed: {sql}"));
        self.0.exec(&stmt)
    }

    fn is_in_transaction(&self) -> bool {
        self.0.is_in_transaction()
    }
}
