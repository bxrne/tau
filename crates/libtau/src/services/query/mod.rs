//! Query service: the read half of statement execution.
//!
//! Evaluates every read-only statement (`AT`, `RANGE`, `REDUCE`, `HISTORY`,
//! `SHOW DATABASES` / `LENSES` / `STATUS`) against the [`Registry`] shared
//! with the [`crate::services::db`] service.  Mutations belong to the db
//! service; user statements to the auth service.  The kernel routes between
//! the three.
//!
//! `AT` returns [`Output::Value`] (`None` when no tau covers `t`).
//! `RANGE` returns [`Output::Range`] - a vec of `(start, end, value)` segments.
//! `REDUCE` returns [`Output::Value`] - a single scalar aggregate.
//! `SHOW DATABASES` / `SHOW LENSES` return [`Output::Names`] - a sorted name list.

pub(crate) mod eval;

use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::kernel::{Service, SyscallCtx, SyscallError};
use crate::model::Timestamp;
use crate::ql::ast::{AggFunc, Expr, Stmt};
use crate::services::db::{
    DbState, ExecError, LayerInfo, Output, Registry, ensure_base_lens, ensure_lens_exists,
    ensure_single_axis,
};
use eval::{build_range_segments, collect_range_bounds, eval_agg, eval_lens, ttl_cutoff};

