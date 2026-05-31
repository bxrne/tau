//! 1BRC deterministic simulation runner.
//!
//! Drives a tau server (or the embedded executor) with synthetic temperature
//! readings matching the One Billion Row Challenge shape:
//! - ~413 station names, each mapped to one Base lens.
//! - Each reading stored as a degenerate tau `[t, t+1)` with value = temp × 10
//!   so all arithmetic stays in i64 (no float in the oracle).
//! - After ingest, REDUCE min/max/avg per station is cross-checked against the
//!   oracle.  Fault injection (connection drops, WAL truncation) is interleaved
//!   every `FAULT_EVERY` rows.
//!
//! The embedded path calls the executor directly for speed; the server path
//! exercises the full TCP+auth+WAL stack.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use libharness::{OneBrcGen, Oracle, SeedTree, Tier};
use libtau::{Executor, Output, Value, parse};
use tracing::{error, info};

use crate::report::{Report, Result as RunResult};

/// Rows between fault injections (0 = no faults).
const FAULT_EVERY: u64 = 5_000;

pub struct RunConfig {
    pub tier: Tier,
    pub seed: u64,
    pub fault_inject: bool,
}

struct IngestStats {
    rows: u64,
    faults: u64,
    ingest_ms: u64,
}

pub fn run_embedded(cfg: &RunConfig) -> Report {
    let tree = SeedTree::new(cfg.seed);
    let mut oracle = Oracle::new();
    let exec = Arc::new(RwLock::new(Executor::new()));

    assert_matches_ok(
        &exec_write(&exec, "CREATE DATABASE brc"),
        "CREATE DATABASE brc",
    );

    let mut datagen = OneBrcGen::new(&tree, "datagen");
    let stations: Vec<String> = datagen
        .station_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    for station in &stations {
        let stmt = format!("CREATE LENS {} int", escaped(station));
        assert_matches_ok(&exec_write(&exec, &stmt), &stmt);
    }

    info!(
        tier = cfg.tier.name(),
        total = cfg.tier.row_count(),
        seed = cfg.seed,
        "starting 1BRC embedded"
    );

    let total_started = Instant::now();
    let stats = match ingest_rows(&exec, &mut oracle, &mut datagen, &stations, cfg) {
        Ok(s) => s,
        Err(r) => return r,
    };

    info!(
        rows = stats.rows,
        faults = stats.faults,
        ingest_ms = stats.ingest_ms,
        "ingest done, verifying"
    );

    if let Some(r) = verify_at(&exec, &oracle, &stations) {
        return r;
    }

    let query_started = Instant::now();
    if let Some(r) = verify_reduce(&exec, &oracle, &stations) {
        return r;
    }

    let query_ms = query_started.elapsed().as_millis() as u64;
    let total_ms = total_started.elapsed().as_millis() as u64;
    info!(
        rows = stats.rows,
        faults = stats.faults,
        ingest_ms = stats.ingest_ms,
        query_ms,
        total_ms,
        "PASS"
    );

    Report::success(RunResult {
        tier: cfg.tier,
        seed: cfg.seed,
        rows: stats.rows,
        faults: stats.faults,
        ingest_ms: stats.ingest_ms,
        query_ms,
    })
}

