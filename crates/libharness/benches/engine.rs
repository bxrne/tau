//! Criterion micro-benchmarks for the tau engine.
//!
//! Measures the pure library cost of AT, RANGE, REDUCE, and APPEND at
//! varying layer counts and dataset sizes.  No TCP, no server, no WAL.
//!
//! Run with:
//!   cargo bench -p libharness
//!
//! Flamegraph (requires cargo-flamegraph):
//!   cargo flamegraph --bench engine -- --bench at/layers/64

use std::sync::{Arc, RwLock};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use libharness::{SeedTree, Tier, datagen::OneBrcGen};
use libtau::{Executor, Output, parse};

/// Build a fresh executor with one database `bench` and one int lens `x`,
/// populated with `layers` layers of `taus_per_layer` taus each.
/// Returns (executor, max_t) — max_t is the exclusive end of all data.
fn build_executor(layers: usize, taus_per_layer: usize) -> (Arc<RwLock<Executor>>, i64) {
    let exec = Arc::new(RwLock::new(Executor::new()));
    run_write(&exec, "CREATE DATABASE bench");
    run_write(&exec, "CREATE LENS x int");
    let mut t = 0i64;
    for _ in 0..layers {
        let mut taus = String::new();
        for i in 0..taus_per_layer {
            if i > 0 {
                taus.push_str(", ");
            }
            taus.push_str(&format!("{} {} {}", t, t + 1, i as i64 % 100));
            t += 1;
        }
        run_write(&exec, &format!("APPEND LENS x {taus}"));
    }
    (exec, t)
}

fn run_write(exec: &Arc<RwLock<Executor>>, stmt: &str) {
    let (_, s) = parse(stmt).unwrap();
    exec.write().unwrap().exec(&s).unwrap();
}

fn run_read(exec: &Arc<RwLock<Executor>>, stmt: &str) -> Output {
    let (_, s) = parse(stmt).unwrap();
    exec.read().unwrap().exec_read(&s).unwrap_or(Output::Empty)
}

fn bench_at(c: &mut Criterion) {
    let mut group = c.benchmark_group("at");
    for &layers in &[1usize, 4, 8, 16, 64] {
        let taus_per = 1_000;
        let (exec, max_t) = build_executor(layers, taus_per);
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new("layers", layers), &layers, |b, _| {
            b.iter_batched(
                || max_t / 2,
                |probe| run_read(&exec, &format!("AT LENS x {probe}")),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("range");
    for &layers in &[1usize, 8, 64] {
        let taus_per = 1_000;
        let (exec, max_t) = build_executor(layers, taus_per);
        // Narrow query: 1% of total range.
        let narrow_end = max_t / 100;
        group.throughput(Throughput::Elements(narrow_end as u64));
        group.bench_with_input(
            BenchmarkId::new("narrow/layers", layers),
            &layers,
            |b, _| {
                b.iter(|| run_read(&exec, &format!("RANGE LENS x 0 {narrow_end}")));
            },
        );
        // Full range.
        group.throughput(Throughput::Elements(max_t as u64));
        group.bench_with_input(BenchmarkId::new("full/layers", layers), &layers, |b, _| {
            b.iter(|| run_read(&exec, &format!("RANGE LENS x 0 {max_t}")));
        });
    }
    group.finish();
}

fn bench_reduce(c: &mut Criterion) {
    let mut group = c.benchmark_group("reduce");
    let taus_per = 10_000;
    let (exec, max_t) = build_executor(8, taus_per);
    for &func in &["min", "max", "avg", "sum", "count"] {
        group.throughput(Throughput::Elements(max_t as u64));
        group.bench_with_input(BenchmarkId::new("func", func), &func, |b, f| {
            b.iter(|| run_read(&exec, &format!("REDUCE LENS x 0 {max_t} USING {f}")));
        });
    }
    group.finish();
}

fn bench_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("append");

    for &batch in &[1usize, 100, 1_000] {
        group.throughput(Throughput::Elements(batch as u64));
        group.bench_with_input(BenchmarkId::new("batch", batch), &batch, |b, &n| {
            b.iter_batched(
                || {
                    let exec = Arc::new(RwLock::new(Executor::new()));
                    run_write(&exec, "CREATE DATABASE bench");
                    run_write(&exec, "CREATE LENS x int");
                    exec
                },
                |exec| {
                    let mut taus = String::new();
                    for i in 0..n {
                        if i > 0 {
                            taus.push_str(", ");
                        }
                        let t = i as i64;
                        taus.push_str(&format!("{t} {} {i}", t + 1));
                    }
                    run_write(&exec, &format!("APPEND LENS x {taus}"));
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn setup_brc_exec(tree: &SeedTree) -> Arc<RwLock<Executor>> {
    let exec = Arc::new(RwLock::new(Executor::new()));
    run_write(&exec, "CREATE DATABASE brc");
    let dg = OneBrcGen::new(tree, "setup");
    for name in dg.station_names() {
        run_write(&exec, &format!("CREATE LENS {} int", safe_name(name)));
    }
    exec
}

fn bench_1brc_ingest(c: &mut Criterion) {
    let mut group = c.benchmark_group("onebrc");
    let tree = SeedTree::new(0x0bab_5eed);
    let rows = Tier::Nano.row_count() as usize;
    group.throughput(Throughput::Elements(rows as u64));

    group.bench_function("nano/append", |b| {
        b.iter_batched(
            || setup_brc_exec(&tree),
            |exec| {
                let mut dg = OneBrcGen::new(&tree, "bench");
                for t in 0..rows {
                    let r = dg.draw();
                    run_write(
                        &exec,
                        &format!(
                            "APPEND LENS {} {t} {} {}",
                            safe_name(&r.station),
                            t + 1,
                            r.temp_x10
                        ),
                    );
                }
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_at,
    bench_range,
    bench_reduce,
    bench_append,
    bench_1brc_ingest
);
criterion_main!(benches);
