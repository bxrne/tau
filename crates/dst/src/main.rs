mod apply;
mod btree;
mod cli;
mod harness;
mod op;
mod oracle;
mod profile;
mod sim;
mod target;

use clap::Parser;
use libdst::SuiteResult;
use rand::SeedableRng;
use rand::rngs::StdRng;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use cli::Cli;
use harness::run_concurrent;
use profile::ProfileSpec;
use sim::run_profile;

fn main() {
    let cli = Cli::parse();

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,dst=info,libtau::storage=error"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let seed = cli.seed.unwrap_or_else(rand::random);
    let tier = cli.suite_tier();
    let readers = cli.concurrent_readers();

    let mut suite = SuiteResult::default();

    let mut failed_profiles: Vec<(String, usize)> = Vec::new();

    info!(
        seed,
        ci = cli.ci,
        tier = ?tier,
        ops = cli.ops,
        concurrency = readers,
        "dst start"
    );
    for profile in ProfileSpec::all_for_tier(tier) {
        let n_ops = cli.ops_for_profile(profile.ci_ops());
        let mut profile_rng = StdRng::seed_from_u64(seed);
        info!(profile = %profile.name(), n_ops, seed, tier = ?tier, "profile start");
        let phase = run_profile(profile, n_ops, &mut profile_rng);
        info!(
            profile = %profile.name(),
            errors = phase.errors,
            ops = phase.ops_run,
            passed = phase.passed(),
            "profile end"
        );
        if !phase.passed() {
            failed_profiles.push((profile.name(), phase.errors));
            error!(
                profile = %profile.name(),
                errors = phase.errors,
                ops = phase.ops_run,
                "profile failed"
            );
        }
        suite.absorb(phase);
    }

    if readers > 0 {
        let writes = cli.concurrent_writes();
        info!(readers, writes, "concurrent start");
        let phase = run_concurrent(writes, readers, seed ^ 0xC0FFEE);
        info!(
            errors = phase.errors,
            ops = phase.ops_run,
            passed = phase.passed(),
            "concurrent end"
        );
        if !phase.passed() {
            failed_profiles.push(("concurrent".into(), phase.errors));
            error!(
                errors = phase.errors,
                ops = phase.ops_run,
                "concurrent phase failed"
            );
        }
        suite.absorb(phase);
    }

    if suite.passed() {
        info!(seed, ops_run = suite.ops_run, "PASS");
        std::process::exit(0);
    }

    error!(seed, errors = suite.errors, ops_run = suite.ops_run, "FAIL");
    for (name, errs) in &failed_profiles {
        error!(profile = %name, errors = errs, "profile error tally");
    }
    std::process::exit(1);
}
