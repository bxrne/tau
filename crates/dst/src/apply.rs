//! Lock-step apply to target (SUT) and isolated oracle (model).

use libdst::Divergence;
use libtau::{Executor, Output, Value};
use tracing::{debug, warn};

use crate::op::Op;
use crate::oracle::{Oracle, Ts};
use crate::target::{DirectExecutor, Target};

#[cfg(test)]
pub fn apply_oracle_only(op: &Op, model: &mut Oracle) {
    model.apply_op(op);
}

/// Keep target and oracle transaction flags aligned so btree guards and replay match.
pub fn sync_transactions(target: &mut impl Target, model: &mut Oracle) {
    let target_tx = target.is_in_transaction();
    let model_tx = model.in_transaction();
    if target_tx && !model_tx {
        target.rollback_open_transaction();
    } else if model_tx && !target_tx {
        model.rollback();
    }
}

fn apply_model_after_target(op: &Op, model: &mut Oracle) {
    match op {
        Op::At { .. } | Op::Range { .. } | Op::Reduce { .. } => model.apply_op(op),
        Op::StartTransaction => {
            if model.in_transaction() {
                model.rollback();
            }
            model.start_transaction();
        }
        Op::Commit => model.commit(),
        Op::Rollback => model.rollback(),
        _ => model.apply_op(op),
    }
}

/// Apply one op to target and oracle in lock-step; return any divergences found.
pub fn apply_dual(
    step: usize,
    op: &Op,
    target: &mut impl Target,
    model: &mut Oracle,
) -> Vec<Divergence> {
    let mut divs: Vec<Divergence> = Vec::new();
    sync_transactions(target, model);

    match op {
        Op::At { lens, t } => {
            let Some(got) = exec_output(target, &op.to_sql(), step, &mut divs) else {
                return divs;
            };
            let expected = model.at(lens, *t);
            check_at(step, &format!("AT {lens} {t}"), &got, expected, &mut divs);
        }
        Op::AtAsOf { lens, t, as_of } => {
            let Some(got) = exec_output(target, &op.to_sql(), step, &mut divs) else {
                return divs;
            };
            let expected = model.at_as_of(lens, *t, *as_of);
            check_at(
                step,
                &format!("AT {lens} {t} AS OF {as_of}"),
                &got,
                expected,
                &mut divs,
            );
        }
        Op::History { lens } => {
            let Some(got) = exec_output(target, &op.to_sql(), step, &mut divs) else {
                return divs;
            };
            let expected = model.history_generations(lens);
            check_history(step, &format!("HISTORY {lens}"), &got, expected, &mut divs);
        }
        Op::AtNd { lens, ts, as_of } => {
            let Some(got) = exec_output(target, &op.to_sql(), step, &mut divs) else {
                return divs;
            };
            let expected = model.at_nd(lens, ts, *as_of);
            check_at(
                step,
                &format!("AT ND {lens} {ts:?} as_of={as_of:?}"),
                &got,
                expected,
                &mut divs,
            );
        }
        Op::RangeNd {
            lens,
            start,
            end,
            fixed,
        } => {
            if start >= end {
                return divs;
            }
            let Some(got) = exec_output(target, &op.to_sql(), step, &mut divs) else {
                return divs;
            };
            let expected = model.range_nd(lens, *start, *end, fixed);
            check_range(
                step,
                &format!("RANGE ND {lens} {start} {end} at {fixed:?}"),
                &got,
                expected,
                &mut divs,
            );
        }
        Op::Range { lens, start, end } => {
            if start >= end {
                return divs;
            }
            let Some(got) = exec_output(target, &op.to_sql(), step, &mut divs) else {
                return divs;
            };
            let expected = model.range(lens, *start, *end);
            check_range(
                step,
                &format!("RANGE {lens} {start} {end}"),
                &got,
                expected,
                &mut divs,
            );
        }
        Op::Reduce {
            lens,
            start,
            end,
            func,
        } => {
            let Some(got) = exec_output(target, &op.to_sql(), step, &mut divs) else {
                return divs;
            };
            let expected = model.reduce(lens, *start, *end, *func);
            check_reduce(
                step,
                &format!("REDUCE {lens} {start} {end} {func}"),
                &got,
                expected,
                &mut divs,
            );
        }
        // Mutations: apply to the target first, then mirror into the model
        // only on success so the two never drift on rejected statements.
        _ => {
            if matches!(op, Op::StartTransaction) && target.is_in_transaction() {
                target.rollback_open_transaction();
            }
            let sql = op.to_sql();
            if exec_ok(target, &sql, step, &mut divs) {
                apply_model_after_target(op, model);
                debug!(step, sql, "OP");
            }
        }
    }
    divs
}

