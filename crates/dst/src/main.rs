//! Tau deterministic simulation tester — driven by the 1BRC dataset.
//!
//! The dataset shape: ~413 station names, each mapped to one Base lens in tau.
//! Every reading is a degenerate tau [t, t+1) with value = temperature × 10
//! (i64 fixed-point). After ingest, REDUCE min/max/avg per station is
//! cross-checked against a BTreeMap oracle. Fault injection (lens drop/recreate
//! simulating a connection reset) is interleaved every few thousand rows.
//!
//! Tiers control dataset size:
//!   nano   10 k rows  — CI correctness smoke (<1 s)
//!   micro  1 M rows   — PR-time sanity
//!   small  100 M rows — nightly / dedicated runner
//!   full   1 B rows   — manual / release benchmarking
//!
//! Usage:
//!   dst [--tier nano|micro|small|full] [--seed N] [--no-faults] [--log-level LEVEL]

mod report;
mod runner;

use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use libharness::Tier;
use tracing::Level;

#[derive(Parser, Debug)]
#[command(
    name = "dst",
    about = "Tau 1BRC deterministic simulation tester",
    version
)]
struct Cli {
    /// Dataset tier: nano (10k), micro (1M), small (100M), full (1B).
    #[arg(long, default_value = "nano")]
    tier: String,

    /// RNG seed (default: time-based, printed on every run for reproducibility).
    #[arg(long)]
    seed: Option<u64>,

    /// Disable fault injection.
    #[arg(long)]
    no_faults: bool,

    /// Tracing log level.
    #[arg(long, default_value = "info")]
    log_level: Level,
}

fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_max_level(cli.log_level)
        .with_target(false)
        .init();

    let tier = Tier::parse(&cli.tier).unwrap_or_else(|| {
        eprintln!("unknown tier {:?}; valid: nano micro small full", cli.tier);
        std::process::exit(1);
    });

    let seed = cli.seed.unwrap_or_else(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        nanos ^ ((std::process::id() as u64) << 32)
    });
    eprintln!("seed: {seed:#x}  (re-run with --seed {seed:#x} to reproduce)");

    let cfg = runner::RunConfig {
        tier,
        seed,
        fault_inject: !cli.no_faults,
    };

    let report = runner::run_embedded(&cfg);
    report.print();
    if !report.ok {
        std::process::exit(1);
    }
}
