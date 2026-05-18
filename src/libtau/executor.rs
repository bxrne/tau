//! Query-language executor.
//!
//! Wires the [`Stmt`](crate::libtau::ql::ast::Stmt) AST to a runtime registry
//! of [`Database<Value>`](crate::libtau::database::Database) instances.
//!
//! # Model
//!
//! * The executor owns a `HashMap` of *named databases*.  `CREATE DATABASE`
//!   adds one, `DROP DATABASE` removes one, `USE DATABASE` selects the
//!   active database for subsequent lens statements.
//! * Each database has a single value type — all of its *base* lenses must
//!   share the type declared at `CREATE LENS`.  An `APPEND` whose literal
//!   does not match the declared type is rejected with [`ExecError::TypeMismatch`].
//! * *Derived* lenses are pure expressions over other lenses.  Their result
//!   type is whatever the expression yields, which may differ from the
//!   underlying base type (e.g. a `bool` derived from an `int` lens).
//! * Every lookup walks the AST live — derived lenses never cache.
//!
//! # Statement semantics
//!
//! | Statement         | Returns                                            |
//! | ----------------- | -------------------------------------------------- |
//! | `CREATE DATABASE` | [`Output::Empty`]                                  |
//! | `DROP DATABASE`   | [`Output::Empty`]                                  |
//! | `USE DATABASE`    | [`Output::Empty`]                                  |
//! | `CREATE LENS`     | [`Output::Empty`]                                  |
//! | `APPEND LENS`     | [`Output::Empty`]                                  |
//! | `DERIVE LENS`     | [`Output::Empty`]                                  |
//! | `AT LENS`         | [`Output::Value`] (`None` if no tau covers `t`)    |
//! | `RANGE LENS`      | [`Output::Range`] — `(start, end, value)` segments |
//! | `DROP LENS`       | [`Output::Empty`]                                  |

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::libtau::database::Database;
use crate::libtau::model::{Layer, Tau, Timestamp};
use crate::libtau::ql::ast::{AggFn, BinOp, Expr, Stmt, Type, UnOp};
use crate::libtau::ql::parse;
use crate::libtau::storage::InMemory;
use crate::libtau::storage::wal::{Wal, WalEntry, WalWaiter};
use crate::libtau::value::Value;
use std::sync::Arc;

/// Output of a single executed statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Statement produced no value (DDL / writes).
    Empty,
    /// Point lookup result.  `None` means the lens has no value at that time.
    Value(Option<Value>),
    /// Sequence of `(start, end, value)` segments covering the queried range.
    Range(Vec<(Timestamp, Timestamp, Value)>),
    /// Aggregation result.  `None` means the queried interval had no values.
    Scalar(Option<Value>),
}

/// All errors the executor can produce.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecError {
    /// No `USE DATABASE` (or initial `CREATE DATABASE`) has set an active DB.
    NoActiveDatabase,
    /// `USE DATABASE` / `DROP DATABASE` targeted a database that doesn't exist.
    UnknownDatabase(String),
    /// `CREATE DATABASE` for a name already present.
    DuplicateDatabase(String),
    /// Reference to a lens that is neither base nor derived in the active DB.
    UnknownLens(String),
    /// `CREATE LENS` / `DERIVE LENS` for a name already in use.
    DuplicateLens(String),
    /// Append value's runtime type does not match the lens's declared type.
    TypeMismatch {
        lens: String,
        expected: Type,
        got: String,
    },
    /// Expression evaluation produced an illegal combination of types.
    InvalidExpr(String),
    /// `APPEND` / `RANGE` with `start >= end`.
    InvalidRange,
}

/// Per-database executor state.
struct DbState {
    db: Database<Value>,
    /// Declared type of every base lens in this database.  Absence of an
    /// entry means the lens is either derived or unknown.
    base_types: HashMap<String, Type>,
    /// Monotonic layer-id source so `APPEND` doesn't need the caller to
    /// supply one.
    next_layer_id: u64,
    /// Derived lens definitions, stored by name.  Lookups recursively
    /// re-evaluate these — no caching.
    derived: HashMap<String, Expr>,
}

impl DbState {
    fn new() -> Self {
        Self {
            db: Database::new(InMemory::<Value>::new()),
            base_types: HashMap::new(),
            next_layer_id: 1,
            derived: HashMap::new(),
        }
    }

    /// Build a state whose `Database` shares the given `Arc<Wal>` for both
    /// DDL persistence (driven by the executor) and data persistence
    /// (driven by `Database::append`).
    fn with_shared_wal(wal: Arc<Wal>) -> Self {
        Self {
            db: Database::with_wal_arc(InMemory::<Value>::new(), wal),
            base_types: HashMap::new(),
            next_layer_id: 1,
            derived: HashMap::new(),
        }
    }

    /// Apply a replayed DATA entry directly to the in-memory store, bypassing
    /// the WAL so replay doesn't double-persist.  Tracks `next_layer_id` so
    /// fresh appends after replay don't collide with replayed ids.
    fn apply_data_entry(&mut self, entry: WalEntry<Value>) {
        let layer_id = entry.layer_id;
        let taus: Vec<Tau<Value>> = entry
            .taus
            .into_iter()
            .map(|(s, e, v)| Tau::new(s, e, v))
            .collect();
        let layer = Layer::new(layer_id, taus);
        self.db.apply_layer(&entry.lens, layer);
        if layer_id >= self.next_layer_id {
            self.next_layer_id = layer_id + 1;
        }
    }
}

