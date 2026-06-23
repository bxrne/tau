//! Pure query evaluation over [`DbState`](super::executor::DbState).
//!
//! All functions here take only immutable references — no lock acquisition, no
//! mutation. They are the computational core shared by AT, RANGE, REDUCE, and
//! DERIVE queries, extracted from executor.rs so it stays focused on dispatch
//! and lifecycle.

use std::sync::Arc;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::executor::{DbState, ExecError};
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

pub(crate) fn ttl_cutoff(state: &DbState, lens: &str) -> Option<Timestamp> {
    state
        .ttl_secs
        .get(lens)
        .map(|&secs| crate::wall_clock::now_secs() - secs)
}

pub(crate) fn eval_lens(
    state: &DbState,
    name: &str,
    t: Timestamp,
) -> Result<Option<Value>, ExecError> {
    if state.base_types.contains_key(name) {
        if ttl_cutoff(state, name).is_some_and(|c| t < c) {
            return Ok(None);
        }
        Ok(state.db.at_name(name, t))
    } else if let Some(expr) = state.derived.get(name) {
        // An `OVER` bound on a derived lens limits its visible domain.
        if let Some(&(s, e)) = state.derived_ranges.get(name)
            && (t < s || t >= e)
        {
            return Ok(None);
        }
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
        (UnOp::Neg, Value::Int(i)) => Ok(Value::Int(i.wrapping_neg())),
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

/// Collect boundary timestamps and a layer snapshot for `name` over `[start, end)`.
/// Returns `(bounds, snap)` where `snap` is `Some` for base lenses and `None` for derived.
fn collect_bounds_and_snap(
    state: &DbState,
    name: &str,
    start: Timestamp,
    end: Timestamp,
) -> RangeBoundsResult {
    let mut bounds = Vec::with_capacity(64);
    bounds.push(start);
    bounds.push(end);
    let snap = if state.base_types.contains_key(name) {
        let s = state.db.layers(name);
        if let Some(ref ls) = s {
            collect_bounds_from_layers(ls, start, end, &mut bounds);
        }
        s
    } else {
        collect_lens_bounds(state, name, start, end, &mut bounds)?;
        None
    };
    Ok((bounds, snap))
}

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
    let (mut bounds, layers_snap) = collect_bounds_and_snap(state, name, start, end)?;
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
pub fn at_layers(layers: &[Layer<Value>], t: Timestamp) -> Option<Value> {
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

pub fn collect_bounds_from_layers(
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

/// Materialise `expr` into concrete `(start, end, value)` segments for an
/// `XDERIVE` lens.  Breakpoints come from the referenced lenses (the same
/// boundary set a `RANGE` scan would use); the value of each segment is the
/// expression evaluated at the segment start.  Adjacent equal segments merge.
///
/// The domain is `range` when given, otherwise the union extent of the
/// referenced base lenses' layers.  An empty domain (no `OVER`, no data yet)
/// yields no segments — the view is populated later as its sources are written.
pub(crate) fn materialise_expr(
    state: &DbState,
    expr: &Expr,
    range: Option<(Timestamp, Timestamp)>,
) -> Result<Vec<(Timestamp, Timestamp, Value)>, ExecError> {
    let (start, end) = match range {
        Some((s, e)) => (s, e),
        None => match expr_domain(state, expr) {
            Some(d) => d,
            None => return Ok(Vec::new()),
        },
    };
    if start >= end {
        return Ok(Vec::new());
    }
    let mut bounds = vec![start, end];
    collect_expr_bounds(state, expr, start, end, &mut bounds)?;
    bounds.retain(|&b| b >= start && b <= end);
    bounds.sort_unstable();
    bounds.dedup();
    let mut out: Vec<(Timestamp, Timestamp, Value)> =
        Vec::with_capacity(bounds.len().saturating_sub(1));
    for w in bounds.windows(2) {
        let (s, e) = (w[0], w[1]);
        if let Some(v) = eval_expr(state, expr, s)? {
            match out.last_mut() {
                Some(last) if last.1 == s && last.2 == v => last.1 = e,
                _ => out.push((s, e, v)),
            }
        }
    }
    Ok(out)
}

/// The combined time extent `[min_start, max_end)` of every base/materialised
/// lens referenced by `expr`, following lazy derived lenses transitively.
/// `None` when no referenced lens holds any data.
fn expr_domain(state: &DbState, expr: &Expr) -> Option<(Timestamp, Timestamp)> {
    let mut min: Option<Timestamp> = None;
    let mut max: Option<Timestamp> = None;
    collect_domain(state, expr, &mut min, &mut max);
    Some((min?, max?))
}

fn collect_domain(
    state: &DbState,
    expr: &Expr,
    min: &mut Option<Timestamp>,
    max: &mut Option<Timestamp>,
) {
    match expr {
        Expr::Lit(_) => {}
        Expr::Ident(name) | Expr::Agg { lens: name, .. } => {
            if state.base_types.contains_key(name) {
                if let Some(layers) = state.db.layers(name) {
                    for l in layers.iter() {
                        *min = Some(min.map_or(l.min_start, |m: Timestamp| m.min(l.min_start)));
                        *max = Some(max.map_or(l.max_end, |m: Timestamp| m.max(l.max_end)));
                    }
                }
            } else if let Some(inner) = state.derived.get(name) {
                let inner = inner.clone();
                collect_domain(state, &inner, min, max);
            }
        }
        Expr::Unary { expr, .. } => collect_domain(state, expr, min, max),
        Expr::Binary { lhs, rhs, .. } => {
            collect_domain(state, lhs, min, max);
            collect_domain(state, rhs, min, max);
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
    let (mut bounds, layers_snap) = collect_bounds_and_snap(state, lens, start, end)?;
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
        AggFunc::Min => fold_min_max(segments, false)?,
        AggFunc::Max => fold_min_max(segments, true)?,
        AggFunc::Sum => agg_sum(&segments)?,
        AggFunc::Avg => return agg_avg(&segments),
    }))
}

fn fold_min_max(segments: Vec<(i64, Value)>, want_max: bool) -> Result<Value, ExecError> {
    let mut values = segments.into_iter().map(|(_, v)| v);
    let first = values
        .next()
        .expect("non-empty segments guaranteed by caller");
    values.try_fold(first, |acc, v| numeric_min_max(acc, v, want_max))
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::database::Database;
    use crate::executor::DbState;
    use crate::model::{Layer, Tau};
    use crate::ql::ast::{Literal, Type};
    use crate::storage::InMemory;

    fn make_int_state(lens: &str, taus: &[(i64, i64, i64)]) -> DbState {
        let db = Database::new(InMemory::<Value>::new());
        if !taus.is_empty() {
            let layer = Layer::new(
                1,
                taus.iter()
                    .map(|&(s, e, v)| Tau::new(s, e, Value::Int(v)))
                    .collect(),
            );
            db.append(lens, layer).unwrap();
        }
        let mut base_types = HashMap::default();
        base_types.insert(lens.to_string(), Type::Int);
        DbState {
            db,
            base_types,
            next_layer_id: 2,
            derived: HashMap::default(),
            derived_ranges: HashMap::default(),
            xderived: HashMap::default(),
            ttl_secs: HashMap::default(),
        }
    }

    #[hegel::test]
    fn pbt_apply_unary_neg_int_negates(tc: TestCase) {
        let v = tc.draw(gs::integers::<i64>());
        assert_eq!(
            apply_unary(UnOp::Neg, Value::Int(v)).unwrap(),
            Value::Int(v.wrapping_neg())
        );
    }

    #[hegel::test]
    fn pbt_apply_unary_neg_float_negates(tc: TestCase) {
        let v = tc.draw(gs::floats::<f64>().filter(|f| f.is_finite()));
        assert_eq!(
            apply_unary(UnOp::Neg, Value::Float(v)).unwrap(),
            Value::Float(-v)
        );
    }

    #[hegel::test]
    fn pbt_apply_unary_not_inverts_bool(tc: TestCase) {
        let b = tc.draw(gs::booleans());
        assert_eq!(
            apply_unary(UnOp::Not, Value::Bool(b)).unwrap(),
            Value::Bool(!b)
        );
    }

    #[hegel::test]
    fn pbt_apply_unary_neg_bool_errors(tc: TestCase) {
        let b = tc.draw(gs::booleans());
        assert!(apply_unary(UnOp::Neg, Value::Bool(b)).is_err());
    }

    #[hegel::test]
    fn pbt_apply_unary_not_non_bool_errors(tc: TestCase) {
        let v = tc.draw(gs::integers::<i64>());
        assert!(apply_unary(UnOp::Not, Value::Int(v)).is_err());
    }

    #[hegel::test]
    fn pbt_apply_binary_int_add_is_wrapping(tc: TestCase) {
        let x = tc.draw(gs::integers::<i64>());
        let y = tc.draw(gs::integers::<i64>());
        let r = apply_binary(BinOp::Add, Value::Int(x), Value::Int(y)).unwrap();
        assert_eq!(r, Value::Int(x.wrapping_add(y)));
    }

    #[hegel::test]
    fn pbt_apply_binary_int_sub_is_wrapping(tc: TestCase) {
        let x = tc.draw(gs::integers::<i64>());
        let y = tc.draw(gs::integers::<i64>());
        let r = apply_binary(BinOp::Sub, Value::Int(x), Value::Int(y)).unwrap();
        assert_eq!(r, Value::Int(x.wrapping_sub(y)));
    }

    #[hegel::test]
    fn pbt_apply_binary_int_mul_is_wrapping(tc: TestCase) {
        let x = tc.draw(gs::integers::<i64>().min_value(-1000).max_value(1000));
        let y = tc.draw(gs::integers::<i64>().min_value(-1000).max_value(1000));
        let r = apply_binary(BinOp::Mul, Value::Int(x), Value::Int(y)).unwrap();
        assert_eq!(r, Value::Int(x.wrapping_mul(y)));
    }

    #[test]
    fn apply_binary_int_div_by_zero_errors() {
        assert!(apply_binary(BinOp::Div, Value::Int(1), Value::Int(0)).is_err());
    }

    #[test]
    fn apply_binary_int_mod_by_zero_errors() {
        assert!(apply_binary(BinOp::Mod, Value::Int(5), Value::Int(0)).is_err());
    }

    #[hegel::test]
    fn pbt_apply_binary_int_cmp_lt(tc: TestCase) {
        let x = tc.draw(gs::integers::<i64>().min_value(-1000).max_value(1000));
        let y = tc.draw(gs::integers::<i64>().min_value(-1000).max_value(1000));
        let r = apply_binary(BinOp::Lt, Value::Int(x), Value::Int(y)).unwrap();
        assert_eq!(r, Value::Bool(x < y));
    }

    #[hegel::test]
    fn pbt_apply_binary_bool_and(tc: TestCase) {
        let a = tc.draw(gs::booleans());
        let b = tc.draw(gs::booleans());
        let r = apply_binary(BinOp::And, Value::Bool(a), Value::Bool(b)).unwrap();
        assert_eq!(r, Value::Bool(a && b));
    }

    #[hegel::test]
    fn pbt_apply_binary_bool_or(tc: TestCase) {
        let a = tc.draw(gs::booleans());
        let b = tc.draw(gs::booleans());
        let r = apply_binary(BinOp::Or, Value::Bool(a), Value::Bool(b)).unwrap();
        assert_eq!(r, Value::Bool(a || b));
    }

    #[hegel::test]
    fn pbt_apply_binary_and_non_bool_errors(tc: TestCase) {
        let v = tc.draw(gs::integers::<i64>());
        assert!(apply_binary(BinOp::And, Value::Int(v), Value::Bool(true)).is_err());
    }

    #[hegel::test]
    fn pbt_apply_binary_eq_int_int(tc: TestCase) {
        let x = tc.draw(gs::integers::<i64>().min_value(-100).max_value(100));
        let r = apply_binary(BinOp::Eq, Value::Int(x), Value::Int(x)).unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[hegel::test]
    fn pbt_apply_binary_not_eq_distinct_ints(tc: TestCase) {
        let x = tc.draw(gs::integers::<i64>().min_value(-100).max_value(100));
        let y = tc.draw(
            gs::integers::<i64>()
                .min_value(-100)
                .max_value(100)
                .filter(move |&v| v != x),
        );
        let r = apply_binary(BinOp::NotEq, Value::Int(x), Value::Int(y)).unwrap();
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn values_equal_null_null_is_true() {
        assert_eq!(values_equal(&Value::Null, &Value::Null).unwrap(), true);
    }

    #[hegel::test]
    fn pbt_values_equal_null_anything_is_false(tc: TestCase) {
        let v = tc.draw(gs::integers::<i64>());
        assert_eq!(values_equal(&Value::Null, &Value::Int(v)).unwrap(), false);
        assert_eq!(values_equal(&Value::Int(v), &Value::Null).unwrap(), false);
    }

    #[hegel::test]
    fn pbt_values_equal_int_float_promotion(tc: TestCase) {
        let i = tc.draw(gs::integers::<i64>().min_value(-1000).max_value(1000));
        assert_eq!(
            values_equal(&Value::Int(i), &Value::Float(i as f64)).unwrap(),
            true
        );
    }

    #[hegel::test]
    fn pbt_values_equal_str_str(tc: TestCase) {
        let s = tc.draw(gs::text().max_size(32));
        let arc: Arc<str> = Arc::from(s.as_str());
        assert_eq!(
            values_equal(&Value::Str(arc.clone()), &Value::Str(arc)).unwrap(),
            true
        );
    }

    #[hegel::test]
    fn pbt_values_equal_bool_bool(tc: TestCase) {
        let b = tc.draw(gs::booleans());
        assert_eq!(
            values_equal(&Value::Bool(b), &Value::Bool(b)).unwrap(),
            true
        );
        assert_eq!(
            values_equal(&Value::Bool(b), &Value::Bool(!b)).unwrap(),
            false
        );
    }

    #[test]
    fn values_equal_str_int_errors() {
        assert!(values_equal(&Value::Str(Arc::from("x")), &Value::Int(1)).is_err());
    }

    #[hegel::test]
    fn pbt_as_f64_converts_int_and_float(tc: TestCase) {
        let i = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        assert_eq!(as_f64(&Value::Int(i)), Some(i as f64));
        assert_eq!(as_f64(&Value::Float(i as f64)), Some(i as f64));
        assert_eq!(as_f64(&Value::Bool(true)), None);
        assert_eq!(as_f64(&Value::Null), None);
    }

    #[hegel::test]
    fn pbt_agg_sum_ints_matches_wrapping_sum(tc: TestCase) {
        let vals =
            tc.draw(gs::vecs(gs::integers::<i64>().min_value(-1000).max_value(1000)).max_size(20));
        let segs: Vec<(i64, Value)> = vals.iter().map(|&v| (1i64, Value::Int(v))).collect();
        if segs.is_empty() {
            return;
        }
        let result = agg_sum(&segs).unwrap();
        let expected = vals.iter().copied().fold(0i64, |a, b| a.wrapping_add(b));
        assert_eq!(result, Value::Int(expected));
    }

    #[test]
    fn agg_sum_non_numeric_errors() {
        let segs = vec![(1i64, Value::Str(Arc::from("x")))];
        assert!(agg_sum(&segs).is_err());
    }

    #[hegel::test]
    fn pbt_agg_avg_equal_durations_is_arithmetic_mean(tc: TestCase) {
        let vals = tc.draw(
            gs::vecs(gs::integers::<i64>().min_value(-100).max_value(100))
                .min_size(2)
                .max_size(10),
        );
        let segs: Vec<(i64, Value)> = vals.iter().map(|&v| (1i64, Value::Int(v))).collect();
        let avg = agg_avg(&segs).unwrap().unwrap();
        let expected = vals.iter().map(|&v| v as f64).sum::<f64>() / vals.len() as f64;
        if let Value::Float(f) = avg {
            assert!((f - expected).abs() < 1e-9);
        } else {
            panic!("expected float");
        }
    }

    #[test]
    fn agg_avg_empty_returns_none() {
        assert_eq!(agg_avg(&[]).unwrap(), None);
    }

    #[hegel::test]
    fn pbt_numeric_min_max_ints(tc: TestCase) {
        let x = tc.draw(gs::integers::<i64>().min_value(-1000).max_value(1000));
        let y = tc.draw(gs::integers::<i64>().min_value(-1000).max_value(1000));
        assert_eq!(
            numeric_min_max(Value::Int(x), Value::Int(y), false).unwrap(),
            Value::Int(x.min(y))
        );
        assert_eq!(
            numeric_min_max(Value::Int(x), Value::Int(y), true).unwrap(),
            Value::Int(x.max(y))
        );
    }

    #[test]
    fn numeric_min_max_non_numeric_errors() {
        assert!(numeric_min_max(Value::Str(Arc::from("a")), Value::Int(1), false).is_err());
    }

    #[test]
    fn would_cycle_direct_self_reference() {
        let derived = HashMap::default();
        let expr = Expr::Ident("a".into());
        let mut visited = HashSet::default();
        assert!(would_cycle(&derived, "a", &expr, &mut visited));
    }

    #[test]
    fn would_cycle_transitive() {
        let mut derived = HashMap::default();
        derived.insert("b".to_string(), Expr::Ident("a".into()));
        let expr = Expr::Ident("b".into());
        let mut visited = HashSet::default();
        assert!(would_cycle(&derived, "a", &expr, &mut visited));
    }

    #[test]
    fn would_cycle_acyclic_returns_false() {
        let mut derived = HashMap::default();
        derived.insert("b".to_string(), Expr::Lit(Literal::Int(1)));
        let expr = Expr::Ident("b".into());
        let mut visited = HashSet::default();
        assert!(!would_cycle(&derived, "a", &expr, &mut visited));
    }

    #[test]
    fn would_cycle_literal_never_cycles() {
        let derived = HashMap::default();
        let expr = Expr::Lit(Literal::Int(42));
        let mut visited = HashSet::default();
        assert!(!would_cycle(&derived, "anything", &expr, &mut visited));
    }

    #[test]
    fn eval_lens_base_returns_value_at_t() {
        let state = make_int_state("x", &[(0, 10, 42)]);
        assert_eq!(eval_lens(&state, "x", 5).unwrap(), Some(Value::Int(42)));
    }

    #[test]
    fn eval_lens_base_returns_none_outside_interval() {
        let state = make_int_state("x", &[(0, 10, 42)]);
        assert_eq!(eval_lens(&state, "x", 10).unwrap(), None);
        assert_eq!(eval_lens(&state, "x", 100).unwrap(), None);
    }

    #[test]
    fn eval_lens_unknown_errors() {
        let state = make_int_state("x", &[(0, 10, 1)]);
        assert!(eval_lens(&state, "ghost", 5).is_err());
    }

    #[hegel::test]
    fn pbt_eval_lens_in_range_vs_out_of_range(tc: TestCase) {
        let t_in = tc.draw(gs::integers::<i64>().min_value(0).max_value(99));
        let t_out = tc.draw(gs::integers::<i64>().min_value(100).max_value(1_000_000));
        let state = make_int_state("x", &[(0, 100, 7)]);
        assert_eq!(eval_lens(&state, "x", t_in).unwrap(), Some(Value::Int(7)));
        assert_eq!(eval_lens(&state, "x", t_out).unwrap(), None);
    }

    #[test]
    fn eval_expr_literal_returns_value() {
        let state = make_int_state("x", &[]);
        assert_eq!(
            eval_expr(&state, &Expr::Lit(Literal::Int(99)), 0).unwrap(),
            Some(Value::Int(99))
        );
    }

    #[test]
    fn eval_expr_binary_add() {
        let state = make_int_state("x", &[(0, 10, 3)]);
        let expr = Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(Expr::Ident("x".into())),
            rhs: Box::new(Expr::Lit(Literal::Int(10))),
        };
        assert_eq!(eval_expr(&state, &expr, 5).unwrap(), Some(Value::Int(13)));
    }

    #[test]
    fn eval_expr_binary_propagates_none_on_missing_operand() {
        let state = make_int_state("x", &[]);
        let expr = Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(Expr::Ident("x".into())),
            rhs: Box::new(Expr::Lit(Literal::Int(1))),
        };
        assert_eq!(eval_expr(&state, &expr, 5).unwrap(), None);
    }

    #[test]
    fn collect_agg_segments_returns_duration_weighted_list() {
        let state = make_int_state("x", &[(0, 5, 10), (5, 10, 20)]);
        let segs = collect_agg_segments(&state, "x", 0, 10).unwrap();
        assert_eq!(segs.len(), 2);
        assert!(segs.iter().all(|(d, _)| *d == 5));
    }

    #[test]
    fn eval_agg_count_counts_distinct_segments() {
        let state = make_int_state("x", &[(0, 5, 1), (5, 10, 2), (10, 15, 3)]);
        let r = eval_agg(&state, "x", AggFunc::Count, 0, 15).unwrap();
        assert_eq!(r, Some(Value::Int(3)));
    }

    #[test]
    fn eval_agg_sum_sums_values() {
        let state = make_int_state("x", &[(0, 5, 3), (5, 10, 7)]);
        let r = eval_agg(&state, "x", AggFunc::Sum, 0, 10).unwrap();
        assert_eq!(r, Some(Value::Int(10)));
    }

    #[test]
    fn eval_agg_min_max_correct() {
        let state = make_int_state("x", &[(0, 5, 3), (5, 10, 7)]);
        assert_eq!(
            eval_agg(&state, "x", AggFunc::Min, 0, 10).unwrap(),
            Some(Value::Int(3))
        );
        assert_eq!(
            eval_agg(&state, "x", AggFunc::Max, 0, 10).unwrap(),
            Some(Value::Int(7))
        );
    }

    #[test]
    fn eval_agg_empty_range_returns_none() {
        let state = make_int_state("x", &[(0, 10, 5)]);
        assert_eq!(eval_agg(&state, "x", AggFunc::Sum, 20, 30).unwrap(), None);
    }

    #[hegel::test]
    fn pbt_at_layers_newest_wins(tc: TestCase) {
        let t = tc.draw(gs::integers::<i64>().min_value(0).max_value(9));
        let v1 = tc.draw(gs::integers::<i64>());
        let v2 = tc.draw(gs::integers::<i64>());
        let layers = vec![
            Layer::new(1, vec![Tau::new(0, 10, Value::Int(v1))]),
            Layer::new(2, vec![Tau::new(0, 10, Value::Int(v2))]),
        ];
        assert_eq!(at_layers(&layers, t), Some(Value::Int(v2)));
    }

    #[hegel::test]
    fn pbt_at_layers_returns_none_outside_all_layers(tc: TestCase) {
        let t = tc.draw(gs::integers::<i64>().min_value(100).max_value(1_000_000));
        let layers = vec![Layer::new(1, vec![Tau::new(0, 10, Value::Int(1))])];
        assert_eq!(at_layers(&layers, t), None);
    }

    #[test]
    fn at_layers_empty_returns_none() {
        assert_eq!(at_layers(&[], 5), None);
    }
}
