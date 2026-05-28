use std::process;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tau::{Executor, Output, Value, parse};
use tracing::{error, info, trace};

use crate::oracle::Oracle;
use crate::{Cli, rng_range, rng_usize};

const EMBEDDED_LENSES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];

pub(crate) fn run_embedded(seed: u64, cli: &Cli) {
    info!(
        "dst embedded: seed={seed:#018x} duration={}s readers={}",
        cli.duration, cli.readers
    );
    let result = std::panic::catch_unwind(|| embedded_sim(seed, cli));
    match result {
        Ok(()) => info!("dst embedded: PASS"),
        Err(e) => {
            let msg = e
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| e.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            error!("dst embedded: FAIL  seed={seed:#018x}  {msg}");
            error!("Reproduce: cargo run --release --bin dst -- --quick --seed {seed}");
            process::exit(1);
        }
    }
}

fn spawn_stress_reader(
    seed: u64,
    executor: Arc<RwLock<Executor>>,
    stop: Arc<AtomicBool>,
    max_cursor: Arc<AtomicI64>,
) {
    thread::spawn(move || {
        let mut rng = StdRng::seed_from_u64(seed.wrapping_add(0xdead_beef));
        while !stop.load(Ordering::Relaxed) {
            let lens = EMBEDDED_LENSES[rng.gen_range(0..EMBEDDED_LENSES.len())];
            let hi = max_cursor.load(Ordering::Relaxed).max(1);
            let t = rng.gen_range(0..hi);
            if let Ok((_, stmt)) = parse(&format!("AT LENS {lens} {t}")) {
                let _ = executor_read_quick(&executor, &stmt);
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn embedded_do_append(
    rng: &mut StdRng,
    executor: &Arc<RwLock<Executor>>,
    oracle: &mut Oracle,
    max_cursor: &Arc<AtomicI64>,
    lens_name: &str,
    time_cursor: &mut i64,
    readers: usize,
    op_idx: u64,
) -> bool {
    let batch_size = rng_usize(rng, 1, 9);
    let mut segs: Vec<(i64, i64, i64)> = Vec::with_capacity(batch_size);
    let mut cur = *time_cursor;
    for _ in 0..batch_size {
        let advance = rng_range(rng, 1_000_000, 100_000_000);
        let s = cur + advance;
        let e = s + rng_range(rng, 1, 10_000_001);
        let v = rng_range(rng, i32::MIN as i64, i32::MAX as i64);
        segs.push((s, e, v));
        cur = e;
    }
    *time_cursor = cur;
    max_cursor.store(*time_cursor, Ordering::Relaxed);

    let mut stmt_text = format!("APPEND LENS {lens_name}");
    for (s, e, v) in &segs {
        stmt_text.push_str(&format!(" {s} {e} {v},"));
    }
    stmt_text.pop();

    let stmt = parse(&stmt_text).expect("parse").1;
    if executor.write().expect("lock").exec(&stmt).is_err() {
        return false;
    }
    oracle.append(lens_name, &segs);
    let mid = segs[0].0;
    let expected = oracle.at(lens_name, mid);
    embedded_concurrent_burst(executor, lens_name, mid, expected, readers, op_idx);
    true
}

fn embedded_sim(seed: u64, cli: &Cli) {
    let mut rng = StdRng::seed_from_u64(seed);
    let executor: Arc<RwLock<Executor>> = Arc::new(RwLock::new(Executor::with_threshold(4)));
    let mut oracle = Oracle::new();
    let mut live = [true; 8];

    {
        let mut exec = executor.write().expect("write lock");
        exec.exec(&parse("CREATE DATABASE default").expect("parse").1)
            .expect("create db");
        for name in EMBEDDED_LENSES {
            exec.exec(&parse(&format!("CREATE LENS {name} int")).expect("parse").1)
                .expect("create lens");
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    // max_cursor tracks the high-water mark so the stress reader covers real data.
    let max_cursor = Arc::new(AtomicI64::new(1));
    spawn_stress_reader(
        seed,
        Arc::clone(&executor),
        Arc::clone(&stop),
        Arc::clone(&max_cursor),
    );

    let mut total_ops: u64 = 0;
    let mut write_ops: u64 = 0;
    let mut time_cursor: i64 = 0;
    let mut op_idx: u64 = 0;
    let deadline = Instant::now() + Duration::from_secs(cli.duration);

    while Instant::now() < deadline {
        op_idx += 1;
        let lens_idx = rng_usize(&mut rng, 0, EMBEDDED_LENSES.len());
        let lens_name = EMBEDDED_LENSES[lens_idx];

        match rng.gen_range(0u8..16) {
            0 if live[lens_idx] => {
                let stmt = parse(&format!("DROP LENS {lens_name}")).expect("parse").1;
                executor.write().expect("lock").exec(&stmt).ok();
                oracle.reset(lens_name);
                live[lens_idx] = false;
                write_ops += 1;
            }
            1 if !live[lens_idx] => {
                let stmt = parse(&format!("CREATE LENS {lens_name} int"))
                    .expect("parse")
                    .1;
                executor.write().expect("lock").exec(&stmt).ok();
                live[lens_idx] = true;
                write_ops += 1;
            }
            2..=4 if live[lens_idx] && time_cursor > 0 => {
                let t = rng.gen_range(0..time_cursor);
                embedded_check_at(&executor, &oracle, lens_name, t, op_idx);
            }
            5..=6 if live[lens_idx] && time_cursor > 0 => {
                let rs = rng.gen_range(0..time_cursor);
                let re = rs + rng_range(&mut rng, 1, (time_cursor / 10).max(2));
                embedded_check_range(&executor, lens_name, rs, re, op_idx);
            }
            _ if live[lens_idx] => {
                if !embedded_do_append(
                    &mut rng,
                    &executor,
                    &mut oracle,
                    &max_cursor,
                    lens_name,
                    &mut time_cursor,
                    cli.readers,
                    op_idx,
                ) {
                    continue;
                }
                write_ops += 1;
            }
            _ => {}
        }

        total_ops += 1;
    }

    stop.store(true, Ordering::Relaxed);
    let simulated_years = (time_cursor as f64) / (365.25 * 24.0 * 3600.0 * 1_000.0);
    let segments = oracle.total_segments();
    info!(
        "dst embedded: {total_ops} ops ({write_ops} writes), \
         {segments} segments stored, simulated {simulated_years:.0} years"
    );
}

fn executor_read_quick(exec: &Arc<RwLock<Executor>>, stmt: &tau::Stmt) -> Output {
    exec.read()
        .expect("lock")
        .exec_read(stmt)
        .unwrap_or(Output::Value(None))
}

fn embedded_check_at(
    exec: &Arc<RwLock<Executor>>,
    oracle: &Oracle,
    lens: &str,
    t: i64,
    op_idx: u64,
) {
    let stmt = parse(&format!("AT LENS {lens} {t}")).expect("parse").1;
    let out = executor_read_quick(exec, &stmt);
    let oracle_val = oracle.at(lens, t);
    let exec_val = match &out {
        Output::Value(Some(Value::Int(i))) => Some(*i),
        _ => None,
    };
    trace!("  AT {lens} {t}: exec={exec_val:?} oracle={oracle_val:?}");
    assert_eq!(
        exec_val, oracle_val,
        "op {op_idx}: AT LENS {lens} {t} diverged: executor={exec_val:?} oracle={oracle_val:?}"
    );
}

fn embedded_check_range(exec: &Arc<RwLock<Executor>>, lens: &str, rs: i64, re: i64, op_idx: u64) {
    let stmt = parse(&format!("RANGE LENS {lens} {rs} {re}"))
        .expect("parse")
        .1;
    let out = executor_read_quick(exec, &stmt);
    let segs = match out {
        Output::Range(v) => v,
        _ => vec![],
    };
    let mut prev_end: Option<i64> = None;
    for (s, e, _) in &segs {
        let (s, e) = (*s, *e);
        assert!(
            s < e,
            "op {op_idx}: RANGE {lens} [{rs},{re}): segment [{s},{e}) has start >= end"
        );
        assert!(
            s >= rs,
            "op {op_idx}: RANGE {lens} [{rs},{re}): segment [{s},{e}) extends before range start"
        );
        assert!(
            e <= re,
            "op {op_idx}: RANGE {lens} [{rs},{re}): segment [{s},{e}) extends past range end"
        );
        if let Some(pe) = prev_end {
            assert!(
                s >= pe,
                "op {op_idx}: RANGE {lens} [{rs},{re}): overlapping segments ending at {pe} and starting at {s}"
            );
        }
        prev_end = Some(e);
    }
    trace!("  RANGE {lens} {rs}..{re}: {} segments", segs.len());
}

fn embedded_concurrent_burst(
    exec: &Arc<RwLock<Executor>>,
    lens: &str,
    t: i64,
    expected: Option<i64>,
    n: usize,
    op_idx: u64,
) {
    thread::scope(|s| {
        let handles: Vec<_> = (0..n)
            .map(|_| {
                s.spawn(|| {
                    let stmt = parse(&format!("AT LENS {lens} {t}")).expect("parse").1;
                    match exec
                        .read()
                        .expect("lock")
                        .exec_read(&stmt)
                        .unwrap_or(Output::Value(None))
                    {
                        Output::Value(Some(Value::Int(i))) => Some(i),
                        _ => None,
                    }
                })
            })
            .collect();

        for h in handles {
            let got = h.join().expect("reader panicked");
            assert_eq!(
                got, expected,
                "op {op_idx}: concurrent AT LENS {lens} {t}: reader={got:?} expected={expected:?}"
            );
        }
    });
}