/// In-flight append produced by [`Executor::exec_prepare`].  Holds a
/// pre-built [`Layer`] plus an optional [`WalWaiter`] the caller must block
/// on before calling [`Executor::exec_commit`].
pub struct PendingAppend {
    pub(crate) db: String,
    pub(crate) lens: String,
    pub(crate) layer: Layer<Value>,
    pub(crate) waiter: Option<WalWaiter>,
}

impl PendingAppend {
    /// Block until the WAL batch containing this append has been fsynced.
    /// Call this outside the executor write lock so concurrent prepares can
    /// continue to enqueue.
    pub fn wait_for_durability(&mut self) -> io::Result<()> {
        if let Some(w) = self.waiter.take() {
            w.wait()?;
        }
        Ok(())
    }
}

/// Result of [`Executor::exec_prepare`].
pub enum PreparedWrite {
    /// Statement completed entirely under the prepare lock — no further
    /// work needed.  Covers DDL and appends to a WAL-less database.
    Done(Output),
    /// WAL-backed append waiting for durability confirmation.  The caller
    /// must run `wait_for_durability()` then `exec_commit()`.
    Pending(PendingAppend),
}

/// Runtime container for executing parsed [`Stmt`]s.
#[derive(Default)]
pub struct Executor {
    databases: HashMap<String, DbState>,
    /// Name of the currently active database (set by the first
    /// `CREATE DATABASE` and by `USE DATABASE`).  Cleared if the active
    /// database is dropped.
    active: Option<String>,
    /// Shared WAL used for DDL persistence.  When present, every successful
    /// DDL statement is also enqueued so a future restart can reconstruct
    /// the schema before replaying data entries.  Data persistence still
    /// flows through the same WAL via each database's writer path.
    wal: Option<Arc<Wal>>,
    /// True while replaying the WAL from disk.  Suppresses re-persisting
    /// statements that originated from the log we are reading.
    replay_in_progress: bool,
}

