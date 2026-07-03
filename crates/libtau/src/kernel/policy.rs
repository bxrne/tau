//! Kernel-level statement policy: per-statement permission checks and
//! result filtering.  The kernel mediates every statement, so authorization
//! lives here — services never see a statement the caller wasn't allowed to
//! run.

use crate::ql::ast::Stmt;
use crate::services::auth::{Perm, User};
use crate::services::db::{ExecError, Output};
use crate::services::metrics::{Metrics, Op};

/// Per-statement permission check.  Returns `Err(PermissionDenied)` when
/// the caller does not have the right grants.
pub(crate) fn check_permission(
    stmt: &Stmt,
    user: &User,
    active: Option<&str>,
) -> Result<(), ExecError> {
    let require = |db: &str, bit: Perm| {
        if user.effective(db).contains(bit) {
            Ok(())
        } else {
            Err(ExecError::PermissionDenied(format!(
                "user '{}' lacks {} on {}",
                user.name, bit, db
            )))
        }
    };
    let require_global_admin = || {
        if user.is_global_admin() {
            Ok(())
        } else {
            Err(ExecError::PermissionDenied(format!(
                "user '{}' is not a global admin",
                user.name
            )))
        }
    };
    let require_active = || active.ok_or(ExecError::NoActiveDatabase);
    let require_admin_or_a_on = |db: &str| {
        if user.is_global_admin() || user.effective(db).contains(Perm::A) {
            Ok(())
        } else {
            Err(ExecError::PermissionDenied(format!(
                "user '{}' lacks A on {}",
                user.name, db
            )))
        }
    };

    let require_any_grant = |db: &str| {
        if user.effective(db).is_empty() {
            Err(ExecError::PermissionDenied(format!(
                "user '{}' has no grants on {}",
                user.name, db
            )))
        } else {
            Ok(())
        }
    };
    match stmt {
        Stmt::CreateDatabase { .. } => require_global_admin(),
        Stmt::DropDatabase { name } => require_admin_or_a_on(name),
        Stmt::UseDatabase { name } => require_any_grant(name),
        Stmt::Create { .. } => require(require_active()?, Perm::C),
        Stmt::Append { .. } | Stmt::AppendNd { .. } | Stmt::Copy { .. } => {
            require(require_active()?, Perm::U)
        }
        Stmt::Derive { .. } | Stmt::Xderive { .. } => require(require_active()?, Perm::C),
        Stmt::At { .. }
        | Stmt::AtNd { .. }
        | Stmt::Range { .. }
        | Stmt::RangeNd { .. }
        | Stmt::Reduce { .. } => require(require_active()?, Perm::R),
        Stmt::Drop { .. } => require(require_active()?, Perm::D),
        Stmt::ShowDatabases => Ok(()),
        Stmt::ShowLenses => require(require_active()?, Perm::R),
        Stmt::CreateUser { .. } | Stmt::DropUser { .. } | Stmt::ShowUsers => require_global_admin(),
        Stmt::Grant { database, .. } | Stmt::Revoke { database, .. } => {
            require_admin_or_a_on(database)
        }
        Stmt::ShowGrants { user: target } => {
            if target.as_deref().is_some_and(|t| t == user.name) {
                return Ok(());
            }
            require_global_admin()
        }
        Stmt::StartTransaction | Stmt::Commit | Stmt::Rollback => Ok(()),
        Stmt::BatchAppend { .. } => require(require_active()?, Perm::U),
        Stmt::AtAsOf { .. } | Stmt::AtLayer { .. } | Stmt::HistoryLens { .. } => {
            require(require_active()?, Perm::R)
        }
        Stmt::SetTtl { .. } | Stmt::UnsetTtl { .. } => require(require_active()?, Perm::C),
        Stmt::ShowStatus => Ok(()),
        Stmt::BackupDatabase { name, .. } => require(name, Perm::R),
        Stmt::RestoreDatabase { .. } => require_global_admin(),
    }
}

/// For `SHOW DATABASES` returned to a non-global-admin caller, drop entries
/// they have no grants on.  Pass-through for every other statement / caller.
pub(crate) fn filter_show_databases(out: Output, stmt: &Stmt, user: &User) -> Output {
    if matches!(stmt, Stmt::ShowDatabases)
        && !user.is_global_admin()
        && let Output::Names(names) = out
    {
        return Output::Names(
            names
                .into_iter()
                .filter(|n| !user.effective(n).is_empty())
                .collect(),
        );
    }
    out
}

pub(crate) fn record_metrics(metrics: &Metrics, active: Option<&str>, stmt: &Stmt, ns: u64) {
    let op = stmt_to_op(stmt);
    metrics.record_op(op, ns);
    if let Some(db) = active {
        metrics.record_db_op(db, op);
    }
}

fn stmt_to_op(stmt: &Stmt) -> Op {
    match stmt {
        Stmt::Append { .. } | Stmt::BatchAppend { .. } | Stmt::AppendNd { .. } => Op::Append,
        Stmt::Copy { .. } => Op::Copy,
        Stmt::At { .. } | Stmt::AtNd { .. } | Stmt::AtAsOf { .. } | Stmt::AtLayer { .. } => Op::At,
        Stmt::Range { .. } | Stmt::RangeNd { .. } => Op::Range,
        Stmt::Reduce { .. } => Op::Reduce,
        Stmt::HistoryLens { .. } => Op::History,
        Stmt::Create { .. } | Stmt::Derive { .. } | Stmt::Xderive { .. } => Op::CreateLens,
        Stmt::Drop { .. } => Op::DropLens,
        Stmt::ShowDatabases
        | Stmt::ShowLenses
        | Stmt::ShowStatus
        | Stmt::ShowUsers
        | Stmt::ShowGrants { .. } => Op::Show,
        Stmt::CreateDatabase { .. } | Stmt::DropDatabase { .. } | Stmt::UseDatabase { .. } => {
            Op::Database
        }
        Stmt::SetTtl { .. } | Stmt::UnsetTtl { .. } => Op::CreateLens,
        Stmt::CreateUser { .. }
        | Stmt::DropUser { .. }
        | Stmt::Grant { .. }
        | Stmt::Revoke { .. } => Op::User,
        Stmt::BackupDatabase { .. } | Stmt::RestoreDatabase { .. } => Op::Backup,
        Stmt::StartTransaction | Stmt::Commit | Stmt::Rollback => Op::Transaction,
    }
}
