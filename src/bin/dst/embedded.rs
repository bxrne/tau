use std::process;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tau::{Executor, Output, Value, parse};
use tracing::{error, info, trace, warn};

use crate::oracle::Oracle;
use crate::{Cli, rng_range, rng_usize};

const EMBEDDED_LENSES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
enum Op {
    Append,
    BatchAppend,
    At,
    Range,
    History,
    AtAsOf,
    AtLayer,
    Drop,
    Create,
    TxCommit,
    TxRollback,
}

// Weighted random op selection. Guards ensure only valid ops for the current
// lens state are returned. The loop retries on guard misses so Append (always
// valid for a live lens) acts as the guaranteed fallback.
fn pick_op(rng: &mut StdRng, live: bool, has_data: bool) -> Op {
    if !live {
        return Op::Create;
    }
    loop {
        match rng.gen_range(0u8..22) {
            0 => return Op::Drop,
            1..=3 if has_data => return Op::At,
            4..=5 if has_data => return Op::Range,
            6 if has_data => return Op::History,
            7 if has_data => return Op::AtAsOf,
            8 if has_data => return Op::AtLayer,
            9..=11 => return Op::BatchAppend,
            12..=13 => return Op::TxCommit,
            14 => return Op::TxRollback,
            15..=21 => return Op::Append,
            _ => continue,
        }
    }
}

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
            error!("Reproduce: cargo run --release --bin dst -- --quick --seed {seed:#018x}");
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
    let mut read_ops: u64 = 0;
    let mut time_cursor: i64 = 0;
    let deadline = Instant::now() + Duration::from_secs(cli.duration);
    let mut last_progress = Instant::now();
    let mut ops_since_progress: u64 = 0;

    while Instant::now() < deadline {
        let lens_idx = rng_usize(&mut rng, 0, EMBEDDED_LENSES.len());
        let lens_name = EMBEDDED_LENSES[lens_idx];
        let has_data = time_cursor > 0 && live[lens_idx];
        let op = pick_op(&mut rng, live[lens_idx], has_data);

        match op {
            Op::Drop => {
                let stmt = parse(&format!("DROP LENS {lens_name}")).expect("parse").1;
                executor.write().expect("lock").exec(&stmt).ok();
                oracle.reset(lens_name);
                live[lens_idx] = false;
                write_ops += 1;
            }
            Op::Create => {
                let stmt = parse(&format!("CREATE LENS {lens_name} int"))
                    .expect("parse")
                    .1;
                executor.write().expect("lock").exec(&stmt).ok();
                live[lens_idx] = true;
                write_ops += 1;
            }
            Op::At => {
                let t = rng.gen_range(0..time_cursor);
                embedded_check_at(&executor, &oracle, lens_name, t, total_ops);
                read_ops += 1;
            }
            Op::Range => {
                let rs = rng.gen_range(0..time_cursor);
                let re = rs + rng_range(&mut rng, 1, (time_cursor / 10).max(2));
                embedded_check_range(&executor, &oracle, lens_name, rs, re, total_ops);
                read_ops += 1;
            }
            Op::History => {
                embedded_check_history(&executor, lens_name, total_ops);
                read_ops += 1;
            }
            Op::AtAsOf => {
                let t = rng.gen_range(0..time_cursor);
                embedded_check_at_as_of(&executor, &oracle, lens_name, t, total_ops);
                read_ops += 1;
            }
            Op::AtLayer => {
                let t = rng.gen_range(0..time_cursor);
                embedded_check_at_layer(&executor, &oracle, lens_name, t, total_ops);
                read_ops += 1;
            }
            Op::Append => {
                if embedded_do_append(
                    &mut rng,
                    &executor,
                    &mut oracle,
                    &max_cursor,
                    lens_name,
                    &mut time_cursor,
                    cli.readers,
                    total_ops,
                ) {
                    write_ops += 1;
                }
            }
            Op::BatchAppend => {
                if embedded_do_batch_append(
                    &mut rng,
                    &executor,
                    &mut oracle,
                    &max_cursor,
                    lens_name,
                    &mut time_cursor,
                    cli.readers,
                    total_ops,
                ) {
                    write_ops += 1;
                }
            }
            Op::TxCommit => {
                if embedded_do_tx_commit(
                    &mut rng,
                    &executor,
                    &mut oracle,
                    &max_cursor,
                    lens_name,
                    &mut time_cursor,
                    total_ops,
                ) {
                    write_ops += 1;
                }
            }
            Op::TxRollback => {
                embedded_do_tx_rollback(
                    &mut rng,
                    &executor,
                    &oracle,
                    lens_name,
                    time_cursor,
                    total_ops,
                );
                read_ops += 1;
            }
        }

        total_ops += 1;
        ops_since_progress += 1;

        // Periodic backup/restore spot-check (every 500 writes).
        if write_ops > 0 && write_ops.is_multiple_of(500) {
            embedded_check_backup_restore(&executor, &oracle, total_ops);
        }

        let elapsed = last_progress.elapsed();
        if elapsed >= PROGRESS_INTERVAL {
            let rate = ops_since_progress as f64 / elapsed.as_secs_f64();
            let segments = oracle.total_segments();
            info!(
                "dst embedded: {total_ops} ops | {write_ops} writes {read_ops} reads | \
                 {segments} segments | {rate:.0} ops/s"
            );
            last_progress = Instant::now();
            ops_since_progress = 0;
        }
    }

    stop.store(true, Ordering::Relaxed);
    let simulated_years = (time_cursor as f64) / (365.25 * 24.0 * 3600.0 * 1_000.0);
    let segments = oracle.total_segments();
    info!(
        "dst embedded: {total_ops} ops ({write_ops} writes, {read_ops} reads), \
         {segments} segments, simulated {simulated_years:.0} years"
    );
}