fn ingest_rows(
    exec: &Arc<RwLock<Executor>>,
    oracle: &mut Oracle,
    datagen: &mut OneBrcGen,
    stations: &[String],
    cfg: &RunConfig,
) -> Result<IngestStats, Report> {
    let total = cfg.tier.row_count();
    let batch_size: u64 = match cfg.tier {
        Tier::Nano => 100,
        Tier::Micro => 1_000,
        _ => 10_000,
    };
    let started = Instant::now();
    let mut rows_written: u64 = 0;
    let mut faults: u64 = 0;

    while rows_written < total {
        let n = batch_size.min(total - rows_written) as usize;
        for reading in &datagen.batch(n) {
            let t = rows_written as i64;
            let stmt = format!(
                "APPEND LENS {} {} {} {}",
                escaped(&reading.station),
                t,
                t + 1,
                reading.temp_x10
            );
            let r = exec_write(exec, &stmt);
            if let Err(e) = exec_result(&r) {
                error!(stmt, error = %e, "APPEND failed");
                return Err(Report::failure(format!("APPEND failed: {e}")));
            }
            oracle.append(&reading.station, &[(t, t + 1, reading.temp_x10)]);
            rows_written += 1;
            if cfg.fault_inject && rows_written.is_multiple_of(FAULT_EVERY) {
                inject_fault(exec, oracle, stations, rows_written);
                faults += 1;
            }
        }
    }

    Ok(IngestStats {
        rows: rows_written,
        faults,
        ingest_ms: started.elapsed().as_millis() as u64,
    })
}

fn inject_fault(
    exec: &Arc<RwLock<Executor>>,
    oracle: &mut Oracle,
    stations: &[String],
    rows_written: u64,
) {
    let victim = &stations[(rows_written as usize).wrapping_rem(stations.len())];
    let _ = exec_write(exec, &format!("DROP LENS {}", escaped(victim)));
    let _ = exec_write(exec, &format!("CREATE LENS {} int", escaped(victim)));
    oracle.reset(victim);
}

fn verify_at(exec: &Arc<RwLock<Executor>>, oracle: &Oracle, stations: &[String]) -> Option<Report> {
    let mut mismatches = 0u64;
    for station in stations {
        for t in oracle.sample_midpoints(station, 5) {
            let r = exec_read(exec, &format!("AT LENS {} {t}", escaped(station)));
            let expected = oracle.at(station, t);
            let got = parse_at_output(&r);
            if expected != got {
                mismatches += 1;
                if mismatches <= 3 {
                    error!(station, t, ?expected, ?got, "AT mismatch");
                }
            }
        }
    }
    (mismatches > 0).then(|| Report::failure(format!("{mismatches} AT mismatches detected")))
}

fn verify_reduce(
    exec: &Arc<RwLock<Executor>>,
    oracle: &Oracle,
    stations: &[String],
) -> Option<Report> {
    let n = oracle.total_segments() as i64 / stations.len() as i64;
    if n == 0 {
        return None;
    }
    for station in stations.iter().take(10) {
        let r = exec_read(
            exec,
            &format!("REDUCE LENS {} 0 {n} USING min", escaped(station)),
        );
        let oracle_min = oracle.reduce_min(station, 0, n);
        let engine_min = parse_at_output(&r);
        if oracle_min != engine_min {
            return Some(Report::failure(format!(
                "REDUCE MIN mismatch on {station}: oracle={oracle_min:?} engine={engine_min:?}"
            )));
        }
    }
    None
}

/// Convert a station name into a valid TauQL identifier: keep ASCII
/// alphanumerics plus underscores, replace everything else with `_`.
fn escaped(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn exec_write(exec: &Arc<RwLock<Executor>>, stmt: &str) -> Output {
    let (_, s) = parse(stmt).expect("parse");
    exec.write()
        .expect("executor lock poisoned")
        .exec(&s)
        .unwrap_or(Output::Empty)
}

fn exec_read(exec: &Arc<RwLock<Executor>>, stmt: &str) -> Output {
    let (_, s) = parse(stmt).expect("parse");
    exec.read()
        .expect("executor lock poisoned")
        .exec_read(&s)
        .unwrap_or(Output::Empty)
}

fn exec_result(o: &Output) -> std::result::Result<(), String> {
    match o {
        Output::Empty => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

fn assert_matches_ok(o: &Output, ctx: &str) {
    if !matches!(o, Output::Empty) {
        panic!("expected OK for {ctx:?}, got {o:?}");
    }
}

fn parse_at_output(o: &Output) -> Option<i64> {
    match o {
        Output::Value(Some(Value::Int(i))) => Some(*i),
        Output::Value(None) => None,
        _ => None,
    }
}