impl Executor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an executor with WAL for durability.
    ///
    /// Opens (or creates) the WAL at `path`, replays both DDL (`STMT`) and
    /// data entries in order, then attaches the WAL so subsequent
    /// statements are persisted.  A `default` database is always created
    /// as the initial active context; replayed `CREATE DATABASE` /
    /// `USE DATABASE` statements override which one is active.
    pub fn with_wal(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let existed = path.exists();

        // Open the WAL up front so DDL replay and data replay can both share
        // it (replay paths apply state without re-persisting).
        let wal = Arc::new(Wal::open(&path)?);

        let mut executor = Self::default();
        // Seed the default database, hooked to the shared WAL, so any
        // post-replay writes are persisted via the same writer thread.
        executor.databases.insert(
            "default".to_string(),
            DbState::with_shared_wal(Arc::clone(&wal)),
        );
        executor.active = Some("default".to_string());

        // Replay path reads the file directly without engaging the writer.
        if existed {
            executor.replay_in_progress = true;
            let res = replay_wal_file(&path, &mut executor);
            executor.replay_in_progress = false;
            res?;
        }

        executor.wal = Some(wal);
        Ok(executor)
    }

    /// Name of the active database, if any.
    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Execute a read-only statement (`AT` or `RANGE`) without taking an
    /// exclusive borrow.  Returns [`ExecError::InvalidExpr`] for any
    /// mutating statement.
    ///
    /// The TCP server uses this entry point under a shared read lock so
    /// concurrent lookups don't serialise on each other.
    pub fn exec_read(&self, stmt: &Stmt) -> Result<Output, ExecError> {
        match stmt {
            Stmt::At { name, t } => self.at_lens(name, *t),
            Stmt::Range {
                name,
                start,
                end,
                filter,
            } => self.range_lens(name, *start, *end, filter.as_ref()),
            Stmt::Reduce {
                name,
                start,
                end,
                func,
            } => self.reduce_lens(name, *start, *end, *func),
            _ => Err(ExecError::InvalidExpr(
                "exec_read called on a mutating statement".into(),
            )),
        }
    }

    /// Execute a single parsed statement.
    pub fn exec(&mut self, stmt: &Stmt) -> Result<Output, ExecError> {
        match stmt {
            Stmt::CreateDatabase { name } => self.create_database(name),
            Stmt::DropDatabase { name } => self.drop_database(name),
            Stmt::UseDatabase { name } => self.use_database(name),
            Stmt::Create { name, ty } => self.create_lens(name, ty.clone()),
            Stmt::Append {
                name,
                start,
                end,
                value,
            } => self.append_lens(name, *start, *end, value.clone().into()),
            Stmt::Derive { name, expr } => self.derive_lens(name, expr.clone()),
            Stmt::At { name, t } => self.at_lens(name, *t),
            Stmt::Range {
                name,
                start,
                end,
                filter,
            } => self.range_lens(name, *start, *end, filter.as_ref()),
            Stmt::Drop { name } => self.drop_lens(name),
            Stmt::Compact { name } => self.compact_lens(name),
            Stmt::Reduce {
                name,
                start,
                end,
                func,
            } => self.reduce_lens(name, *start, *end, *func),
        }
    }

    /// Phase 1 of a two-phase write: validate the statement, allocate a
    /// layer id, and enqueue the WAL entry without waiting for the fsync.
    ///
    /// * DDL and other non-`APPEND` mutations execute synchronously and
    ///   return [`PreparedWrite::Done`].
    /// * `APPEND` on a WAL-less database also runs to completion and
    ///   returns `Done`.
    /// * `APPEND` on a WAL-backed database returns
    ///   [`PreparedWrite::Pending`] carrying the pre-built layer and a
    ///   [`WalWaiter`].  The caller must call
    ///   [`PendingAppend::wait_for_durability`] outside the executor lock
    ///   and then [`Executor::exec_commit`] under the write lock again.
    ///
    /// Must be called under the executor write lock.
    pub fn exec_prepare(&mut self, stmt: &Stmt) -> Result<PreparedWrite, ExecError> {
        match stmt {
            Stmt::Append {
                name,
                start,
                end,
                value,
            } => self.prepare_append(name, *start, *end, value.clone().into()),
            _ => self.exec(stmt).map(PreparedWrite::Done),
        }
    }

    /// Phase 3 of a two-phase write: apply a pre-built layer to the active
    /// database's in-memory store.  No WAL interaction — the caller must
    /// have awaited durability already.  Must be called under the executor
    /// write lock.
    pub fn exec_commit(&mut self, pending: PendingAppend) -> Result<Output, ExecError> {
        let state = self
            .databases
            .get_mut(&pending.db)
            .ok_or_else(|| ExecError::UnknownDatabase(pending.db.clone()))?;
        state.db.apply_layer(&pending.lens, pending.layer);
        Ok(Output::Empty)
    }

    fn prepare_append(
        &mut self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        value: Value,
    ) -> Result<PreparedWrite, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let db_name = self
            .active
            .as_deref()
            .ok_or(ExecError::NoActiveDatabase)?
            .to_string();
        let state = self
            .databases
            .get_mut(&db_name)
            .ok_or_else(|| ExecError::UnknownDatabase(db_name.clone()))?;
        let ty = state
            .base_types
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownLens(name.into()))?;
        if let Some(got) = value.ty()
            && got != ty
        {
            return Err(ExecError::TypeMismatch {
                lens: name.into(),
                expected: ty,
                got: value.type_name().into(),
            });
        }
        let id = state.next_layer_id;
        state.next_layer_id += 1;
        let layer = Layer::new(id, vec![Tau::new(start, end, value)]);
        let lens = state.db.lens(name);
        let waiter = state.db.push_wal(&lens, &layer);
        if waiter.is_none() {
            // No WAL: apply immediately, return Done.
            state.db.apply_layer(name, layer);
            Ok(PreparedWrite::Done(Output::Empty))
        } else {
            Ok(PreparedWrite::Pending(PendingAppend {
                db: db_name,
                lens: name.into(),
                layer,
                waiter,
            }))
        }
    }

    // ---- database management ------------------------------------------------

    fn create_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if self.databases.contains_key(name) {
            return Err(ExecError::DuplicateDatabase(name.into()));
        }
        // When a WAL is attached, newly-created databases also share it so
        // future appends to them are durable.
        let state = match &self.wal {
            Some(wal) => DbState::with_shared_wal(Arc::clone(wal)),
            None => DbState::new(),
        };
        self.databases.insert(name.into(), state);
        if self.active.is_none() {
            self.active = Some(name.into());
        }
        self.persist_ddl(&Stmt::CreateDatabase { name: name.into() })?;
        Ok(Output::Empty)
    }

    fn drop_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if self.databases.remove(name).is_none() {
            return Err(ExecError::UnknownDatabase(name.into()));
        }
        if self.active.as_deref() == Some(name) {
            self.active = None;
        }
        self.persist_ddl(&Stmt::DropDatabase { name: name.into() })?;
        Ok(Output::Empty)
    }

    fn use_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if !self.databases.contains_key(name) {
            return Err(ExecError::UnknownDatabase(name.into()));
        }
        self.active = Some(name.into());
        self.persist_ddl(&Stmt::UseDatabase { name: name.into() })?;
        Ok(Output::Empty)
    }

    // ---- lens management ----------------------------------------------------

    fn create_lens(&mut self, name: &str, ty: Type) -> Result<Output, ExecError> {
        let state = self.active_mut()?;
        if state.base_types.contains_key(name) || state.derived.contains_key(name) {
            return Err(ExecError::DuplicateLens(name.into()));
        }
        state.base_types.insert(name.into(), ty.clone());
        self.persist_ddl(&Stmt::Create {
            name: name.into(),
            ty,
        })?;
        Ok(Output::Empty)
    }

    fn append_lens(
        &mut self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        value: Value,
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let state = self.active_mut()?;
        let ty = state
            .base_types
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownLens(name.into()))?;
        // Null is type-compatible with any declared type.
        if let Some(got) = value.ty()
            && got != ty
        {
            return Err(ExecError::TypeMismatch {
                lens: name.into(),
                expected: ty,
                got: value.type_name().into(),
            });
        }
        let id = state.next_layer_id;
        state.next_layer_id += 1;
        let layer = Layer::new(id, vec![Tau::new(start, end, value)]);
        state.db.append(&state.db.lens(name), layer);
        Ok(Output::Empty)
    }

    fn derive_lens(&mut self, name: &str, expr: Expr) -> Result<Output, ExecError> {
        let state = self.active_mut()?;
        if state.base_types.contains_key(name) || state.derived.contains_key(name) {
            return Err(ExecError::DuplicateLens(name.into()));
        }
        state.derived.insert(name.into(), expr.clone());
        self.persist_ddl(&Stmt::Derive {
            name: name.into(),
            expr,
        })?;
        Ok(Output::Empty)
    }

    fn at_lens(&self, name: &str, t: Timestamp) -> Result<Output, ExecError> {
        let state = self.active_state()?;
        Ok(Output::Value(eval_lens(state, name, t)?))
    }

    fn range_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        filter: Option<&Expr>,
    ) -> Result<Output, ExecError> {
        let mut out: Vec<(Timestamp, Timestamp, Value)> = Vec::new();
        self.range_lens_inner(name, start, end, filter, |s, e, v| {
            // Merge with previous segment if it's adjacent and same value.
            match out.last_mut() {
                Some(last) if last.1 == s && last.2 == *v => last.1 = e,
                _ => out.push((s, e, v.clone())),
            }
        })?;
        Ok(Output::Range(out))
    }

    /// Streaming variant of `range_lens` — invokes `cb` for each merged
    /// segment instead of allocating a `Vec`.  Returns the number of segments
    /// emitted so callers can write a header before/after consuming the
    /// stream.
    pub fn exec_range_cb<F>(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        filter: Option<&Expr>,
        mut cb: F,
    ) -> Result<usize, ExecError>
    where
        F: FnMut(Timestamp, Timestamp, &Value),
    {
        // Buffer the current segment so adjacent equal values still merge.
        let mut count = 0usize;
        let mut pending: Option<(Timestamp, Timestamp, Value)> = None;
        self.range_lens_inner(name, start, end, filter, |s, e, v| match &mut pending {
            Some(prev) if prev.1 == s && prev.2 == *v => prev.1 = e,
            _ => {
                if let Some((ps, pe, pv)) = pending.take() {
                    cb(ps, pe, &pv);
                    count += 1;
                }
                pending = Some((s, e, v.clone()));
            }
        })?;
        if let Some((ps, pe, pv)) = pending {
            cb(ps, pe, &pv);
            count += 1;
        }
        Ok(count)
    }

    /// Shared implementation of RANGE iteration.  Calls `each` for every
    /// `(seg_start, seg_end, value)` produced by the change-point sweep —
    /// the caller is responsible for any merging.
    fn range_lens_inner<F>(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        filter: Option<&Expr>,
        mut each: F,
    ) -> Result<(), ExecError>
    where
        F: FnMut(Timestamp, Timestamp, &Value),
    {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let state = self.active_state()?;
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        let mut bounds = vec![start, end];
        collect_lens_bounds(state, name, start, end, &mut bounds)?;
        if let Some(f) = filter {
            collect_expr_bounds(state, f, start, end, &mut bounds)?;
        }
        bounds.sort();
        bounds.dedup();

        for w in bounds.windows(2) {
            let (s, e) = (w[0], w[1]);
            let v = match eval_lens(state, name, s)? {
                Some(v) => v,
                None => continue,
            };
            if let Some(f) = filter {
                match eval_expr(state, f, s)? {
                    Some(Value::Bool(true)) => {}
                    _ => continue,
                }
            }
            each(s, e, &v);
        }
        Ok(())
    }

    fn reduce_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        func: AggFn,
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }

        let mut count: i64 = 0;
        let mut sum_i: i64 = 0;
        let mut sum_f: f64 = 0.0;
        let mut is_float = false;
        let mut min_val: Option<Value> = None;
        let mut max_val: Option<Value> = None;

        self.range_lens_inner(name, start, end, None, |_s, _e, v| {
            count += 1;
            match v {
                Value::Int(i) => {
                    sum_i = sum_i.wrapping_add(*i);
                    sum_f += *i as f64;
                }
                Value::Float(f) => {
                    is_float = true;
                    sum_f += *f;
                }
                _ => {}
            }
            // Track min/max via numeric promotion when possible; otherwise
            // fall back to variant equality.
            if let Some(cur) = as_f64(v) {
                let cur_min = min_val.as_ref().and_then(as_f64);
                if cur_min.map(|m| cur < m).unwrap_or(true) {
                    min_val = Some(v.clone());
                }
                let cur_max = max_val.as_ref().and_then(as_f64);
                if cur_max.map(|m| cur > m).unwrap_or(true) {
                    max_val = Some(v.clone());
                }
            }
        })?;

        if count == 0 {
            return Ok(Output::Scalar(None));
        }

        let result = match func {
            AggFn::Count => Value::Int(count),
            AggFn::Sum => {
                if is_float {
                    Value::Float(sum_f)
                } else {
                    Value::Int(sum_i)
                }
            }
            AggFn::Avg => Value::Float(sum_f / count as f64),
            AggFn::Min => min_val.unwrap_or(Value::Null),
            AggFn::Max => max_val.unwrap_or(Value::Null),
        };
        Ok(Output::Scalar(Some(result)))
    }

    fn compact_lens(&mut self, name: &str) -> Result<Output, ExecError> {
        let state = self.active_mut()?;
        if !state.base_types.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        let layers = match state.db.layers(name) {
            Some(l) if l.len() <= 1 => return Ok(Output::Empty),
            Some(l) => l,
            None => return Ok(Output::Empty),
        };

        // Gather all change-point boundaries plus the global min/max.
        let mut boundaries: Vec<Timestamp> = Vec::new();
        let mut global_min = i64::MAX;
        let mut global_max = i64::MIN;
        for layer in &layers {
            if layer.min_ts < global_min {
                global_min = layer.min_ts;
            }
            if layer.max_ts > global_max {
                global_max = layer.max_ts;
            }
            for tau in layer.taus.iter() {
                boundaries.push(tau.start);
                boundaries.push(tau.end);
            }
        }
        boundaries.sort();
        boundaries.dedup();
        if boundaries.len() < 2 {
            return Ok(Output::Empty);
        }

        // Walk windows newest-wins; coalesce adjacent equal-valued spans.
        let mut merged: Vec<Tau<Value>> = Vec::new();
        for w in boundaries.windows(2) {
            let (s, e) = (w[0], w[1]);
            let v = layers
                .iter()
                .rev()
                .filter(|l| l.min_ts <= s && s < l.max_ts)
                .find_map(|l| l.at(s));
            let Some(v) = v else { continue };
            match merged.last_mut() {
                Some(prev) if prev.end == s && prev.value == *v => prev.end = e,
                _ => merged.push(Tau::new(s, e, v.clone())),
            }
        }

        if merged.is_empty() {
            return Ok(Output::Empty);
        }

        let new_id = state.next_layer_id;
        state.next_layer_id += 1;
        let new_layer = Layer::new(new_id, merged);
        state.db.replace_layers(name, new_layer);
        self.persist_ddl(&Stmt::Compact { name: name.into() })?;
        Ok(Output::Empty)
    }

    /// Persist a DDL statement to the shared WAL.  No-op when no WAL is
    /// attached or when the executor is currently replaying its own log.
    fn persist_ddl(&self, stmt: &Stmt) -> Result<(), ExecError> {
        if self.replay_in_progress {
            return Ok(());
        }
        let Some(wal) = &self.wal else { return Ok(()) };
        let Some(text) = stmt.to_wal_string() else {
            return Ok(());
        };
        wal.push_stmt(&text)
            .wait()
            .map_err(|e| ExecError::InvalidExpr(format!("WAL persist failed: {e}")))
    }

    fn drop_lens(&mut self, name: &str) -> Result<Output, ExecError> {
        let state = self.active_mut()?;
        if state.base_types.remove(name).is_some() || state.derived.remove(name).is_some() {
            self.persist_ddl(&Stmt::Drop { name: name.into() })?;
            Ok(Output::Empty)
        } else {
            Err(ExecError::UnknownLens(name.into()))
        }
    }

    // ---- helpers ------------------------------------------------------------

    fn active_state(&self) -> Result<&DbState, ExecError> {
        let name = self.active.as_deref().ok_or(ExecError::NoActiveDatabase)?;
        self.databases
            .get(name)
            .ok_or_else(|| ExecError::UnknownDatabase(name.into()))
    }

    fn active_mut(&mut self) -> Result<&mut DbState, ExecError> {
        let name = self
            .active
            .as_deref()
            .ok_or(ExecError::NoActiveDatabase)?
            .to_string();
        self.databases
            .get_mut(&name)
            .ok_or(ExecError::UnknownDatabase(name))
    }
}

