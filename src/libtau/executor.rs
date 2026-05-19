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
//! DDL (`CREATE`/`DROP`/`USE`/`APPEND`/`DERIVE`) returns [`Output::Empty`].
//! `AT` returns [`Output::Value`] (`None` when no tau covers `t`).
//! `RANGE` returns [`Output::Range`] — a vec of `(start, end, value)` segments.
//! `REDUCE` returns [`Output::Value`] — a single scalar aggregate.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::libtau::database::Database;
use crate::libtau::model::{Layer, Tau, Timestamp};
use crate::libtau::ql::ast::{AggFunc, BinOp, Expr, Stmt, Type, UnOp};
use crate::libtau::storage::InMemory;
use crate::libtau::value::Value;

/// Output of a single executed statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Statement produced no value (DDL / writes).
    Empty,
    /// Point lookup result.  `None` means the lens has no value at that time.
    Value(Option<Value>),
    /// Sequence of `(start, end, value)` segments covering the queried range.
    Range(Vec<(Timestamp, Timestamp, Value)>),
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
    fn new(compact_threshold: usize) -> Self {
        Self {
            db: Database::new(InMemory::<Value>::with_threshold(compact_threshold)),
            base_types: HashMap::new(),
            next_layer_id: 1,
            derived: HashMap::new(),
        }
    }

    fn with_wal(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        key: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        let store = InMemory::<Value>::with_threshold(compact_threshold);
        // TODO: replay must also reconstruct base_types and derived maps from
        // persisted CREATE LENS / DERIVE LENS events so lens declarations
        // survive a restart. Currently only data is replayed.
        let db = Database::open(store, path, key).map_err(io::Error::other)?;
        Ok(Self {
            db,
            base_types: HashMap::new(),
            next_layer_id: 1,
            derived: HashMap::new(),
        })
    }
}

/// Runtime container for executing parsed [`Stmt`]s.
pub struct Executor {
    databases: HashMap<String, DbState>,
    /// Name of the currently active database (set by the first
    /// `CREATE DATABASE` and by `USE DATABASE`).  Cleared if the active
    /// database is dropped.
    active: Option<String>,
    compact_threshold: usize,
}

impl Default for Executor {
    fn default() -> Self {
        Self::with_threshold(crate::libtau::storage::COMPACT_THRESHOLD)
    }
}

impl Executor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an executor with a custom layer compaction threshold.
    pub fn with_threshold(compact_threshold: usize) -> Self {
        Self {
            databases: HashMap::new(),
            active: None,
            compact_threshold,
        }
    }

    /// Open a WAL-backed executor with the default compaction threshold.
    pub fn with_wal(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        Self::with_wal_threshold(path, crate::libtau::storage::COMPACT_THRESHOLD, key)
    }

    /// Open a WAL-backed executor with a custom compaction threshold.
    pub fn with_wal_threshold(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        key: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        let mut executor = Self::with_threshold(compact_threshold);
        let db_state = DbState::with_wal(path, compact_threshold, key)?;
        executor.databases.insert("default".to_string(), db_state);
        executor.active = Some("default".to_string());
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
            Stmt::Reduce {
                name,
                start,
                end,
                func,
            } => self.reduce_lens(name, *start, *end, *func),
        }
    }

    fn create_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if self.databases.contains_key(name) {
            return Err(ExecError::DuplicateDatabase(name.into()));
        }
        self.databases
            .insert(name.into(), DbState::new(self.compact_threshold));
        // First database created becomes active by convention.
        if self.active.is_none() {
            self.active = Some(name.into());
        }
        Ok(Output::Empty)
    }

    fn drop_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if self.databases.remove(name).is_none() {
            return Err(ExecError::UnknownDatabase(name.into()));
        }
        if self.active.as_deref() == Some(name) {
            self.active = None;
        }
        Ok(Output::Empty)
    }

    fn use_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if !self.databases.contains_key(name) {
            return Err(ExecError::UnknownDatabase(name.into()));
        }
        self.active = Some(name.into());
        Ok(Output::Empty)
    }

    fn create_lens(&mut self, name: &str, ty: Type) -> Result<Output, ExecError> {
        let state = self.active_mut()?;
        if state.base_types.contains_key(name) || state.derived.contains_key(name) {
            return Err(ExecError::DuplicateLens(name.into()));
        }
        state.base_types.insert(name.into(), ty);
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
        // TODO: detect cycles before inserting — a derived lens that references
        // itself (directly or via a chain) will stack-overflow at query time.
        state.derived.insert(name.into(), expr);
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
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let state = self.active_state()?;
        // Confirm the lens exists up front so an empty range still errors.
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

        let mut out: Vec<(Timestamp, Timestamp, Value)> = Vec::new();
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
            // Merge with previous segment if it's adjacent and same value.
            match out.last_mut() {
                Some(last) if last.1 == s && last.2 == v => last.1 = e,
                _ => out.push((s, e, v)),
            }
        }
        Ok(Output::Range(out))
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
        let state = self.active_state()?;
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        eval_agg(state, name, func, start, end).map(Output::Value)
    }

    fn drop_lens(&mut self, name: &str) -> Result<Output, ExecError> {
        let state = self.active_mut()?;
        if state.base_types.remove(name).is_some() || state.derived.remove(name).is_some() {
            Ok(Output::Empty)
        } else {
            Err(ExecError::UnknownLens(name.into()))
        }
    }

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
        Expr::Agg {
            func,
            lens,
            rel_start,
            rel_end,
        } => {
            let abs_start = t + rel_start;
            let abs_end = t + rel_end;
            if abs_start >= abs_end {
                return Ok(None);
            }
            eval_agg(state, lens, *func, abs_start, abs_end)
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
    if matches!(op, And | Or) {
        return match (a, b) {
            (Value::Bool(x), Value::Bool(y)) => {
                Ok(Value::Bool(if op == And { x && y } else { x || y }))
            }
            (x, y) => Err(ExecError::InvalidExpr(format!(
                "logical {op:?} requires bool/bool, got {}/{}",
                x.type_name(),
                y.type_name()
            ))),
        };
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
        Expr::Agg {
            lens,
            rel_start,
            rel_end,
            ..
        } => {
            // The aggregate's value changes when a boundary of the underlying
            // lens enters or exits the sliding window [t+rel_start, t+rel_end).
            // A lens boundary at position p causes a change at t = p - rel_start
            // (enters) and t = p - rel_end (exits), so collect over the wider
            // underlying range and project back.
            let wstart = start.saturating_add(*rel_start).min(start);
            let wend = end.saturating_add(*rel_end).max(end);
            let mut inner = Vec::new();
            collect_lens_bounds(state, lens, wstart, wend, &mut inner)?;
            for p in inner {
                for shift in [*rel_start, *rel_end] {
                    let t_change = p - shift;
                    if t_change > start && t_change < end {
                        out.push(t_change);
                    }
                }
            }
            Ok(())
        }
    }
}

