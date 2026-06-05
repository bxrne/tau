//! **Target** abstraction: same [`crate::op::Op`] workload, different ways to reach libtau.

mod wire;

use libtau::{ExecError, Executor, Output};

use crate::op::Op;

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

#[allow(dead_code)]
pub fn apply_op(target: &mut impl Target, op: &Op) -> Result<(), ExecError> {
    match op {
        Op::Append { lens, data } => {
            target.exec(&data.batch_sql(lens))?;
        }
        Op::At { lens, t } => {
            target.exec(&format!("AT LENS {lens} {t}"))?;
        }
        Op::Range { lens, start, end } => {
            if start < end {
                target.exec(&format!("RANGE LENS {lens} {start} {end}"))?;
            }
        }
        Op::Reduce {
            lens,
            start,
            end,
            func,
        } => {
            target.exec(&format!("REDUCE LENS {lens} {start} {end} USING {func}"))?;
        }
        Op::CreateLens { name, ty } => {
            target.exec(&format!("CREATE LENS {name} {ty}"))?;
        }
        Op::DropLens { name } => {
            target.exec(&format!("DROP LENS {name}"))?;
        }
        Op::Derive { name, spec } => {
            target.exec(&format!("DERIVE LENS {name} AS {} + {}", spec.a, spec.b))?;
        }
        Op::Ttl {
            lens,
            secs: Some(s),
        } => {
            target.exec(&format!("SET TTL LENS {lens} {s}"))?;
        }
        Op::Ttl { lens, secs: None } => {
            target.exec(&format!("UNSET TTL LENS {lens}"))?;
        }
        Op::UseDb(db) => {
            target.exec(&format!("USE DATABASE {db}"))?;
        }
        Op::StartTransaction => {
            target.exec("START TRANSACTION")?;
        }
        Op::Commit => {
            target.exec("COMMIT")?;
        }
        Op::Rollback => {
            target.exec("ROLLBACK")?;
        }
    }
    Ok(())
}