pub fn apply_dual_executor(
    step: usize,
    op: &Op,
    target: &mut Executor,
    model: &mut Oracle,
) -> Vec<Divergence> {
    apply_dual(step, op, &mut DirectExecutor(target), model)
}

fn exec_ok(target: &mut impl Target, q: &str, step: usize, divs: &mut Vec<Divergence>) -> bool {
    exec_output(target, q, step, divs).is_some()
}

fn exec_output(
    target: &mut impl Target,
    q: &str,
    step: usize,
    divs: &mut Vec<Divergence>,
) -> Option<Output> {
    match target.exec(q) {
        Ok(out) => Some(out),
        Err(e) => {
            warn!(?e, q, "exec error");
            divs.push(Divergence::new(
                step,
                format!("exec error: {q}"),
                "Ok",
                format!("{e:?}"),
            ));
            None
        }
    }
}

fn check_at(
    step: usize,
    label: &str,
    got: &Output,
    expected: Option<Value>,
    divs: &mut Vec<Divergence>,
) {
    let got_val = match got {
        Output::Value(v) => v.clone(),
        _ => panic!("expected Output::Value for AT"),
    };
    if got_val != expected {
        warn!(step, label, ?got_val, ?expected, "MISMATCH AT");
        divs.push(Divergence::new(step, label, expected, got_val));
    } else {
        debug!(step, label, "AT OK");
    }
}

fn check_range(
    step: usize,
    label: &str,
    got: &Output,
    expected: Vec<(Ts, Ts, Value)>,
    divs: &mut Vec<Divergence>,
) {
    let got_segs = match got {
        Output::Range(s) => s.clone(),
        _ => panic!("expected Output::Range"),
    };
    if got_segs != expected {
        warn!(
            step,
            label,
            got_len = got_segs.len(),
            expected_len = expected.len(),
            "MISMATCH RANGE"
        );
        divs.push(Divergence::new(step, label, expected, got_segs));
    } else {
        debug!(step, label, segs = got_segs.len(), "RANGE OK");
    }
}

/// Compare the transaction-time generations reported by `HISTORY` against the
/// oracle. Layer ids and per-layer fragment counts are engine-internal and may
/// differ, but the multiset of surviving `written_at` generations must match
/// exactly — that is the property that proves compaction preserved the
/// transaction axis.
fn check_history(
    step: usize,
    label: &str,
    got: &Output,
    expected: Option<Vec<i64>>,
    divs: &mut Vec<Divergence>,
) {
    let got_gens = match got {
        Output::LayerHistory(layers) => {
            let mut g: Vec<i64> = layers.iter().map(|l| l.written_at).collect();
            g.sort_unstable();
            g.dedup();
            g
        }
        _ => panic!("expected Output::LayerHistory for HISTORY"),
    };
    let expected = expected.unwrap_or_default();
    if got_gens != expected {
        warn!(step, label, ?got_gens, ?expected, "MISMATCH HISTORY");
        divs.push(Divergence::new(
            step,
            label,
            format!("{expected:?}"),
            format!("{got_gens:?}"),
        ));
    } else {
        debug!(step, label, gens = got_gens.len(), "HISTORY OK");
    }
}

