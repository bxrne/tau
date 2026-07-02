//! Concurrent DST phase and shared executor helper.

use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use libdst::{Divergence, RunResult};
use libtau::{Executor, Output, parse, wall_clock};
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use tracing::error;

use crate::apply::apply_dual;
use crate::op::{self, Op, Payload, gen_int_taus};
use crate::oracle::Oracle;
use crate::target::DirectExecutor;

pub fn exec(ex: &mut Executor, q: &str) -> Output {
    let (_, stmt) = parse(q).unwrap_or_else(|_| panic!("parse failed: {q}"));
    ex.exec(&stmt)
        .unwrap_or_else(|e| panic!("exec error: {e:?}\n  stmt: {q}"))
}

fn exec_read(ex: &Executor, q: &str) -> Result<Output, libtau::ExecError> {
    let (_, stmt) = parse(q).unwrap_or_else(|_| panic!("parse failed: {q}"));
    ex.exec_read(&stmt)
}

/// Concurrent readers + writer; in-memory target with isolated oracle.
pub fn run_concurrent(n_writes: usize, n_readers: usize, seed: u64) -> RunResult {
    // Serialize against other simulations that drive the global virtual clock.
    let _clock_guard = crate::sim::lock_clock();
    wall_clock::set_fixed_now_secs(crate::oracle::DST_NOW_SECS);
    let ex = {
        let mut e = Executor::new();
        exec(&mut e, "CREATE DATABASE conc");
        exec(&mut e, "CREATE LENS a int");
        exec(&mut e, "CREATE LENS b int");
        e
    };
    let executor = Arc::new(RwLock::new(ex));
    let stop = Arc::new(RwLock::new(false));
    let reader_errors: Arc<RwLock<usize>> = Arc::new(RwLock::new(0));

    let reader_handles: Vec<_> = (0..n_readers)
        .map(|id| {
            let ex_arc = Arc::clone(&executor);
            let stop_arc = Arc::clone(&stop);
            let errs_arc = Arc::clone(&reader_errors);
            thread::spawn(move || {
                let mut rng = StdRng::seed_from_u64(seed ^ (id as u64).wrapping_mul(0xDEAD_BEEF));
                loop {
                    if *stop_arc.read().expect("stop flag lock poisoned") {
                        break;
                    }
                    let lens = if rng.gen_bool(0.5) { "a" } else { "b" };

                    let s = rng.gen_range(-50..2500);
                    let e = s + rng.gen_range(1..500);
                    let range_result = {
                        let guard = ex_arc.read().unwrap();
                        exec_read(&guard, &format!("RANGE LENS {lens} {s} {e}"))
                    };
                    if let Ok(Output::Range(segs)) = range_result {
                        let mut prev: Option<i64> = None;
                        for &(ss, se, _) in &segs {
                            if ss >= se {
                                error!(reader = id, ss, se, "INVALID SEGMENT");
                                *errs_arc.write().expect("error counter lock poisoned") += 1;
                            }
                            if let Some(prev_end) = prev
                                && ss < prev_end
                            {
                                error!(reader = id, prev_end, ss, "OVERLAP");
                                *errs_arc.write().expect("error counter lock poisoned") += 1;
                            }
                            prev = Some(se);
                        }
                    }

                    thread::sleep(Duration::from_micros(10));
                }
            })
        })
        .collect();

    let mut model = Oracle::new();
    model.create_lens("a");
    model.create_lens("b");

    let mut write_rng = StdRng::seed_from_u64(seed);
    let mut first_div: Option<Divergence> = None;
    let mut apply_errs = 0usize;
    for i in 0..n_writes {
        let lens = if write_rng.gen_bool(0.5) { "a" } else { "b" };
        let count = write_rng.gen_range(1..=4usize);
        let taus = gen_int_taus(&mut write_rng, count);
        let data = Payload::Int(taus);
        {
            let mut guard = executor.write().expect("executor lock poisoned");
            let divs = apply_dual(
                i,
                &Op::Append {
                    lens: lens.to_string(),
                    data,
                },
                &mut DirectExecutor(&mut guard),
                &mut model,
            );
            apply_errs += divs.len();
            if first_div.is_none() {
                first_div = divs.into_iter().next();
            }
        }
        thread::sleep(Duration::from_micros(50));
    }

    *stop.write().expect("stop flag lock poisoned") = true;
    for h in reader_handles {
        h.join().expect("reader thread panicked");
    }

    let reader_errs = *reader_errors.read().expect("reader errors lock poisoned");
    let mut reconcile_errs = 0;
    {
        let guard = executor.read().expect("executor lock poisoned");
        for lens in op::INT {
            for &t in &[100i64, 500, 1000, 1500, 2000, 2499] {
                if let Ok(Output::Value(got)) = exec_read(&guard, &format!("AT LENS {lens} {t}"))
                    && got != model.at(lens, t)
                {
                    error!(lens, t, ?got, "RECONCILE MISMATCH AT");
                    reconcile_errs += 1;
                }
            }
        }
    }

    RunResult {
        errors: reader_errs + reconcile_errs + apply_errs,
        ops_run: n_writes,
        first_divergence: first_div,
    }
}
