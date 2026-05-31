//! Pure query evaluation over [`DbState`](super::executor::DbState).
//!
//! All functions here take only immutable references — no lock acquisition, no
//! mutation. They are the computational core shared by AT, RANGE, REDUCE, and
//! DERIVE queries, extracted from executor.rs so it stays focused on dispatch
//! and lifecycle.

use std::sync::Arc;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::executor::DbState;
use crate::executor::ExecError;
use crate::model::{Layer, Timestamp};
use crate::ql::ast::{AggFunc, BinOp, Expr, UnOp};
use crate::value::Value;

/// Returns `true` if wiring `target = expr` would create a derivation cycle.
///
/// Performs a DFS through `expr`'s identifiers following the existing `derived`
/// map. The `visited` set prevents revisiting so the traversal terminates even
/// if `derived` already contains cycles from before this check was introduced.
pub(crate) fn would_cycle(
    derived: &HashMap<String, Expr>,
    target: &str,
    expr: &Expr,
    visited: &mut HashSet<String>,
) -> bool {
    let names: Vec<&str> = match expr {
        Expr::Ident(n) => vec![n.as_str()],
        Expr::Agg { lens, .. } => vec![lens.as_str()],
        Expr::Lit(_) => return false,
        Expr::Unary { expr, .. } => return would_cycle(derived, target, expr, visited),
        Expr::Binary { lhs, rhs, .. } => {
            return would_cycle(derived, target, lhs, visited)
                || would_cycle(derived, target, rhs, visited);
        }
    };
    for name in names {
        if name == target {
            return true;
        }
        if visited.insert(name.to_string())
            && let Some(dep_expr) = derived.get(name)
        {
            let dep_expr = dep_expr.clone();
            if would_cycle(derived, target, &dep_expr, visited) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn eval_lens(
    state: &DbState,
    name: &str,
    t: Timestamp,
) -> Result<Option<Value>, ExecError> {
    if state.base_types.contains_key(name) {
        Ok(state.db.at_name(name, t))
    } else if let Some(expr) = state.derived.get(name) {
        eval_expr(state, expr, t)
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

pub(crate) fn eval_expr(
    state: &DbState,
    expr: &Expr,
    t: Timestamp,
) -> Result<Option<Value>, ExecError> {
    match expr {
        Expr::Lit(l) => Ok(Some(l.into())),
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

pub(crate) fn apply_unary(op: UnOp, v: Value) -> Result<Value, ExecError> {
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

/// Coerce an `Int`/`Float` value to `f64`; `None` for all other variants.
pub(crate) fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

pub(crate) fn apply_int_binary_op(op: BinOp, x: i64, y: i64) -> Result<Value, ExecError> {
    use BinOp::*;
    Ok(match op {
        Add => Value::Int(x.wrapping_add(y)),
        Sub => Value::Int(x.wrapping_sub(y)),
        Mul => Value::Int(x.wrapping_mul(y)),
        Div => {
            if y == 0 {
                return Err(ExecError::InvalidExpr("divide by zero".into()));
            }
            Value::Int(x / y)
        }
        Mod => {
            if y == 0 {
                return Err(ExecError::InvalidExpr("modulo by zero".into()));
            }
            Value::Int(x % y)
        }
        Lt => Value::Bool(x < y),
        LtEq => Value::Bool(x <= y),
        Gt => Value::Bool(x > y),
        GtEq => Value::Bool(x >= y),
        _ => unreachable!(),
    })
}

pub(crate) fn apply_float_binary_op(op: BinOp, x: f64, y: f64) -> Result<Value, ExecError> {
    use BinOp::*;
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

pub(crate) fn apply_binary(op: BinOp, a: Value, b: Value) -> Result<Value, ExecError> {
    use BinOp::*;
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
    if matches!(op, Eq | NotEq) {
        let eq = values_equal(&a, &b)?;
        return Ok(Value::Bool(if op == Eq { eq } else { !eq }));
    }
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        return apply_int_binary_op(op, *x, *y);
    }
    let (Some(x), Some(y)) = (as_f64(&a), as_f64(&b)) else {
        return Err(ExecError::InvalidExpr(format!(
            "operator {op:?} requires numeric operands, got {}/{}",
            a.type_name(),
            b.type_name()
        )));
    };
    apply_float_binary_op(op, x, y)
}

/// Strict variant equality with numeric promotion. Returns `Err` only for
/// genuinely incomparable variants.
pub(crate) fn values_equal(a: &Value, b: &Value) -> Result<bool, ExecError> {
    use Value::*;
    match (a, b) {
        (Null, Null) => Ok(true),
        (Null, _) | (_, Null) => Ok(false),
        (Bool(x), Bool(y)) => Ok(x == y),
        (Str(x), Str(y)) => Ok(x == y),
        (Int(_), Int(_)) | (Int(_), Float(_)) | (Float(_), Int(_)) | (Float(_), Float(_)) => {
            Ok(as_f64(a).expect("Int/Float is always convertible to f64")
                == as_f64(b).expect("Int/Float is always convertible to f64"))
        }
        (x, y) => Err(ExecError::InvalidExpr(format!(
            "cannot compare {} with {}",
            x.type_name(),
            y.type_name()
        ))),
    }
}

type RangeBoundsResult = Result<(Vec<Timestamp>, Option<Arc<Vec<Layer<Value>>>>), ExecError>;

/// Collect sorted, deduplicated boundary timestamps for a range scan of `name`
/// over `[start, end)`, including any filter-expression boundaries. Returns the
/// bounds and an optional layer snapshot (Some for base lenses, None for derived).
pub(crate) fn collect_range_bounds(
    state: &DbState,
    name: &str,
    start: Timestamp,
    end: Timestamp,
    filter: Option<&Expr>,
) -> RangeBoundsResult {
    let mut bounds = Vec::with_capacity(64);
    bounds.push(start);
    bounds.push(end);
    let layers_snap: Option<Arc<Vec<Layer<Value>>>> = if state.base_types.contains_key(name) {
        let snap = state.db.layers(name);
        if let Some(ref ls) = snap {
            collect_bounds_from_layers(ls, start, end, &mut bounds);
        }
        snap
    } else {
        collect_lens_bounds(state, name, start, end, &mut bounds)?;
        None
    };
    if let Some(f) = filter {
        collect_expr_bounds(state, f, start, end, &mut bounds)?;
    }
    bounds.sort_unstable();
    bounds.dedup();
    Ok((bounds, layers_snap))
}

/// Resolve the value of `name` at time `t`, using a pre-taken layer snapshot
/// when available to avoid re-acquiring the store lock per boundary.
pub(crate) fn resolve_value_at(
    state: &DbState,
    name: &str,
    layers_snap: Option<&[Layer<Value>]>,
    t: Timestamp,
) -> Result<Option<Value>, ExecError> {
    Ok(if let Some(ls) = layers_snap {
        at_layers(ls, t)
    } else {
        eval_lens(state, name, t)?
    })
}

/// Build output segments for a range scan from pre-computed `bounds`, an
/// optional layer snapshot, and an optional filter expression.
pub(crate) fn build_range_segments(
    state: &DbState,
    name: &str,
    bounds: &[Timestamp],
    layers_snap: Option<&[Layer<Value>]>,
    filter: Option<&Expr>,
) -> Result<Vec<(Timestamp, Timestamp, Value)>, ExecError> {
    let mut out: Vec<(Timestamp, Timestamp, Value)> =
        Vec::with_capacity(bounds.len().saturating_sub(1));
    for w in bounds.windows(2) {
        let (s, e) = (w[0], w[1]);
        let v = match resolve_value_at(state, name, layers_snap, s)? {
            Some(v) => v,
            None => continue,
        };
        if let Some(f) = filter {
            match eval_expr(state, f, s)? {
                Some(Value::Bool(true)) => {}
                _ => continue,
            }
        }
        match out.last_mut() {
            Some(last) if last.1 == s && last.2 == v => last.1 = e,
            _ => out.push((s, e, v)),
        }
    }
    Ok(out)
}

/// Point lookup over a pre-taken layer snapshot. Avoids any lock acquisition —
/// callers must already hold a safe snapshot via `Database::layers`.
/// Uses each layer's `min_start`/`max_end` range to skip non-covering layers
/// before chasing the Arc pointer into the tau slice.
#[inline]
pub(crate) fn at_layers(layers: &[Layer<Value>], t: Timestamp) -> Option<Value> {
    layers
        .iter()
        .rev()
        .find_map(|layer| {
            if t < layer.min_start || t >= layer.max_end {
                return None;
            }
            layer.at(t)
        })
        .cloned()
}

pub(crate) fn collect_bounds_from_layers(
    layers: &[Layer<Value>],
    start: Timestamp,
    end: Timestamp,
    out: &mut Vec<Timestamp>,
) {
    for layer in layers {
        let taus = &layer.taus;
        let s_lo = taus.partition_point(|t| t.start <= start);
        let s_hi = taus.partition_point(|t| t.start < end);
        for tau in &taus[s_lo..s_hi] {
            out.push(tau.start);
        }
        let e_lo = taus.partition_point(|t| t.end <= start);
        let e_hi = taus.partition_point(|t| t.end < end);
        for tau in &taus[e_lo..e_hi] {
            out.push(tau.end);
        }
    }
}

pub(crate) fn collect_lens_bounds(
    state: &DbState,
    name: &str,
    start: Timestamp,
    end: Timestamp,
    out: &mut Vec<Timestamp>,
) -> Result<(), ExecError> {
    if state.base_types.contains_key(name) {
        if let Some(layers) = state.db.layers(name) {
            collect_bounds_from_layers(&layers, start, end, out);
        }
        Ok(())
    } else if let Some(expr) = state.derived.get(name) {
        collect_expr_bounds(state, expr, start, end, out)
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

pub(crate) fn collect_expr_bounds(
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
            // lens at position p enters the window (t = p - rel_start) or exits
            // it (t = p - rel_end). Collect the bounding superset.
            let lo = start.saturating_add((*rel_start).min(*rel_end));
            let hi = end.saturating_add((*rel_start).max(*rel_end));
            let mut inner = Vec::new();
            if lo < hi {
                collect_lens_bounds(state, lens, lo, hi, &mut inner)?;
            }
            for p in inner {
                for shift in [*rel_start, *rel_end] {
                    let t_change = p.saturating_sub(shift);
                    if t_change > start && t_change < end {
                        out.push(t_change);
                    }
                }
            }
            Ok(())
        }
    }
}

pub(crate) fn agg_sum(segments: &[(i64, Value)]) -> Result<Value, ExecError> {
    let mut int_sum: i64 = 0;
    let mut float_sum: Option<f64> = None;
    for (_, v) in segments {
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
    Ok(float_sum.map(Value::Float).unwrap_or(Value::Int(int_sum)))
}

pub(crate) fn agg_avg(segments: &[(i64, Value)]) -> Result<Option<Value>, ExecError> {
    let total: i64 = segments.iter().map(|(d, _)| *d).sum();
    if total == 0 {
        return Ok(None);
    }
    let mut weighted = 0.0f64;
    for (d, v) in segments {
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
    Ok(Some(Value::Float(weighted / total as f64)))
}

pub(crate) fn collect_agg_segments(
    state: &DbState,
    lens: &str,
    start: Timestamp,
    end: Timestamp,
) -> Result<Vec<(i64, Value)>, ExecError> {
    let mut bounds = Vec::with_capacity(64);
    bounds.push(start);
    bounds.push(end);
    let layers_snap: Option<Arc<Vec<Layer<Value>>>> = if state.base_types.contains_key(lens) {
        let snap = state.db.layers(lens);
        if let Some(ref ls) = snap {
            collect_bounds_from_layers(ls, start, end, &mut bounds);
        }
        snap
    } else {
        collect_lens_bounds(state, lens, start, end, &mut bounds)?;
        None
    };
    bounds.sort_unstable();
    bounds.dedup();
    let mut segments: Vec<(i64, Value)> = Vec::with_capacity(bounds.len().saturating_sub(1));
    for w in bounds.windows(2) {
        let (s, e) = (w[0], w[1]);
        let v = if let Some(ref ls) = layers_snap {
            at_layers(ls.as_slice(), s)
        } else {
            eval_lens(state, lens, s)?
        };
        if let Some(v) = v {
            segments.push((e - s, v));
        }
    }
    Ok(segments)
}

pub(crate) fn eval_agg(
    state: &DbState,
    lens: &str,
    func: AggFunc,
    start: Timestamp,
    end: Timestamp,
) -> Result<Option<Value>, ExecError> {
    let segments = collect_agg_segments(state, lens, start, end)?;
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
            .expect("non-empty segments guaranteed by early return above"),
        AggFunc::Max => segments
            .into_iter()
            .map(|(_, v)| v)
            .try_fold(None::<Value>, |acc, v| match acc {
                None => Ok(Some(v)),
                Some(a) => numeric_min_max(a, v, true).map(Some),
            })?
            .expect("non-empty segments guaranteed by early return above"),
        AggFunc::Sum => agg_sum(&segments)?,
        AggFunc::Avg => return agg_avg(&segments),
    }))
}

pub(crate) fn numeric_min_max(a: Value, b: Value, want_max: bool) -> Result<Value, ExecError> {
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
