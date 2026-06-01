//! Tau deterministic simulation tester — driven by the 1BRC dataset.

mod report;
mod runner;

use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use libharness::Tier;
use runner::Backend;
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

    /// RNG seed (default: time-based).
    #[arg(long)]
    seed: Option<u64>,

    /// Disable fault injection.
    #[arg(long)]
    no_faults: bool,

    /// Tracing log level.
    #[arg(long, default_value = "info")]
    log_level: Level,

    /// Storage backend: embedded (default), wal, tcp.
    #[arg(long, default_value = "embedded")]
    backend: String,
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

    let backend = match cli.backend.as_str() {
        "embedded" => Backend::Embedded,
        "wal" => Backend::Wal,
        "tcp" => Backend::Tcp,
        other => {
            eprintln!("unknown backend {other:?}; valid: embedded wal tcp");
            std::process::exit(1);
        }
    };

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
        backend,
        rows_override: None,
    };

    let report = runner::run(&cfg);
    report.print();
    if !report.ok {
        std::process::exit(1);
    }
}
