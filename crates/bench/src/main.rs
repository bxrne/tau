//! `benchtau` - deterministic workload runner.
//!
//! Runs one or all named workloads, at a fixed seed and scale, against
//! either the in-process engine or a live wire connection, for one or more
//! config-grid cells. Prints a table or JSON of throughput and latency
//! percentiles.
//!
//! This is the binary that produces the numbers quoted in the docs, the
//! demo slides, and the capped Docker stack - every number it prints carries
//! its seed, scale, and cell so it can be reproduced.

use std::sync::{Arc, RwLock};

use clap::Parser;

use bench::grid::{ConfigCell, Preset};
use bench::runner::{self, WorkloadResult};
use bench::workload::{DEFAULT_SEED, ValueType, WorkloadKind};

#[derive(Parser, Debug)]
#[command(name = "benchtau", author, version)]
#[command(about = "Run tau benchmark workloads and print throughput/latency results")]
struct Cli {
    /// Which workload to run. Omit to run all workloads.
    #[arg(long, value_enum)]
    workload: Option<WorkloadKind>,

    /// Which layer to drive: the in-process engine, the wire protocol, or both.
    #[arg(long, value_enum, default_value_t = Layer::Both)]
    layer: Layer,

    /// Number of measured operations per workload.
    #[arg(long, default_value_t = 1000)]
    scale: usize,

    /// RNG seed; identical seed + scale produces identical operations.
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,

    /// Value type for lenses (ignored by reduce_agg/derived_lens, which always use int).
    #[arg(long, value_enum, default_value_t = ValueType::Int)]
    value_type: ValueType,

    /// Run a named preset of config-grid cells instead of the default single cell.
    #[arg(long, value_enum)]
    preset: Option<Preset>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    format: Format,

    /// Also write the result (in `format`) to this file. Useful in
    /// containers with no shell to redirect stdout.
    #[arg(long)]
    output: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Layer {
    Engine,
    Wire,
    Both,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Format {
    Table,
    Json,
}

fn main() {
    let cli = Cli::parse();

    let cells = match &cli.preset {
        Some(preset) => preset.cells(),
        None => vec![ConfigCell::default()],
    };

    let kinds: Vec<WorkloadKind> = match cli.workload {
        Some(k) => vec![k],
        None => WorkloadKind::all().to_vec(),
    };

    let mut results = Vec::new();

    for cell in &cells {
        for kind in &kinds {
            let workload = kind.build(cli.scale, cli.value_type, cli.seed);

            if matches!(cli.layer, Layer::Engine | Layer::Both) {
                let dir = tempfile::tempdir().expect("tempdir");
                let mut executor = cell.build_executor(dir.path()).expect("build executor");
                results.push(runner::run_engine(&mut executor, &workload, cell.name));
            }

            if matches!(cli.layer, Layer::Wire | Layer::Both) {
                let dir = tempfile::tempdir().expect("tempdir");
                let executor = cell.build_executor(dir.path()).expect("build executor");
                let executor = Arc::new(RwLock::new(executor));
                let server = tau::harness::EphemeralServer::spawn(executor, cell.harness_opts())
                    .expect("spawn ephemeral server");
                match runner::run_wire(server.addr, cell.tls, cell.auth, &workload, cell.name) {
                    Ok(result) => results.push(result),
                    Err(e) => eprintln!(
                        "warning: wire run for {} / {} failed: {e}",
                        kind.name(),
                        cell.name
                    ),
                }
            }
        }
    }

    let rendered = match cli.format {
        Format::Table => render_table(&results, cli.seed, cli.scale),
        Format::Json => render_json(&results, cli.seed, cli.scale),
    };
    println!("{rendered}");

    if let Some(path) = &cli.output {
        std::fs::write(path, &rendered).expect("write output file");
    }
}

fn render_table(results: &[WorkloadResult], seed: u64, scale: usize) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "seed={seed} scale={scale}");
    let _ = writeln!(
        out,
        "{:<20} {:<24} {:<8} {:>8} {:>14} {:>10} {:>10}",
        "workload", "cell", "layer", "ops", "throughput/s", "p50 (us)", "p99 (us)"
    );
    for r in results {
        let _ = writeln!(
            out,
            "{:<20} {:<24} {:<8} {:>8} {:>14.1} {:>10.2} {:>10.2}",
            r.workload, r.cell, r.layer, r.ops, r.throughput_ops_sec, r.p50_us, r.p99_us
        );
    }
    out.trim_end().to_string()
}

fn render_json(results: &[WorkloadResult], seed: u64, scale: usize) -> String {
    #[derive(serde::Serialize)]
    struct Report<'a> {
        seed: u64,
        scale: usize,
        results: &'a [WorkloadResult],
    }
    let report = Report {
        seed,
        scale,
        results,
    };
    serde_json::to_string_pretty(&report).expect("serialize report")
}