fn eval_agg(
    state: &DbState,
    lens: &str,
    func: AggFunc,
    start: Timestamp,
    end: Timestamp,
) -> Result<Option<Value>, ExecError> {
    let mut bounds = vec![start, end];
    collect_lens_bounds(state, lens, start, end, &mut bounds)?;
    bounds.sort();
    bounds.dedup();

    let mut segments: Vec<(i64, Value)> = Vec::new();
    for w in bounds.windows(2) {
        let (s, e) = (w[0], w[1]);
        if let Some(v) = eval_lens(state, lens, s)? {
            segments.push((e - s, v));
        }
    }

    if segments.is_empty() {
        return Ok(None);
    }

    Ok(Some(match func {
        AggFunc::Count => Value::Int(segments.len() as i64),
        AggFunc::Min => segments
            .into_iter()
            .map(|(_, v)| v)
            .try_fold(None::<Value>, |acc, v| match acc {
                None => Ok(Some(v)),
                Some(a) => numeric_min_max(a, v, false).map(Some),
            })?
            .unwrap(),
        AggFunc::Max => segments
            .into_iter()
            .map(|(_, v)| v)
            .try_fold(None::<Value>, |acc, v| match acc {
                None => Ok(Some(v)),
                Some(a) => numeric_min_max(a, v, true).map(Some),
            })?
            .unwrap(),
        AggFunc::Sum => {
            let mut int_sum: i64 = 0;
            let mut float_sum: Option<f64> = None;
            for (_, v) in &segments {
                match v {
                    Value::Int(i) => match &mut float_sum {
                        Some(f) => *f += *i as f64,
                        None => int_sum = int_sum.wrapping_add(*i),
                    },
                    Value::Float(f) => {
                        *float_sum.get_or_insert(int_sum as f64) += f;
                    }
                    _ => {
                        return Err(ExecError::InvalidExpr(format!(
                            "sum requires numeric values, got {}",
                            v.type_name()
                        )));
                    }
                }
            }
            float_sum.map(Value::Float).unwrap_or(Value::Int(int_sum))
        }
        AggFunc::Avg => {
            let total: i64 = segments.iter().map(|(d, _)| *d).sum();
            if total == 0 {
                return Ok(None);
            }
            let mut weighted = 0.0f64;
            for (d, v) in &segments {
                match v {
                    Value::Int(i) => weighted += *i as f64 * *d as f64,
                    Value::Float(f) => weighted += f * *d as f64,
                    _ => {
                        return Err(ExecError::InvalidExpr(format!(
                            "avg requires numeric values, got {}",
                            v.type_name()
                        )));
                    }
                }
            }
            Value::Float(weighted / total as f64)
        }
    }))
}