fn apply_offset_limit<T>(v: Vec<T>, offset: Option<usize>, limit: Option<usize>) -> Vec<T> {
    if offset.is_none() && limit.is_none() {
        return v;
    }
    v.into_iter()
        .skip(offset.unwrap_or(0))
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

/// The read service.  Holds a shared handle to the db service's registry and
/// never takes a per-database write lock, so concurrent lookups don't
/// serialise on each other or on writers.
pub struct QueryService {
    registry: Arc<RwLock<Registry>>,
    /// Instant the service was created — used to report `uptime_secs` in
    /// `SHOW STATUS`.
    started_at: Instant,
}

impl Service for QueryService {
    fn boot(&mut self, _ctx: &mut SyscallCtx<'_>) -> Result<(), SyscallError> {
        Ok(())
    }
}

impl QueryService {
    pub(crate) fn new(registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            registry,
            started_at: Instant::now(),
        }
    }

    /// Execute a read-only statement.  Returns [`ExecError::InvalidExpr`] for
    /// any mutating statement — those belong to the db service.
    pub fn exec_read(&self, stmt: &Stmt) -> Result<Output, ExecError> {
        match stmt {
            Stmt::At { name, t } => self.at_lens(name, *t),
            Stmt::AtNd { name, ts, as_of } => self.at_nd_lens(name, ts, *as_of),
            Stmt::AtAsOf { name, t, as_of } => self.at_as_of_lens(name, *t, *as_of),
            Stmt::AtLayer { name, t, layer_id } => self.at_layer_lens(name, *t, *layer_id),
            Stmt::HistoryLens { name, range } => self.history_lens(name, *range),
            Stmt::Range {
                name,
                start,
                end,
                filter,
                limit,
                offset,
            } => self.range_lens(name, *start, *end, filter.as_ref(), *limit, *offset),
            Stmt::RangeNd {
                name,
                start,
                end,
                fixed,
            } => self.range_nd_lens(name, *start, *end, fixed),
            Stmt::Reduce {
                name,
                start,
                end,
                func,
            } => self.reduce_lens(name, *start, *end, *func),
            Stmt::ShowDatabases => self.show_databases(),
            Stmt::ShowLenses => self.show_lenses(),
            Stmt::ShowStatus => self.show_status(),
            _ => Err(ExecError::InvalidExpr(
                "query service: not a read-only statement".into(),
            )),
        }
    }

    /// Returns the `Arc<RwLock<DbState>>` for the active database.
    fn active_db_arc(&self) -> Result<Arc<RwLock<DbState>>, ExecError> {
        self.registry
            .read()
            .expect("registry lock poisoned")
            .active_db_arc()
    }

    fn at_lens(&self, name: &str, t: Timestamp) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_single_axis(&state, name, "AT")?;
        if ttl_cutoff(&state, name).is_some_and(|c| t < c) {
            return Ok(Output::Value(None));
        }
        Ok(Output::Value(eval_lens(&state, name, t)?))
    }

    /// N-dimensional point lookup: newest layer wins, optionally scoped to
    /// layers written at or before `as_of`.
    fn at_nd_lens(&self, name: &str, ts: &[i64], as_of: Option<i64>) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_base_lens(&state, name, "AT")?;
        let arity = state.axes.get(name).map_or(1, Vec::len);
        if ts.len() != arity {
            return Err(crate::services::db::arity_error(name, arity, ts.len()));
        }
        Ok(Output::Value(state.db.get(name, ts, as_of)))
    }

    fn at_as_of_lens(&self, name: &str, t: Timestamp, as_of: i64) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_base_lens(&state, name, "AT AS OF")?;
        ensure_single_axis(&state, name, "AT AS OF")?;
        Ok(Output::Value(state.db.get(name, &[t], Some(as_of))))
    }

    fn at_layer_lens(&self, name: &str, t: Timestamp, layer_id: u64) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_base_lens(&state, name, "AT LAYER")?;
        ensure_single_axis(&state, name, "AT LAYER")?;
        let result = state
            .db
            .layers(name)
            .as_deref()
            .and_then(|ls| ls.iter().find(|l| l.id == layer_id))
            .and_then(|l| l.at(t))
            .cloned();
        Ok(Output::Value(result))
    }

    fn history_lens(&self, name: &str, range: Option<(i64, i64)>) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_lens_exists(&state, name)?;
        let infos = state
            .db
            .layer_infos(name)
            .into_iter()
            .filter(|(_, _, min_start, max_end, _)| match range {
                Some((start, end)) => *max_end > start && *min_start < end,
                None => true,
            })
            .map(
                |(id, written_at, min_start, max_end, tau_count)| LayerInfo {
                    id,
                    written_at,
                    min_start,
                    max_end,
                    tau_count,
                },
            )
            .collect();
        Ok(Output::LayerHistory(infos))
    }

    fn range_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        filter: Option<&Expr>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_lens_exists(&state, name)?;
        ensure_single_axis(&state, name, "RANGE")?;
        let effective_start = ttl_cutoff(&state, name).map_or(start, |c| start.max(c));
        if effective_start >= end {
            return Ok(Output::Range(vec![]));
        }
        // Fast path for unfiltered base-lens queries: pushed to the store as a
        // single valid-axis scan (no non-valid axes to fix on a 1-D lens).
        if filter.is_none() && state.base_types.contains_key(name) {
            let out = state.db.scan(name, effective_start, end, &[], None);
            return Ok(Output::Range(apply_offset_limit(out, offset, limit)));
        }
        let (bounds, layers_snap) =
            collect_range_bounds(&state, name, effective_start, end, filter)?;
        let out = build_range_segments(&state, name, &bounds, layers_snap.as_deref(), filter)?;
        Ok(Output::Range(apply_offset_limit(out, offset, limit)))
    }

    /// N-dimensional range scan: sweep valid time over `[start, end)` with the
    /// remaining axes fixed at `fixed`. Taus whose non-valid axes contain the
    /// fixed points form a 1-D non-overlapping view per layer (two taus that
    /// both cover the fixed points and overlap on valid time would be a full
    /// orthotope overlap, which the append path rejects), so the standard
    /// newest-wins sweep applies unchanged.
    fn range_nd_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        fixed: &[i64],
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_base_lens(&state, name, "RANGE")?;
        let arity = state.axes.get(name).map_or(1, Vec::len);
        if fixed.len() + 1 != arity {
            return Err(crate::services::db::arity_error(
                name,
                arity,
                fixed.len() + 1,
            ));
        }
        Ok(Output::Range(state.db.scan(name, start, end, fixed, None)))
    }

    fn reduce_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        func: AggFunc,
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_lens_exists(&state, name)?;
        ensure_single_axis(&state, name, "REDUCE")?;
        let effective_start = ttl_cutoff(&state, name).map_or(start, |c| start.max(c));
        if effective_start >= end {
            return Ok(Output::Value(None));
        }
        eval_agg(&state, name, func, effective_start, end).map(Output::Value)
    }

    fn show_status(&self) -> Result<Output, ExecError> {
        let uptime = self.started_at.elapsed().as_secs();
        let reg = self.registry.read().expect("registry lock poisoned");
        let db_count = reg.databases.len();
        let mut lens_count = 0usize;
        let mut wal_bytes = 0u64;
        for db_arc in reg.databases.values() {
            let state = db_arc.read().expect("db lock poisoned");
            lens_count += state.base_types.len() + state.derived.len();
            wal_bytes += state.db.wal_size_bytes();
        }
        Ok(Output::Status(vec![
            ("uptime_secs".into(), uptime.to_string()),
            ("databases".into(), db_count.to_string()),
            ("lenses".into(), lens_count.to_string()),
            ("wal_bytes".into(), wal_bytes.to_string()),
        ]))
    }

    fn show_databases(&self) -> Result<Output, ExecError> {
        let reg = self.registry.read().expect("registry lock poisoned");
        let mut names: Vec<String> = reg.databases.keys().cloned().collect();
        names.sort();
        Ok(Output::Names(names))
    }

    fn show_lenses(&self) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        let mut names: Vec<String> = state
            .base_types
            .keys()
            .chain(state.derived.keys())
            .cloned()
            .collect();
        names.sort();
        Ok(Output::Names(names))
    }
}
