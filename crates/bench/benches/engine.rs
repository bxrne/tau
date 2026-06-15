//! In-process engine benchmarks: drive `libtau::Executor` directly, with no
//! TCP/TLS involved. These are the headline domain-model numbers.

use bench::grid::ConfigCell;
use bench::runner::{apply_setup, measure_engine};
use bench::workload::{DEFAULT_SEED, ValueType, WorkloadKind};

const SCALE: usize = 200;

fn main() {
    divan::main();
}

#[divan::bench(args = [
    WorkloadKind::AppendHeavy,
    WorkloadKind::CorrectionHeavy,
    WorkloadKind::PointQuery,
    WorkloadKind::RangeScan,
    WorkloadKind::ReduceAgg,
    WorkloadKind::DerivedLens,
    WorkloadKind::CompactionStress,
])]
fn memory(bencher: divan::Bencher, kind: WorkloadKind) {
    bencher
        .with_inputs(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let cell = ConfigCell::default();
            let mut executor = cell.build_executor(dir.path()).expect("executor");
            let workload = kind.build(SCALE, ValueType::Int, DEFAULT_SEED);
            apply_setup(&mut executor, &workload);
            (dir, executor, workload)
        })
        .bench_values(|(_dir, mut executor, workload)| {
            measure_engine(&mut executor, &workload, "memory")
        });
}

#[divan::bench(args = [
    WorkloadKind::AppendHeavy,
    WorkloadKind::PointQuery,
    WorkloadKind::CompactionStress,
])]
fn disk(bencher: divan::Bencher, kind: WorkloadKind) {
    bencher
        .with_inputs(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let cell = ConfigCell {
                backend: bench::grid::Backend::Disk,
                ..ConfigCell::default()
            };
            let mut executor = cell.build_executor(dir.path()).expect("executor");
            let workload = kind.build(SCALE, ValueType::Int, DEFAULT_SEED);
            apply_setup(&mut executor, &workload);
            (dir, executor, workload)
        })
        .bench_values(|(_dir, mut executor, workload)| {
            measure_engine(&mut executor, &workload, "disk")
        });
}
