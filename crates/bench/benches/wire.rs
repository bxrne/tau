//! Over-the-wire benchmarks: drive the real `tau` TCP server via
//! `tau::harness::EphemeralServer`. These measure end-to-end cost including
//! line parsing, optional TLS, and optional auth - not just the engine.

use std::sync::{Arc, RwLock};

use bench::grid::ConfigCell;
use bench::runner::run_wire;
use bench::workload::{DEFAULT_SEED, ValueType, WorkloadKind};
use tau::harness::EphemeralServer;

const SCALE: usize = 100;

fn main() {
    divan::main();
}

#[divan::bench(args = [
    WorkloadKind::AppendHeavy,
    WorkloadKind::PointQuery,
    WorkloadKind::RangeScan,
])]
fn plain(bencher: divan::Bencher, kind: WorkloadKind) {
    run_wire_bench(bencher, kind, ConfigCell::default());
}

#[divan::bench(args = [
    WorkloadKind::AppendHeavy,
    WorkloadKind::PointQuery,
])]
fn tls_ephemeral(bencher: divan::Bencher, kind: WorkloadKind) {
    let cell = ConfigCell {
        tls: true,
        ..ConfigCell::default()
    };
    run_wire_bench(bencher, kind, cell);
}

#[divan::bench(args = [
    WorkloadKind::AppendHeavy,
    WorkloadKind::PointQuery,
])]
fn auth(bencher: divan::Bencher, kind: WorkloadKind) {
    let cell = ConfigCell {
        auth: true,
        ..ConfigCell::default()
    };
    run_wire_bench(bencher, kind, cell);
}

fn run_wire_bench(bencher: divan::Bencher, kind: WorkloadKind, cell: ConfigCell) {
    bencher
        .with_inputs(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let executor = cell.build_executor(dir.path()).expect("executor");
            let executor = Arc::new(RwLock::new(executor));
            let server = EphemeralServer::spawn(executor, cell.harness_opts())
                .expect("spawn ephemeral server");
            let workload = kind.build(SCALE, ValueType::Int, DEFAULT_SEED);
            (dir, server, workload)
        })
        .bench_values(|(_dir, server, workload)| {
            run_wire(server.addr, cell.tls, cell.auth, &workload, cell.name).expect("run_wire")
        });
}
