//! Lock-step apply to target (SUT) and isolated oracle (model).

use libdst::Divergence;
use libtau::{Executor, Output, Value};
use tracing::{debug, warn};

use crate::op::Op;
use crate::oracle::{Oracle, Ts};
use crate::target::{DirectExecutor, Target};

#[allow(dead_code)]
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
        Op::Append { lens, data } => {
            if exec_ok(target, &data.batch_sql(lens), step, &mut divs) {
                apply_model_after_target(op, model);
                debug!(step, lens, n = data.len(), "APPEND");
            }
        }
        Op::At { lens, t } => {
            let Some(got) = exec_output(target, &format!("AT LENS {lens} {t}"), step, &mut divs)
            else {
                return divs;
            };
            let expected = model.at(lens, *t);
            check_at(step, &format!("AT {lens} {t}"), &got, expected, &mut divs);
        }
        Op::Range { lens, start, end } => {
            if start >= end {
                return divs;
            }
            let Some(got) = exec_output(
                target,
                &format!("RANGE LENS {lens} {start} {end}"),
                step,
                &mut divs,
            ) else {
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
            let Some(got) = exec_output(
                target,
                &format!("REDUCE LENS {lens} {start} {end} USING {func}"),
                step,
                &mut divs,
            ) else {
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
        Op::CreateLens { name, ty } => {
            if exec_ok(target, &format!("CREATE LENS {name} {ty}"), step, &mut divs) {
                apply_model_after_target(op, model);
                debug!(step, name, ty, "CREATE LENS");
            }
        }
        Op::DropLens { name } => {
            if exec_ok(target, &format!("DROP LENS {name}"), step, &mut divs) {
                apply_model_after_target(op, model);
                debug!(step, name, "DROP LENS");
            }
        }
        Op::Derive { name, spec } => {
            if exec_ok(
                target,
                &format!("DERIVE LENS {name} AS {} + {}", spec.a, spec.b),
                step,
                &mut divs,
            ) {
                apply_model_after_target(op, model);
                debug!(step, name, a = spec.a, b = spec.b, "DERIVE");
            }
        }
        Op::Ttl { lens, secs } => match secs {
            Some(s) => {
                if exec_ok(target, &format!("SET TTL LENS {lens} {s}"), step, &mut divs) {
                    apply_model_after_target(op, model);
                    debug!(step, lens, secs = s, "SET TTL");
                }
            }
            None => {
                if exec_ok(target, &format!("UNSET TTL LENS {lens}"), step, &mut divs) {
                    apply_model_after_target(op, model);
                    debug!(step, lens, "UNSET TTL");
                }
            }
        },
        Op::UseDb(db) => {
            if exec_ok(target, &format!("USE DATABASE {db}"), step, &mut divs) {
                apply_model_after_target(op, model);
                debug!(step, db, "USE DATABASE");
            }
        }
        Op::StartTransaction | Op::Commit | Op::Rollback => {
            if matches!(op, Op::StartTransaction) && target.is_in_transaction() {
                target.rollback_open_transaction();
            }
            if exec_ok(target, op.sql_line(), step, &mut divs) {
                apply_model_after_target(op, model);
                debug!(step, sql = op.sql_line(), "TX");
            }
        }
    }
    divs
}

impl Op {
    pub fn sql_line(&self) -> &'static str {
        match self {
            Op::StartTransaction => "START TRANSACTION",
            Op::Commit => "COMMIT",
            Op::Rollback => "ROLLBACK",
            _ => panic!("not a bare SQL op"),
        }
    }
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
            ops.push(btree::pick(&mut rng, &o1, false, false));
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