// ---- WAL replay -------------------------------------------------------------

/// Free-function replay used by [`Executor::with_wal`].  Walks every line of
/// the WAL file, distinguishes `STMT` lines (DDL) from data lines, and
/// applies them to `exec` without re-persisting (the executor flips its
/// `replay_in_progress` flag for the duration of the call).
pub(crate) fn replay_wal_file(path: &std::path::Path, exec: &mut Executor) -> std::io::Result<()> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((checksum_str, payload)) = line.split_once(' ') else {
            continue;
        };
        let Ok(checksum) = checksum_str.parse::<u32>() else {
            continue;
        };
        if checksum != crate::libtau::storage::wal::crc32(payload) {
            continue;
        }
        if let Some(stmt_text) = payload.strip_prefix("STMT ") {
            // Re-parse and execute.  Duplicate-DB / -lens errors during
            // replay are tolerated because the surviving log may include
            // repeated DDL.
            let Ok((rest, stmt)) = parse(stmt_text) else {
                continue;
            };
            if !rest.trim().is_empty() {
                continue;
            }
            let _ = exec.exec(&stmt);
        } else if let Some(entry) = WalEntry::<Value>::deserialise(line) {
            let db_name = exec.active.clone();
            if let Some(name) = db_name
                && let Some(state) = exec.databases.get_mut(&name)
            {
                state.apply_data_entry(entry);
            }
        }
    }
    Ok(())
}

