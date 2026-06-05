use clap::{Parser, ValueEnum};

use crate::profile::SuiteTier;

/// Profile matrix tier (see `ProfileSpec::all_for_tier`).
#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum TierCli {
    /// Five representative direct cells (default with `--ci`).
    Smoke,
    /// All direct engine cells (default without `--ci` / `--tier`).
    Standard,
    /// Standard + wire plain/TLS/auth over in-memory server.
    Nightly,
}

impl From<TierCli> for SuiteTier {
    fn from(t: TierCli) -> Self {
        match t {
            TierCli::Smoke => SuiteTier::Smoke,
            TierCli::Standard => SuiteTier::Standard,
            TierCli::Nightly => SuiteTier::Nightly,
        }
    }
}

#[derive(Parser)]
#[command(
    version,
    about = "Tau deterministic simulation test (all libtau backends, faults always on)"
)]
pub struct Cli {
    /// RNG seed (random if omitted; always logged for replay)
    #[arg(long)]
    pub seed: Option<u64>,

    /// Sequential operations per storage profile (ignored for concurrent writes when `--ci`)
    #[arg(long, default_value = "2000")]
    pub ops: usize,

    /// Reader threads in the concurrent phase (`0` skips unless `--ci`)
    #[arg(long, default_value = "0")]
    pub concurrency: usize,

    /// CI preset: profile-specific op counts and default concurrency `4` when unset
    #[arg(long)]
    pub ci: bool,

    /// Profile matrix tier (`smoke`, `standard`, `nightly`). Default: `smoke` with `--ci`, else `standard`.
    #[arg(long, value_enum)]
    pub tier: Option<TierCli>,
}

impl Cli {
    pub fn suite_tier(&self) -> SuiteTier {
        self.tier.map(SuiteTier::from).unwrap_or(if self.ci {
            SuiteTier::Smoke
        } else {
            SuiteTier::Standard
        })
    }
    pub fn ops_for_profile(&self, profile_ci_ops: usize) -> usize {
        if self.ci { profile_ci_ops } else { self.ops }
    }

    pub fn concurrent_readers(&self) -> usize {
        if self.ci && self.concurrency == 0 {
            4
        } else {
            self.concurrency
        }
    }

    pub fn concurrent_writes(&self) -> usize {
        if self.ci { 500 } else { self.ops }
    }
}