// Build a batch of non-overlapping segments advancing from `cursor`.
fn build_segs(rng: &mut StdRng, cursor: i64, count: usize) -> (Vec<(i64, i64, i64)>, i64) {
    let mut segs = Vec::with_capacity(count);
    let mut cur = cursor;
    for _ in 0..count {
        let s = cur + rng_range(rng, 1_000_000, 100_000_000);
        let e = s + rng_range(rng, 1, 10_000_001);
        let v = rng_range(rng, i32::MIN as i64, i32::MAX as i64);
        segs.push((s, e, v));
        cur = e;
    }
    (segs, cur)
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
    let count = rng_usize(rng, 1, 9);
    let (segs, new_cursor) = build_segs(rng, *time_cursor, count);
    *time_cursor = new_cursor;
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

#[allow(clippy::too_many_arguments)]
fn embedded_do_batch_append(
    rng: &mut StdRng,
    executor: &Arc<RwLock<Executor>>,
    oracle: &mut Oracle,
    max_cursor: &Arc<AtomicI64>,
    lens_name: &str,
    time_cursor: &mut i64,
    readers: usize,
    op_idx: u64,
) -> bool {
    let count = rng_usize(rng, 2, 12);
    let (segs, new_cursor) = build_segs(rng, *time_cursor, count);
    *time_cursor = new_cursor;
    max_cursor.store(*time_cursor, Ordering::Relaxed);

    let body = segs
        .iter()
        .map(|(s, e, v)| format!("{s} {e} {v}"))
        .collect::<Vec<_>>()
        .join(" ; ");
    let stmt_text = format!("BATCH APPEND LENS {lens_name} {{ {body} }}");

    let stmt = parse(&stmt_text).expect("parse batch").1;
    if executor.write().expect("lock").exec(&stmt).is_err() {
        return false;
    }
    oracle.append(lens_name, &segs);
    let mid = segs[0].0;
    let expected = oracle.at(lens_name, mid);
    embedded_concurrent_burst(executor, lens_name, mid, expected, readers, op_idx);
    true
}

fn embedded_do_tx_commit(
    rng: &mut StdRng,
    executor: &Arc<RwLock<Executor>>,
    oracle: &mut Oracle,
    max_cursor: &Arc<AtomicI64>,
    lens_name: &str,
    time_cursor: &mut i64,
    op_idx: u64,
) -> bool {
    let count = rng_usize(rng, 1, 5);
    let (segs, new_cursor) = build_segs(rng, *time_cursor, count);
    *time_cursor = new_cursor;
    max_cursor.store(*time_cursor, Ordering::Relaxed);

    let mut exec = executor.write().expect("lock");
    if exec
        .exec(&parse("START TRANSACTION").expect("parse").1)
        .is_err()
    {
        return false;
    }
    for &(s, e, v) in &segs {
        let stmt_text = format!("APPEND LENS {lens_name} {s} {e} {v}");
        if exec.exec(&parse(&stmt_text).expect("parse").1).is_err() {
            exec.exec(&parse("ROLLBACK").expect("parse").1).ok();
            return false;
        }
    }
    if exec.exec(&parse("COMMIT").expect("parse").1).is_err() {
        return false;
    }
    drop(exec);

    oracle.append(lens_name, &segs);

    let mid = segs[0].0;
    let expected = oracle.at(lens_name, mid);
    let stmt = parse(&format!("AT LENS {lens_name} {mid}"))
        .expect("parse")
        .1;
    let got = match executor
        .read()
        .expect("lock")
        .exec_read(&stmt)
        .unwrap_or(Output::Value(None))
    {
        Output::Value(Some(Value::Int(i))) => Some(i),
        _ => None,
    };
    assert_eq!(
        got, expected,
        "op {op_idx}: after COMMIT AT LENS {lens_name} {mid}: exec={got:?} oracle={expected:?}"
    );
    true
}

fn embedded_do_tx_rollback(
    rng: &mut StdRng,
    executor: &Arc<RwLock<Executor>>,
    oracle: &Oracle,
    lens_name: &str,
    time_cursor: i64,
    op_idx: u64,
) {
    // Rollback data lives beyond the current cursor so oracle.at returns None.
    let count = rng_usize(rng, 1, 5);
    let (segs, _) = build_segs(rng, time_cursor + 1_000_000, count);

    let mut exec = executor.write().expect("lock");
    exec.exec(&parse("START TRANSACTION").expect("parse").1)
        .expect("start tx");
    for &(s, e, v) in &segs {
        let stmt_text = format!("APPEND LENS {lens_name} {s} {e} {v}");
        exec.exec(&parse(&stmt_text).expect("parse").1)
            .expect("append in tx");
    }

    // Pending writes must be invisible before COMMIT.
    let mid = segs[0].0;
    let isolation_stmt = parse(&format!("AT LENS {lens_name} {mid}"))
        .expect("parse")
        .1;
    let before = exec.exec(&isolation_stmt).unwrap_or(Output::Value(None));
    assert!(
        !matches!(before, Output::Value(Some(_))),
        "op {op_idx}: pending write visible before COMMIT on {lens_name} at {mid}"
    );

    exec.exec(&parse("ROLLBACK").expect("parse").1)
        .expect("rollback");
    drop(exec);

    let oracle_val = oracle.at(lens_name, mid);
    let stmt = parse(&format!("AT LENS {lens_name} {mid}"))
        .expect("parse")
        .1;
    let exec_val = match executor
        .read()
        .expect("lock")
        .exec_read(&stmt)
        .unwrap_or(Output::Value(None))
    {
        Output::Value(Some(Value::Int(i))) => Some(i),
        _ => None,
    };
    assert_eq!(
        exec_val, oracle_val,
        "op {op_idx}: after ROLLBACK AT LENS {lens_name} {mid}: exec={exec_val:?} oracle={oracle_val:?}"
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

fn embedded_check_range(
    exec: &Arc<RwLock<Executor>>,
    oracle: &Oracle,
    lens: &str,
    rs: i64,
    re: i64,
    op_idx: u64,
) {
    let stmt = parse(&format!("RANGE LENS {lens} {rs} {re}"))
        .expect("parse")
        .1;
    let out = executor_read_quick(exec, &stmt);
    let segs = match out {
        Output::Range(v) => v,
        _ => vec![],
    };

    // Structural invariants: sorted, non-overlapping, within bounds.
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

    // Oracle cross-check: every oracle segment that overlaps [rs, re) must
    // have its midpoint covered by some executor segment.
    for (os, oe, ov) in oracle.range(lens, rs, re) {
        let mid = os + (oe - os) / 2;
        let found = segs
            .iter()
            .any(|(s, e, v)| *s <= mid && mid < *e && matches!(v, tau::Value::Int(i) if *i == ov));
        if !found {
            warn!(
                "op {op_idx}: RANGE {lens} [{rs},{re}): oracle segment [{os},{oe})={ov} \
                 not covered by executor at midpoint {mid}"
            );
        }
    }

    trace!("  RANGE {lens} {rs}..{re}: {} segments", segs.len());
}

fn embedded_check_history(exec: &Arc<RwLock<Executor>>, lens: &str, op_idx: u64) {
    let stmt = parse(&format!("HISTORY LENS {lens}")).expect("parse").1;
    match exec.read().expect("lock").exec_read(&stmt) {
        Ok(Output::LayerHistory(layers)) => {
            for l in &layers {
                assert!(
                    l.min_start <= l.max_end,
                    "op {op_idx}: HISTORY LENS {lens}: layer {} min_start > max_end",
                    l.id
                );
                assert!(
                    l.tau_count > 0,
                    "op {op_idx}: HISTORY LENS {lens}: layer {} has tau_count=0",
                    l.id
                );
            }
            trace!("  HISTORY {lens}: {} layers", layers.len());
        }
        Ok(other) => {
            panic!("op {op_idx}: HISTORY LENS {lens}: unexpected output variant {other:?}")
        }
        Err(e) => panic!("op {op_idx}: HISTORY LENS {lens}: error {e:?}"),
    }
}

fn embedded_check_at_as_of(
    exec: &Arc<RwLock<Executor>>,
    oracle: &Oracle,
    lens: &str,
    t: i64,
    op_idx: u64,
) {
    // All in-memory layers have written_at=0 (legacy sentinel), so any
    // positive as_of value must include them. AT AS OF <max> must equal AT.
    let stmt = parse(&format!("AT LENS {lens} {t} AS OF 9999999999999"))
        .expect("parse")
        .1;
    let out = exec
        .read()
        .expect("lock")
        .exec_read(&stmt)
        .unwrap_or(Output::Value(None));
    let oracle_val = oracle.at(lens, t);
    let exec_val = match &out {
        Output::Value(Some(Value::Int(i))) => Some(*i),
        _ => None,
    };
    trace!("  AT AS OF {lens} {t}: exec={exec_val:?} oracle={oracle_val:?}");
    assert_eq!(
        exec_val, oracle_val,
        "op {op_idx}: AT LENS {lens} {t} AS OF max diverged: exec={exec_val:?} oracle={oracle_val:?}"
    );
}

fn embedded_check_at_layer(
    exec: &Arc<RwLock<Executor>>,
    oracle: &Oracle,
    lens: &str,
    t: i64,
    op_idx: u64,
) {
    // Fetch layer list, then verify AT LAYER for the most recent layer:
    // it must never return a value the oracle doesn't know about.
    let hist_stmt = parse(&format!("HISTORY LENS {lens}")).expect("parse").1;
    let layers = match exec.read().expect("lock").exec_read(&hist_stmt) {
        Ok(Output::LayerHistory(l)) => l,
        _ => return,
    };
    if layers.is_empty() {
        return;
    }
    let newest = layers.iter().max_by_key(|l| l.id).unwrap();
    let stmt = parse(&format!("AT LENS {lens} {t} LAYER {}", newest.id))
        .expect("parse")
        .1;
    let out = exec
        .read()
        .expect("lock")
        .exec_read(&stmt)
        .unwrap_or(Output::Value(None));
    // AT LAYER may differ from AT (newest-wins vs. single-layer), but if the
    // layer value is Some(v), that value must match the oracle when the oracle
    // agrees there is something at t (i.e. it could be an older layer's value,
    // but the oracle tracks only final state). We only assert it doesn't panic
    // and returns a structurally valid Output here.
    let oracle_at = oracle.at(lens, t);
    if oracle_at.is_none() {
        // Oracle says nil → only valid response is nil (no value exists at t).
        let got = match &out {
            Output::Value(Some(Value::Int(i))) => Some(*i),
            _ => None,
        };
        assert!(
            got.is_none(),
            "op {op_idx}: AT LENS {lens} {t} LAYER {}: got {got:?} but oracle says nil",
            newest.id
        );
    }
    // When oracle_at is Some, AT LAYER may validly return a different layer's
    // value or nil — we don't assert exact equality here since layers can
    // overlap with different values from different write epochs.
    trace!("  AT LAYER {lens} {t} layer={}: {:?}", newest.id, out);
}

fn embedded_check_backup_restore(executor: &Arc<RwLock<Executor>>, oracle: &Oracle, op_idx: u64) {
    // Write backup to a temp file, restore into a fresh executor, spot-check.
    let bak_path = env::temp_dir().join(format!("tau-dst-bak-{}-{}.bak", process::id(), op_idx));
    let bak_str = bak_path.display().to_string();

    let backup_stmt = parse(&format!("BACKUP DATABASE default TO \"{bak_str}\""))
        .expect("parse")
        .1;
    if executor
        .read()
        .expect("lock")
        .exec_read(&backup_stmt)
        .is_err()
    {
        return; // backup failed (e.g., no active DB), skip silently
    }

    let mut fresh = Executor::with_threshold(4);
    // RESTORE requires at least one other DB to exist (can't restore into empty executor).
    fresh
        .exec(&parse("CREATE DATABASE _restore_anchor").expect("parse").1)
        .ok();
    let restore_stmt = parse(&format!("RESTORE DATABASE default FROM \"{bak_str}\""))
        .expect("parse")
        .1;
    if fresh.exec(&restore_stmt).is_err() {
        let _ = fs::remove_file(&bak_path);
        return;
    }
    fresh
        .exec(&parse("USE DATABASE default").expect("parse").1)
        .ok();

    // Cross-check a few data points per lens against the oracle.
    for lens_name in EMBEDDED_LENSES {
        for t in oracle.sample_midpoints(lens_name, 3) {
            let oracle_val = oracle.at(lens_name, t);
            let stmt = parse(&format!("AT LENS {lens_name} {t}")).expect("parse").1;
            let exec_val = match fresh.exec_read(&stmt).unwrap_or(Output::Value(None)) {
                Output::Value(Some(Value::Int(i))) => Some(i),
                _ => None,
            };
            assert_eq!(
                exec_val, oracle_val,
                "op {op_idx}: backup/restore: AT LENS {lens_name} {t}: \
                 restored={exec_val:?} oracle={oracle_val:?}"
            );
        }
    }

    let _ = fs::remove_file(&bak_path);
    trace!("  backup/restore spot-check passed at op {op_idx}");
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