// ---- lens evaluation --------------------------------------------------------

fn eval_lens(state: &DbState, name: &str, t: Timestamp) -> Result<Option<Value>, ExecError> {
    if state.base_types.contains_key(name) {
        Ok(state.db.at(&state.db.lens(name), t))
    } else if let Some(expr) = state.derived.get(name) {
        eval_expr(state, expr, t)
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

fn eval_expr(state: &DbState, expr: &Expr, t: Timestamp) -> Result<Option<Value>, ExecError> {
    match expr {
        Expr::Lit(l) => Ok(Some(l.clone().into())),
        Expr::Ident(name) => eval_lens(state, name, t),
        Expr::Unary { op, expr } => match eval_expr(state, expr, t)? {
            None => Ok(None),
            Some(v) => apply_unary(*op, v).map(Some),
        },
        Expr::Binary { op, lhs, rhs } => {
            let l = eval_expr(state, lhs, t)?;
            let r = eval_expr(state, rhs, t)?;
            match (l, r) {
                (Some(a), Some(b)) => apply_binary(*op, a, b).map(Some),
                _ => Ok(None),
            }
        }
    }
}

fn apply_unary(op: UnOp, v: Value) -> Result<Value, ExecError> {
    match (op, v) {
        (UnOp::Neg, Value::Int(i)) => Ok(Value::Int(-i)),
        (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (op, v) => Err(ExecError::InvalidExpr(format!(
            "cannot apply {op:?} to {}",
            v.type_name()
        ))),
    }
}

/// Coerce an `Int`/`Float` pair to a common `f64` representation; returns
/// `None` if either side isn't numeric.
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn apply_binary(op: BinOp, a: Value, b: Value) -> Result<Value, ExecError> {
    use BinOp::*;
    // Logical: both operands must be bool.
    match op {
        And | Or => {
            return match (a, b) {
                (Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(match op {
                    And => x && y,
                    Or => x || y,
                    _ => unreachable!(),
                })),
                (x, y) => Err(ExecError::InvalidExpr(format!(
                    "logical {op:?} requires bool/bool, got {}/{}",
                    x.type_name(),
                    y.type_name()
                ))),
            };
        }
        _ => {}
    }

    // Equality is permitted across any matching variants (and either pair
    // of numerics after promotion).
    if matches!(op, Eq | NotEq) {
        let eq = values_equal(&a, &b)?;
        return Ok(Value::Bool(if op == Eq { eq } else { !eq }));
    }

    // Ordering and arithmetic: numeric only.  Integer fast-path, else f64.
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        return Ok(match op {
            Add => Value::Int(x.wrapping_add(*y)),
            Sub => Value::Int(x.wrapping_sub(*y)),
            Mul => Value::Int(x.wrapping_mul(*y)),
            Div => {
                if *y == 0 {
                    return Err(ExecError::InvalidExpr("divide by zero".into()));
                }
                Value::Int(x / y)
            }
            Mod => {
                if *y == 0 {
                    return Err(ExecError::InvalidExpr("modulo by zero".into()));
                }
                Value::Int(x % y)
            }
            Lt => Value::Bool(x < y),
            LtEq => Value::Bool(x <= y),
            Gt => Value::Bool(x > y),
            GtEq => Value::Bool(x >= y),
            _ => unreachable!(),
        });
    }

    let (Some(x), Some(y)) = (as_f64(&a), as_f64(&b)) else {
        return Err(ExecError::InvalidExpr(format!(
            "operator {op:?} requires numeric operands, got {}/{}",
            a.type_name(),
            b.type_name()
        )));
    };
    Ok(match op {
        Add => Value::Float(x + y),
        Sub => Value::Float(x - y),
        Mul => Value::Float(x * y),
        Div => {
            if y == 0.0 {
                return Err(ExecError::InvalidExpr("divide by zero".into()));
            }
            Value::Float(x / y)
        }
        Mod => {
            if y == 0.0 {
                return Err(ExecError::InvalidExpr("modulo by zero".into()));
            }
            Value::Float(x % y)
        }
        Lt => Value::Bool(x < y),
        LtEq => Value::Bool(x <= y),
        Gt => Value::Bool(x > y),
        GtEq => Value::Bool(x >= y),
        _ => unreachable!(),
    })
}

/// Strict variant equality plus numeric promotion.  Returns `Err` only for
/// genuinely incomparable variants (so the executor can flag bad queries).
fn values_equal(a: &Value, b: &Value) -> Result<bool, ExecError> {
    use Value::*;
    match (a, b) {
        (Null, Null) => Ok(true),
        (Null, _) | (_, Null) => Ok(false),
        (Bool(x), Bool(y)) => Ok(x == y),
        (Str(x), Str(y)) => Ok(x == y),
        (Int(_), Int(_)) | (Int(_), Float(_)) | (Float(_), Int(_)) | (Float(_), Float(_)) => {
            Ok(as_f64(a).unwrap() == as_f64(b).unwrap())
        }
        (x, y) => Err(ExecError::InvalidExpr(format!(
            "cannot compare {} with {}",
            x.type_name(),
            y.type_name()
        ))),
    }
}

// ---- boundary collection (for RANGE) ----------------------------------------

fn collect_lens_bounds(
    state: &DbState,
    name: &str,
    start: Timestamp,
    end: Timestamp,
    out: &mut Vec<Timestamp>,
) -> Result<(), ExecError> {
    if state.base_types.contains_key(name) {
        if let Some(layers) = state.db.layers(name) {
            for layer in layers {
                // Skip layers entirely outside the [start, end) query window.
                if layer.max_ts <= start || layer.min_ts >= end {
                    continue;
                }
                for tau in layer.taus.iter() {
                    if tau.start > start && tau.start < end {
                        out.push(tau.start);
                    }
                    if tau.end > start && tau.end < end {
                        out.push(tau.end);
                    }
                }
            }
        }
        Ok(())
    } else if let Some(expr) = state.derived.get(name) {
        // Clone to release the borrow before recursing into other lenses.
        let expr = expr.clone();
        collect_expr_bounds(state, &expr, start, end, out)
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

fn collect_expr_bounds(
    state: &DbState,
    expr: &Expr,
    start: Timestamp,
    end: Timestamp,
    out: &mut Vec<Timestamp>,
) -> Result<(), ExecError> {
    match expr {
        Expr::Lit(_) => Ok(()),
        Expr::Ident(name) => collect_lens_bounds(state, name, start, end, out),
        Expr::Unary { expr, .. } => collect_expr_bounds(state, expr, start, end, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr_bounds(state, lhs, start, end, out)?;
            collect_expr_bounds(state, rhs, start, end, out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libtau::ql::parse;

    /// Parse + run.  Panics on parse failure; returns `Result` on exec.
    fn run(exec: &mut Executor, q: &str) -> Result<Output, ExecError> {
        let (rest, stmt) = parse(q).expect("parse failed");
        assert!(rest.is_empty(), "unconsumed: {rest:?}");
        exec.exec(&stmt)
    }

    fn setup() -> Executor {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE main").unwrap();
        e
    }

    // ---- database management ------------------------------------------------

    #[test]
    fn create_database_sets_active_on_first_create() {
        let mut e = Executor::new();
        assert_eq!(e.active(), None);
        run(&mut e, "CREATE DATABASE a").unwrap();
        assert_eq!(e.active(), Some("a"));
    }

    #[test]
    fn second_create_does_not_change_active() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE a").unwrap();
        run(&mut e, "CREATE DATABASE b").unwrap();
        assert_eq!(e.active(), Some("a"));
    }

    #[test]
    fn create_duplicate_database_errors() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE a").unwrap();
        assert_eq!(
            run(&mut e, "CREATE DATABASE a"),
            Err(ExecError::DuplicateDatabase("a".into()))
        );
    }

    #[test]
    fn use_unknown_database_errors() {
        let mut e = Executor::new();
        assert_eq!(
            run(&mut e, "USE DATABASE ghost"),
            Err(ExecError::UnknownDatabase("ghost".into()))
        );
    }

    #[test]
    fn use_switches_active() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE a").unwrap();
        run(&mut e, "CREATE DATABASE b").unwrap();
        run(&mut e, "USE DATABASE b").unwrap();
        assert_eq!(e.active(), Some("b"));
    }

    #[test]
    fn drop_active_database_clears_active() {
        let mut e = setup();
        run(&mut e, "DROP DATABASE main").unwrap();
        assert_eq!(e.active(), None);
        assert_eq!(
            run(&mut e, "CREATE LENS x int"),
            Err(ExecError::NoActiveDatabase)
        );
    }

    #[test]
    fn drop_unknown_database_errors() {
        let mut e = Executor::new();
        assert_eq!(
            run(&mut e, "DROP DATABASE ghost"),
            Err(ExecError::UnknownDatabase("ghost".into()))
        );
    }

    // ---- lens DDL -----------------------------------------------------------

    #[test]
    fn create_lens_without_active_database_errors() {
        let mut e = Executor::new();
        assert_eq!(
            run(&mut e, "CREATE LENS x int"),
            Err(ExecError::NoActiveDatabase)
        );
    }

    #[test]
    fn create_duplicate_lens_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "CREATE LENS x int"),
            Err(ExecError::DuplicateLens("x".into()))
        );
    }

    #[test]
    fn drop_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "DROP LENS missing"),
            Err(ExecError::UnknownLens("missing".into()))
        );
    }

    // ---- append + type enforcement ------------------------------------------

    #[test]
    fn append_to_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "APPEND LENS x 0 10 1"),
            Err(ExecError::UnknownLens("x".into()))
        );
    }

    #[test]
    fn append_type_mismatch_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "APPEND LENS x 0 10 1.5"),
            Err(ExecError::TypeMismatch {
                lens: "x".into(),
                expected: Type::Int,
                got: "float".into(),
            })
        );
    }

    #[test]
    fn append_null_is_permitted_for_any_type() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 null").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Null))
        );
    }

    #[test]
    fn append_with_inverted_range_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "APPEND LENS x 10 5 1"),
            Err(ExecError::InvalidRange)
        );
    }

    // ---- AT lookup ----------------------------------------------------------

    #[test]
    fn at_returns_none_for_uncovered_time() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        assert_eq!(run(&mut e, "AT LENS x 50").unwrap(), Output::Value(None));
    }

    #[test]
    fn at_returns_value_in_range() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
    }

    #[test]
    fn at_observes_newest_layer() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 20 1").unwrap();
        run(&mut e, "APPEND LENS x 5 15 2").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 3").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 10").unwrap(),
            Output::Value(Some(Value::Int(2)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 17").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
    }

    // ---- DERIVE -------------------------------------------------------------

    #[test]
    fn derive_simple_arithmetic() {
        let mut e = setup();
        run(&mut e, "CREATE LENS c int").unwrap();
        run(&mut e, "APPEND LENS c 0 100 10").unwrap();
        run(&mut e, "DERIVE LENS doubled AS c * 2").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS doubled 50").unwrap(),
            Output::Value(Some(Value::Int(20)))
        );
    }

    #[test]
    fn derive_celsius_to_fahrenheit_float() {
        let mut e = setup();
        run(&mut e, "CREATE LENS c float").unwrap();
        run(&mut e, "APPEND LENS c 0 100 18.0").unwrap();
        run(&mut e, "DERIVE LENS f AS c * 9.0 / 5.0 + 32.0").unwrap();
        let Output::Value(Some(Value::Float(v))) = run(&mut e, "AT LENS f 50").unwrap() else {
            panic!("expected float");
        };
        assert!((v - 64.4).abs() < 1e-9);
    }

    #[test]
    fn derive_changes_type_from_int_to_bool() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 100 5").unwrap();
        run(&mut e, "DERIVE LENS big AS x > 3").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS big 10").unwrap(),
            Output::Value(Some(Value::Bool(true)))
        );
    }

    #[test]
    fn derive_returns_none_when_source_uncovered() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();
        run(&mut e, "DERIVE LENS d AS x + 1").unwrap();
        assert_eq!(run(&mut e, "AT LENS d 50").unwrap(), Output::Value(None));
    }

    #[test]
    fn derive_chained() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 5").unwrap();
        run(&mut e, "DERIVE LENS y AS x * 2").unwrap();
        run(&mut e, "DERIVE LENS z AS y + 1").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS z 5").unwrap(),
            Output::Value(Some(Value::Int(11)))
        );
    }

    #[test]
    fn derive_unknown_ident_errors_at_query_time() {
        let mut e = setup();
        run(&mut e, "DERIVE LENS d AS ghost + 1").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS d 0"),
            Err(ExecError::UnknownLens("ghost".into()))
        );
    }

    #[test]
    fn divide_by_zero_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();
        run(&mut e, "DERIVE LENS d AS x / 0").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS d 5"),
            Err(ExecError::InvalidExpr("divide by zero".into()))
        );
    }

    // ---- RANGE --------------------------------------------------------------

    #[test]
    fn range_returns_segments_split_at_change_points() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 0 10").unwrap(),
            Output::Range(vec![(0, 5, Value::Int(1)), (5, 10, Value::Int(2))])
        );
    }

    #[test]
    fn range_merges_adjacent_equal_values() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 7").unwrap();
        run(&mut e, "APPEND LENS x 5 10 7").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 0 10").unwrap(),
            Output::Range(vec![(0, 10, Value::Int(7))])
        );
    }

    #[test]
    fn range_skips_gaps() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 8 10 2").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 0 10").unwrap(),
            Output::Range(vec![(0, 5, Value::Int(1)), (8, 10, Value::Int(2))])
        );
    }

    #[test]
    fn range_clips_to_query_window() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 100 9").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 10 20").unwrap(),
            Output::Range(vec![(10, 20, Value::Int(9))])
        );
    }

    #[test]
    fn range_with_where_filter() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 50").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 0 10 WHERE x > 10").unwrap(),
            Output::Range(vec![(5, 10, Value::Int(50))])
        );
    }

    #[test]
    fn range_on_derived_lens() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        run(&mut e, "DERIVE LENS y AS x * 10").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS y 0 10").unwrap(),
            Output::Range(vec![(0, 5, Value::Int(10)), (5, 10, Value::Int(20))])
        );
    }

    #[test]
    fn range_inverted_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 10 5"),
            Err(ExecError::InvalidRange)
        );
    }

    #[test]
    fn range_on_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "RANGE LENS ghost 0 10"),
            Err(ExecError::UnknownLens("ghost".into()))
        );
    }

    // ---- isolation between databases ---------------------------------------

    #[test]
    fn lenses_are_isolated_per_database() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE a").unwrap();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();

        run(&mut e, "CREATE DATABASE b").unwrap();
        run(&mut e, "USE DATABASE b").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5"),
            Err(ExecError::UnknownLens("x".into()))
        );

        run(&mut e, "USE DATABASE a").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
    }

    // ---- COMPACT ------------------------------------------------------------

    #[test]
    fn compact_collapses_overlapping_layers() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 20 1").unwrap();
        run(&mut e, "APPEND LENS x 5 15 2").unwrap();
        // Confirm pre-compact reads.
        assert_eq!(
            run(&mut e, "AT LENS x 7").unwrap(),
            Output::Value(Some(Value::Int(2)))
        );
        run(&mut e, "COMPACT LENS x").unwrap();
        // Reads must be unchanged after compaction.
        assert_eq!(
            run(&mut e, "AT LENS x 3").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 10").unwrap(),
            Output::Value(Some(Value::Int(2)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 17").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
        // Single layer left.
        let state = e.active_state().unwrap();
        let layers = state.db.layers("x").unwrap();
        assert_eq!(layers.len(), 1);
    }

    #[test]
    fn compact_noop_on_single_layer() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();
        run(&mut e, "COMPACT LENS x").unwrap();
        let layers = e.active_state().unwrap().db.layers("x").unwrap();
        assert_eq!(layers.len(), 1);
    }

    #[test]
    fn compact_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "COMPACT LENS ghost"),
            Err(ExecError::UnknownLens("ghost".into()))
        );
    }

    // ---- REDUCE -------------------------------------------------------------

    #[test]
    fn reduce_sum_integer() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 10").unwrap();
        run(&mut e, "APPEND LENS x 5 10 20").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 sum").unwrap(),
            Output::Scalar(Some(Value::Int(30)))
        );
    }

    #[test]
    fn reduce_count_returns_segment_count() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 count").unwrap(),
            Output::Scalar(Some(Value::Int(2)))
        );
    }

    #[test]
    fn reduce_avg_min_max() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 4").unwrap();
        run(&mut e, "APPEND LENS x 5 10 8").unwrap();
        let Output::Scalar(Some(Value::Float(avg))) =
            run(&mut e, "REDUCE LENS x 0 10 avg").unwrap()
        else {
            panic!("expected float");
        };
        assert!((avg - 6.0).abs() < 1e-9);
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 min").unwrap(),
            Output::Scalar(Some(Value::Int(4)))
        );
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 max").unwrap(),
            Output::Scalar(Some(Value::Int(8)))
        );
    }

    #[test]
    fn reduce_empty_range_returns_nil() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 sum").unwrap(),
            Output::Scalar(None)
        );
    }

    // ---- Streaming RANGE callback ------------------------------------------

    #[test]
    fn exec_range_cb_emits_same_segments_as_range() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();

        let Output::Range(expected) = run(&mut e, "RANGE LENS x 0 10").unwrap() else {
            panic!("expected range");
        };

        let mut got: Vec<(i64, i64, Value)> = Vec::new();
        let count = e
            .exec_range_cb("x", 0, 10, None, |s, en, v| got.push((s, en, v.clone())))
            .unwrap();
        assert_eq!(count, expected.len());
        assert_eq!(got, expected);
    }

    // ---- Schema WAL ---------------------------------------------------------

    #[test]
    fn schema_survives_executor_restart() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp); // we only want a unique path; the file will be created

        {
            let mut e = Executor::with_wal(&path).unwrap();
            // The default DB is pre-created.  Schema + data:
            e.exec(&parse("CREATE LENS celsius float").unwrap().1)
                .unwrap();
            e.exec(
                &parse("DERIVE LENS f AS celsius * 9.0 / 5.0 + 32.0")
                    .unwrap()
                    .1,
            )
            .unwrap();
            e.exec(&parse("APPEND LENS celsius 0 100 18.0").unwrap().1)
                .unwrap();
        }

        // Restart against the same WAL: schema and data must be reconstructed.
        let mut e = Executor::with_wal(&path).unwrap();
        let Output::Value(Some(Value::Float(v))) =
            e.exec(&parse("AT LENS f 50").unwrap().1).unwrap()
        else {
            panic!("expected float");
        };
        assert!((v - 64.4).abs() < 1e-9);

        let _ = std::fs::remove_file(&path);
    }
}