fn numeric_min_max(a: Value, b: Value, want_max: bool) -> Result<Value, ExecError> {
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(if want_max {
            (*x).max(*y)
        } else {
            (*x).min(*y)
        })),
        _ => match (as_f64(&a), as_f64(&b)) {
            (Some(x), Some(y)) => Ok(Value::Float(if want_max { x.max(y) } else { x.min(y) })),
            _ => Err(ExecError::InvalidExpr(format!(
                "min/max requires numeric values, got {}/{}",
                a.type_name(),
                b.type_name()
            ))),
        },
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

    #[test]
    fn reduce_count_segments() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 USING count").unwrap(),
            Output::Value(Some(Value::Int(2)))
        );
    }

    #[test]
    fn reduce_sum_integers() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 3").unwrap();
        run(&mut e, "APPEND LENS x 5 10 7").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 USING sum").unwrap(),
            Output::Value(Some(Value::Int(10)))
        );
    }

    #[test]
    fn reduce_min_max() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 3").unwrap();
        run(&mut e, "APPEND LENS x 5 10 7").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 USING min").unwrap(),
            Output::Value(Some(Value::Int(3)))
        );
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 USING max").unwrap(),
            Output::Value(Some(Value::Int(7)))
        );
    }

    #[test]
    fn reduce_avg_time_weighted() {
        // Two equal-duration segments: avg = (3+7)/2 = 5.0
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 3").unwrap();
        run(&mut e, "APPEND LENS x 5 10 7").unwrap();
        let Output::Value(Some(Value::Float(v))) =
            run(&mut e, "REDUCE LENS x 0 10 USING avg").unwrap()
        else {
            panic!("expected float");
        };
        assert!((v - 5.0).abs() < 1e-9);
    }

    #[test]
    fn reduce_avg_weighted_by_duration() {
        // [0,1) = 1, [1,10) = 10  →  weighted avg = (1*1 + 9*10) / 10 = 91/10 = 9.1
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 1 1").unwrap();
        run(&mut e, "APPEND LENS x 1 10 10").unwrap();
        let Output::Value(Some(Value::Float(v))) =
            run(&mut e, "REDUCE LENS x 0 10 USING avg").unwrap()
        else {
            panic!("expected float");
        };
        assert!((v - 9.1).abs() < 1e-9);
    }

    #[test]
    fn reduce_returns_none_for_uncovered_range() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 10 20 USING avg").unwrap(),
            Output::Value(None)
        );
    }

    #[test]
    fn reduce_inverted_range_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 10 5 USING min"),
            Err(ExecError::InvalidRange)
        );
    }

    #[test]
    fn reduce_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "REDUCE LENS ghost 0 10 USING avg"),
            Err(ExecError::UnknownLens("ghost".into()))
        );
    }

    #[test]
    fn derive_with_rolling_avg() {
        // avg(x, -10, 0) at t=10 covers [0,10): values [0,5)=1 and [5,10)=2
        // time-weighted avg = (5*1 + 5*2) / 10 = 1.5
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        run(&mut e, "DERIVE LENS smooth AS avg(x, -10, 0)").unwrap();
        let Output::Value(Some(Value::Float(v))) = run(&mut e, "AT LENS smooth 10").unwrap() else {
            panic!("expected float");
        };
        assert!((v - 1.5).abs() < 1e-9);
    }

    #[test]
    fn derive_with_rolling_min() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 10").unwrap();
        run(&mut e, "APPEND LENS x 5 10 3").unwrap();
        run(&mut e, "DERIVE LENS lo AS min(x, -10, 0)").unwrap();
        // at t=10 window covers [0,10): min of 10 and 3 = 3
        assert_eq!(
            run(&mut e, "AT LENS lo 10").unwrap(),
            Output::Value(Some(Value::Int(3)))
        );
    }

    #[test]
    fn derive_agg_in_comparison() {
        // hot = x > avg(x, -10, 0): true when current value exceeds rolling avg
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        run(&mut e, "DERIVE LENS hot AS x > avg(x, -10, 0)").unwrap();
        // at t=7 x=2, avg([−3,7)) = avg([2,7): only [5,7)=2 → avg=2.0; 2>2 = false
        // at t=8 x=2, avg([−2,8)) = avg([6,8): value=2 → avg=2.0; 2>2.0 = false
        // Let's just check a known case: at t=5 window is [-5,5): only [0,5)=1 → avg=1.0; x(5)=2 > 1.0 = true
        assert_eq!(
            run(&mut e, "AT LENS hot 5").unwrap(),
            Output::Value(Some(Value::Bool(true)))
        );
    }

    #[test]
    fn reduce_on_derived_lens() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 2").unwrap();
        run(&mut e, "APPEND LENS x 5 10 4").unwrap();
        run(&mut e, "DERIVE LENS doubled AS x * 2").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS doubled 0 10 USING sum").unwrap(),
            Output::Value(Some(Value::Int(12))) // 2*2*5 + 4*2*5 = 20+40... wait
        );
        // doubled over [0,5) = 4, over [5,10) = 8; sum = 4+8 = 12 ✓
    }

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
}
