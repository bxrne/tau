//! Deterministic Simulation Tester for the tau database.
//!
//! Combines correctness verification, fault injection, and throughput
//! measurement across all server configuration combinations.
//! Supersedes the former standalone bench binary.
//!
//! Two modes:
//!   embedded (--quick): uses the library executor directly, no server
//!     process, very fast, suitable for CI. Simulates centuries of data.
//!   full (default): spawns a real tau server per config cell, drives
//!     traffic over TCP (plain or TLS, with or without auth, WAL on/off),
//!     cross-checks every response against an oracle, injects faults
//!     (connection drops, process crashes, WAL truncation), and scrapes
//!     Prometheus metrics to verify statement counts.
//!
//! Usage:
//!   dst [OPTIONS]
//!
//!   --quick              Embedded mode (CI-suitable, no server processes)
//!   --seed N             RNG seed (default: time-based)
//!   --duration N         Seconds to run embedded mode (default: 30)
//!   --ops N              Operations per cell in full mode (default: 2000)
//!   --readers N          Concurrent reader threads in embedded mode (default: 8)
//!   --fault-interval N   Inject a fault every N ops in full mode (default: 500)
//!   --scratch DIR        Directory for WAL files (default: $TMPDIR)
//!   --out PATH           Write CSV results to PATH
//!   --label NAME         Tag every CSV row (default: "run")
//!   --log-level LEVEL    Log level for tracing (default: info)

mod embedded;
mod full;
mod oracle;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use rand::Rng;
use rand::rngs::StdRng;

#[derive(Parser, Debug)]
#[command(name = "dst", about = "Tau deterministic simulation tester", version)]
struct Cli {
    #[arg(long)]
    quick: bool,

    #[arg(long)]
    seed: Option<u64>,

    #[arg(long, default_value = "30")]
    duration: u64,

    #[arg(long, default_value = "2000")]
    ops: usize,

    #[arg(long, default_value = "8")]
    readers: usize,

    #[arg(long, default_value = "500")]
    fault_interval: usize,

    #[arg(long, value_name = "DIR")]
    scratch: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,

    #[arg(long, default_value = "run")]
    label: String,

    #[arg(long, default_value = "info", value_enum)]
    log_level: tracing::Level,
}

fn main() {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_max_level(cli.log_level)
        .with_target(false)
        .init();
    let seed = cli.seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .subsec_nanos() as u64
    });

    if cli.quick {
        embedded::run_embedded(seed, &cli);
    } else {
        full::run_full(seed, &cli);
    }
}

fn rng_range(rng: &mut StdRng, lo: i64, hi: i64) -> i64 {
    rng.gen_range(lo..hi)
}

fn rng_usize(rng: &mut StdRng, lo: usize, hi: usize) -> usize {
    rng.gen_range(lo..hi)
}