fn check_reduce(
    step: usize,
    label: &str,
    got: &Output,
    expected: Option<Value>,
    divs: &mut Vec<Divergence>,
) {
    let got_val = match got {
        Output::Value(v) => v.clone(),
        _ => panic!("expected Output::Value for REDUCE"),
    };
    let ok = match (&got_val, &expected) {
        (Some(Value::Float(g)), Some(Value::Float(e))) => (g - e).abs() < 1e-9,
        _ => got_val == expected,
    };
    if !ok {
        warn!(step, label, ?got_val, ?expected, "MISMATCH REDUCE");
        divs.push(Divergence::new(step, label, expected, got_val));
    } else {
        debug!(step, label, "REDUCE OK");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree;
    use crate::profile::MEMORY_MULTI;
    use hegel::TestCase;
    use hegel::generators as gs;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn oracle_replay_is_deterministic() {
        let paths = crate::profile::ProfileWorkspace::new(MEMORY_MULTI).paths;
        let mut o1 = MEMORY_MULTI.bootstrap_oracle(&paths);
        let mut rng = StdRng::seed_from_u64(42);
        let mut ops = Vec::new();
        for _ in 0..30 {
            ops.push(btree::pick(
                &mut rng,
                &o1,
                false,
                false,
                crate::oracle::DST_TX_BASE_MS,
            ));
        }
        for op in &ops {
            apply_oracle_only(op, &mut o1);
        }
        let mut o2 = MEMORY_MULTI.bootstrap_oracle(&paths);
        for op in &ops {
            apply_oracle_only(op, &mut o2);
        }
        for lens in crate::op::INT {
            for t in [0i64, 100, 500, 1500] {
                assert_eq!(o1.at(lens, t), o2.at(lens, t));
            }
        }
    }

    #[test]
    fn ttl_at_matches_oracle_with_fixed_wall_clock() {
        let _clock_guard = crate::sim::lock_clock();
        libtau::wall_clock::set_fixed_now_secs(crate::oracle::DST_NOW_SECS);
        let paths = crate::profile::ProfileWorkspace::new(MEMORY_MULTI).paths;
        let mut target = MEMORY_MULTI.bootstrap_executor(&paths);
        let mut model = MEMORY_MULTI.bootstrap_oracle(&paths);
        let append = Op::Append {
            lens: "a".into(),
            data: crate::op::Payload::Int(vec![(0, 100, 42)]),
        };
        let divs = apply_dual_executor(0, &append, &mut target, &mut model);
        assert!(divs.is_empty(), "unexpected divergences: {divs:?}");
        let divs = apply_dual_executor(
            1,
            &Op::Ttl {
                lens: "a".into(),
                secs: Some(9_999_999_999),
            },
            &mut target,
            &mut model,
        );
        assert!(divs.is_empty(), "{divs:?}");
        let got = crate::harness::exec(&mut target, "AT LENS a 50");
        assert_eq!(
            match got {
                libtau::Output::Value(v) => v,
                _ => panic!("expected value"),
            },
            model.at("a", 50)
        );
    }

    #[hegel::test]
    fn pbt_append_at_matches_oracle(tc: TestCase) {
        let paths = crate::profile::ProfileWorkspace::new(MEMORY_MULTI).paths;
        let mut target = MEMORY_MULTI.bootstrap_executor(&paths);
        let mut model = MEMORY_MULTI.bootstrap_oracle(&paths);
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
        let taus = crate::op::gen_int_taus(
            &mut StdRng::seed_from_u64(tc.draw(gs::integers::<u64>())),
            n,
        );
        let lens = "a".to_string();
        let op = Op::Append {
            lens: lens.clone(),
            data: crate::op::Payload::Int(taus),
        };
        let divs = apply_dual_executor(0, &op, &mut target, &mut model);
        assert!(divs.is_empty(), "{divs:?}");
        let t = tc.draw(gs::integers::<i64>().min_value(0).max_value(3000));
        let got = crate::harness::exec(&mut target, &format!("AT LENS {lens} {t}"));
        assert_eq!(
            match got {
                libtau::Output::Value(v) => v,
                _ => panic!("expected value"),
            },
            model.at(&lens, t)
        );
    }
}
