//! Benchmark harness for tau time-series database.
//!
//! Drives the executor in-process — no server needed. Hot loops use pre-built
//! `Stmt` values so parse overhead is excluded from measurements.
//!
//! Covered workloads:
//!   append-seq          sequential non-overlapping appends
//!   append-correction   overlapping layer corrections (newest-layer-wins)
//!   at-lookup           random point-in-time lookups
//!   range-scan          sliding-window RANGE queries
//!   range-filter        RANGE ... WHERE queries
//!   mixed               interleaved reads + writes at --read-pct ratio
//!   derived             chained DERIVE lens evaluation
//!   iot-ingest          high-fanout ingestion: many lenses, sequential ticks
//!
//! Backends:
//!   InMemory (default)   always measured
//!   WAL                  opt-in with --with-wal; writes to temp dir

use clap::{Parser, ValueEnum};
use std::time::{Duration, Instant};
use tau::libtau::ql::ast::{BinOp, Expr, Literal, Type};
use tau::{Executor, Stmt};
use tracing::{info, warn};

// Seeded LCG — deterministic

struct Rng(u64);

impl Rng {
    fn seed(s: u64) -> Self {
        Self(s)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
    // uniform in [lo, hi)  —  panics if hi <= lo
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(hi > lo, "range: hi must be > lo");
        lo + (self.next_u64() % (hi - lo) as u64) as i64
    }
    fn bool_p(&mut self, true_pct: u8) -> bool {
        (self.next_u64() % 100) < true_pct as u64
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "tau-bench",
    about = "Tau time-series database benchmark harness (in-process, no server)",
    after_help = "All workloads are deterministic given the same --seed.\n\
                  WAL workloads write to a temp file and are slower due to fsync."
)]
struct Cli {
    /// Workload(s) to run
    #[arg(long, value_enum, default_value = "all")]
    workload: Workload,

    /// Measured operations per workload (warmup not included)
    #[arg(long, default_value = "100000")]
    ops: usize,

    /// Distinct lenses in multi-lens workloads (iot-ingest)
    #[arg(long, default_value = "16")]
    lenses: usize,

    /// Read percentage in the mixed workload [0, 100]
    #[arg(long, default_value = "80", value_parser = clap::value_parser!(u8).range(0..=100))]
    read_pct: u8,

    /// DERIVE chain depth
    #[arg(long, default_value = "4")]
    derive_depth: usize,

    /// RNG seed — controls data layout and access pattern
    #[arg(long, default_value = "42")]
    seed: u64,

    /// Also run each workload with WAL-backed storage (slower; writes to temp)
    #[arg(long)]
    with_wal: bool,

    /// Warmup operations before the measurement window
    #[arg(long, default_value = "1000")]
    warmup: usize,

    /// Output format
    #[arg(long, value_enum, default_value = "text")]
    fmt: Fmt,
}

#[derive(ValueEnum, Debug, Clone, PartialEq)]
enum Workload {
    /// Run all workloads in sequence
    All,
    /// Sequential non-overlapping appends (write throughput)
    AppendSeq,
    /// Overlapping correction appends (newest-layer-wins stress)
    AppendCorrection,
    /// Random point-in-time lookups (AT)
    AtLookup,
    /// Sliding-window range scans (RANGE, 10 % of timeline per query)
    RangeScan,
    /// Range scans with a WHERE predicate (RANGE ... WHERE value > threshold)
    RangeFilter,
    /// Interleaved reads and writes at --read-pct ratio
    Mixed,
    /// Chained DERIVE lens evaluation (depth controlled by --derive-depth)
    Derived,
    /// IoT-style ingestion: many lenses updated every tick
    IotIngest,
}

#[derive(ValueEnum, Debug, Clone)]
enum Fmt {
    Text,
    Json,
    Csv,
}

// Result types

struct Run {
    label: String,
    backend: String,
    ops: usize,
    elapsed: Duration,
    /// Sorted ascending after collection.
    latencies_ns: Vec<u64>,
    rss_before_kb: Option<u64>,
    rss_after_kb: Option<u64>,
}

impl Run {
    fn throughput(&self) -> f64 {
        self.ops as f64 / self.elapsed.as_secs_f64()
    }

    /// p-th percentile latency in microseconds (p in [0, 100]).
    fn pct_us(&self, p: f64) -> f64 {
        if self.latencies_ns.is_empty() {
            return 0.0;
        }
        let i = ((self.latencies_ns.len() as f64 * p / 100.0).ceil() as usize)
            .saturating_sub(1)
            .min(self.latencies_ns.len() - 1);
        self.latencies_ns[i] as f64 / 1_000.0
    }

    fn rss_delta_kb(&self) -> Option<i64> {
        Some(self.rss_after_kb? as i64 - self.rss_before_kb? as i64)
    }
}

/// Execute each statement in `ops`, return a sorted `Run`.
/// `exec.exec()` handles both read-only (AT/RANGE) and mutating statements.
fn time_stmts(exec: &mut Executor, label: &str, backend: &str, ops: &[Stmt]) -> Run {
    let rss_before_kb = rss_kb();
    let mut latencies_ns = Vec::with_capacity(ops.len());
    let t0 = Instant::now();
    for stmt in ops {
        let t = Instant::now();
        exec.exec(stmt).expect("op failed");
        latencies_ns.push(t.elapsed().as_nanos() as u64);
    }
    let elapsed = t0.elapsed();
    let rss_after_kb = rss_kb();
    latencies_ns.sort_unstable();
    Run {
        label: label.to_string(),
        backend: backend.to_string(),
        ops: ops.len(),
        elapsed,
        latencies_ns,
        rss_before_kb,
        rss_after_kb,
    }
}

// helpers

fn rss_kb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in text.lines() {
        if line.starts_with("VmRSS:") {
            return line.split_whitespace().nth(1)?.parse().ok();
        }
    }
    None
}

fn exec_ok(e: &mut Executor, stmt: &Stmt) {
    e.exec(stmt).expect("setup statement failed");
}

struct WalGuard(std::path::PathBuf);
impl Drop for WalGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn make_executor(use_wal: bool) -> (Executor, Option<WalGuard>) {
    if use_wal {
        // Use xorshift to avoid collisions between parallel bench runs.
        let mut x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let path = std::env::temp_dir().join(format!("tau_bench_{}_{x}.wal", std::process::id()));
        let exec = Executor::with_wal(&path).expect("WAL init failed");
        (exec, Some(WalGuard(path)))
    } else {
        (Executor::new(), None)
    }
}

// Worklaods

const TICK: i64 = 10;

fn wl_append_seq(cli: &Cli, exec: &mut Executor, backend: &str, rng: &mut Rng) -> Run {
    exec_ok(
        exec,
        &Stmt::CreateDatabase {
            name: "bench".into(),
        },
    );
    exec_ok(
        exec,
        &Stmt::Create {
            name: "ts".into(),
            ty: Type::Int,
        },
    );

    let warmup = cli.warmup.min(cli.ops);
    let total = warmup + cli.ops;
    let stmts: Vec<Stmt> = (0..total as i64)
        .map(|i| Stmt::Append {
            name: "ts".into(),
            start: i * TICK,
            end: i * TICK + TICK,
            value: Literal::Int(rng.range(1, 9_999)),
        })
        .collect();

    for s in &stmts[..warmup] {
        exec_ok(exec, s);
    }
    time_stmts(exec, "append-seq", backend, &stmts[warmup..])
}

fn wl_append_correction(cli: &Cli, exec: &mut Executor, backend: &str, rng: &mut Rng) -> Run {
    exec_ok(
        exec,
        &Stmt::CreateDatabase {
            name: "bench".into(),
        },
    );
    exec_ok(
        exec,
        &Stmt::Create {
            name: "ts".into(),
            ty: Type::Int,
        },
    );

    let timeline = cli.ops as i64 * TICK;
    // Base coverage so corrections always have a layer to shadow.
    exec_ok(
        exec,
        &Stmt::Append {
            name: "ts".into(),
            start: 0,
            end: timeline,
            value: Literal::Int(0),
        },
    );

    let warmup = cli.warmup.min(cli.ops);
    let total = warmup + cli.ops;
    let stmts: Vec<Stmt> = (0..total)
        .map(|_| {
            let lo = rng.range(0, (timeline - TICK).max(1));
            let max_span = (TICK * 100).min(timeline - lo).max(TICK + 1);
            let hi = lo + rng.range(TICK, max_span);
            Stmt::Append {
                name: "ts".into(),
                start: lo,
                end: hi,
                value: Literal::Int(rng.range(1, 9_999)),
            }
        })
        .collect();

    for s in &stmts[..warmup] {
        exec_ok(exec, s);
    }
    time_stmts(exec, "append-correction", backend, &stmts[warmup..])
}

fn wl_at_lookup(cli: &Cli, exec: &mut Executor, backend: &str, rng: &mut Rng) -> Run {
    exec_ok(
        exec,
        &Stmt::CreateDatabase {
            name: "bench".into(),
        },
    );
    exec_ok(
        exec,
        &Stmt::Create {
            name: "ts".into(),
            ty: Type::Int,
        },
    );

    let preload = (cli.ops / 10).max(1_000);
    let timeline = preload as i64 * TICK;
    for i in 0..preload as i64 {
        exec_ok(
            exec,
            &Stmt::Append {
                name: "ts".into(),
                start: i * TICK,
                end: i * TICK + TICK,
                value: Literal::Int(rng.range(1, 9_999)),
            },
        );
    }

    let warmup = cli.warmup.min(cli.ops);
    let total = warmup + cli.ops;
    let stmts: Vec<Stmt> = (0..total)
        .map(|_| Stmt::At {
            name: "ts".into(),
            t: rng.range(0, timeline.max(1)),
        })
        .collect();

    for s in &stmts[..warmup] {
        exec_ok(exec, s);
    }
    time_stmts(exec, "at-lookup", backend, &stmts[warmup..])
}

fn wl_range_scan(cli: &Cli, exec: &mut Executor, backend: &str, rng: &mut Rng) -> Run {
    exec_ok(
        exec,
        &Stmt::CreateDatabase {
            name: "bench".into(),
        },
    );
    exec_ok(
        exec,
        &Stmt::Create {
            name: "ts".into(),
            ty: Type::Int,
        },
    );

    let preload = (cli.ops / 10).max(1_000);
    let timeline = preload as i64 * TICK;
    for i in 0..preload as i64 {
        exec_ok(
            exec,
            &Stmt::Append {
                name: "ts".into(),
                start: i * TICK,
                end: i * TICK + TICK,
                value: Literal::Int(rng.range(1, 9_999)),
            },
        );
    }

    // Each window is ~10% of the timeline.
    let window = (timeline / 10).max(TICK * 2);
    let warmup = cli.warmup.min(cli.ops);
    let total = warmup + cli.ops;
    let stmts: Vec<Stmt> = (0..total)
        .map(|_| {
            let start = rng.range(0, (timeline - window).max(1));
            Stmt::Range {
                name: "ts".into(),
                start,
                end: start + window,
                filter: None,
            }
        })
        .collect();

    for s in &stmts[..warmup] {
        exec_ok(exec, s);
    }
    time_stmts(exec, "range-scan", backend, &stmts[warmup..])
}

fn wl_range_filter(cli: &Cli, exec: &mut Executor, backend: &str, rng: &mut Rng) -> Run {
    exec_ok(
        exec,
        &Stmt::CreateDatabase {
            name: "bench".into(),
        },
    );
    exec_ok(
        exec,
        &Stmt::Create {
            name: "ts".into(),
            ty: Type::Int,
        },
    );

    let preload = (cli.ops / 10).max(1_000);
    let timeline = preload as i64 * TICK;
    for i in 0..preload as i64 {
        exec_ok(
            exec,
            &Stmt::Append {
                name: "ts".into(),
                start: i * TICK,
                end: i * TICK + TICK,
                value: Literal::Int(rng.range(1, 9_999)),
            },
        );
    }

    // WHERE ts > 5000 — ~50% selectivity.
    let filter = Expr::Binary {
        op: BinOp::Gt,
        lhs: Box::new(Expr::Ident("ts".into())),
        rhs: Box::new(Expr::Lit(Literal::Int(5_000))),
    };
    let window = (timeline / 10).max(TICK * 2);
    let warmup = cli.warmup.min(cli.ops);
    let total = warmup + cli.ops;
    let stmts: Vec<Stmt> = (0..total)
        .map(|_| {
            let start = rng.range(0, (timeline - window).max(1));
            Stmt::Range {
                name: "ts".into(),
                start,
                end: start + window,
                filter: Some(filter.clone()),
            }
        })
        .collect();

    for s in &stmts[..warmup] {
        exec_ok(exec, s);
    }
    time_stmts(exec, "range-filter", backend, &stmts[warmup..])
}

fn wl_mixed(cli: &Cli, exec: &mut Executor, backend: &str, rng: &mut Rng) -> Run {
    exec_ok(
        exec,
        &Stmt::CreateDatabase {
            name: "bench".into(),
        },
    );
    exec_ok(
        exec,
        &Stmt::Create {
            name: "ts".into(),
            ty: Type::Int,
        },
    );

    // Pre-seed data so reads don't see a completely empty lens.
    let seed_ticks = ((cli.ops / 10) as i64).max(500);
    let timeline = seed_ticks * TICK;
    for i in 0..seed_ticks {
        exec_ok(
            exec,
            &Stmt::Append {
                name: "ts".into(),
                start: i * TICK,
                end: i * TICK + TICK,
                value: Literal::Int(rng.range(1, 9_999)),
            },
        );
    }

    let warmup = cli.warmup.min(cli.ops);
    let total = warmup + cli.ops;
    let mut write_cursor = seed_ticks;
    let stmts: Vec<Stmt> = (0..total)
        .map(|_| {
            if rng.bool_p(cli.read_pct) {
                Stmt::At {
                    name: "ts".into(),
                    t: rng.range(0, timeline.max(1)),
                }
            } else {
                let s = write_cursor * TICK;
                write_cursor += 1;
                Stmt::Append {
                    name: "ts".into(),
                    start: s,
                    end: s + TICK,
                    value: Literal::Int(rng.range(1, 9_999)),
                }
            }
        })
        .collect();

    for s in &stmts[..warmup] {
        exec_ok(exec, s);
    }
    time_stmts(exec, "mixed", backend, &stmts[warmup..])
}

fn wl_derived(cli: &Cli, exec: &mut Executor, backend: &str, rng: &mut Rng) -> Run {
    exec_ok(
        exec,
        &Stmt::CreateDatabase {
            name: "bench".into(),
        },
    );
    exec_ok(
        exec,
        &Stmt::Create {
            name: "base".into(),
            ty: Type::Float,
        },
    );

    let preload = (cli.ops / 10).max(1_000);
    let timeline = preload as i64 * TICK;
    for i in 0..preload as i64 {
        exec_ok(
            exec,
            &Stmt::Append {
                name: "base".into(),
                start: i * TICK,
                end: i * TICK + TICK,
                value: Literal::Float(rng.range(1, 9_999) as f64 / 100.0),
            },
        );
    }

    // Chain: d0 = base * 2.0, d1 = d0 + 1.0, d2 = d1 * 2.0, ...
    let depth = cli.derive_depth.max(1);
    for i in 0..depth {
        let src = if i == 0 {
            "base".to_string()
        } else {
            format!("d{}", i - 1)
        };
        let dst = format!("d{i}");
        let expr = if i % 2 == 0 {
            Expr::Binary {
                op: BinOp::Mul,
                lhs: Box::new(Expr::Ident(src)),
                rhs: Box::new(Expr::Lit(Literal::Float(2.0))),
            }
        } else {
            Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(Expr::Ident(src)),
                rhs: Box::new(Expr::Lit(Literal::Float(1.0))),
            }
        };
        exec_ok(exec, &Stmt::Derive { name: dst, expr });
    }

    let leaf = format!("d{}", depth - 1);
    let warmup = cli.warmup.min(cli.ops);
    let total = warmup + cli.ops;
    let stmts: Vec<Stmt> = (0..total)
        .map(|_| Stmt::At {
            name: leaf.clone(),
            t: rng.range(0, timeline.max(1)),
        })
        .collect();

    for s in &stmts[..warmup] {
        exec_ok(exec, s);
    }
    time_stmts(exec, "derived", backend, &stmts[warmup..])
}

/// IoT ingestion: every tick writes to all `lenses` sensors.
/// ops = ticks × sensors; throughput = individual sensor-writes/s.
fn wl_iot_ingest(cli: &Cli, exec: &mut Executor, backend: &str, rng: &mut Rng) -> Run {
    exec_ok(
        exec,
        &Stmt::CreateDatabase {
            name: "bench".into(),
        },
    );
    let n = cli.lenses.max(1);
    for i in 0..n {
        exec_ok(
            exec,
            &Stmt::Create {
                name: format!("sensor_{i}"),
                ty: Type::Float,
            },
        );
    }

    let warmup_ticks = cli.warmup.min(cli.ops);
    let total_ticks = warmup_ticks + cli.ops;
    let mut all_stmts: Vec<Stmt> = Vec::with_capacity(total_ticks * n);
    for tick in 0..total_ticks as i64 {
        for sensor in 0..n {
            all_stmts.push(Stmt::Append {
                name: format!("sensor_{sensor}"),
                start: tick * TICK,
                end: tick * TICK + TICK,
                value: Literal::Float(rng.range(0, 10_000) as f64 / 100.0),
            });
        }
    }

    let warmup_ops = warmup_ticks * n;
    for s in &all_stmts[..warmup_ops] {
        exec_ok(exec, s);
    }
    time_stmts(exec, "iot-ingest", backend, &all_stmts[warmup_ops..])
}

fn run_one(cli: &Cli, wl: &Workload, backend: &str, exec: &mut Executor, rng: &mut Rng) -> Run {
    match wl {
        Workload::AppendSeq => wl_append_seq(cli, exec, backend, rng),
        Workload::AppendCorrection => wl_append_correction(cli, exec, backend, rng),
        Workload::AtLookup => wl_at_lookup(cli, exec, backend, rng),
        Workload::RangeScan => wl_range_scan(cli, exec, backend, rng),
        Workload::RangeFilter => wl_range_filter(cli, exec, backend, rng),
        Workload::Mixed => wl_mixed(cli, exec, backend, rng),
        Workload::Derived => wl_derived(cli, exec, backend, rng),
        Workload::IotIngest => wl_iot_ingest(cli, exec, backend, rng),
        Workload::All => unreachable!(),
    }
}

fn fmt_int(n: usize) -> String {
    let s = n.to_string();
    let mut out: Vec<char> = Vec::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out.iter().rev().collect()
}

fn fmt_dur(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1_000.0;
    if ms < 1_000.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{:.2} s", ms / 1_000.0)
    }
}

fn fmt_kb(kb: u64) -> String {
    if kb < 1_024 {
        format!("{kb} KB")
    } else {
        format!("{:.1} MB", kb as f64 / 1_024.0)
    }
}

fn fmt_dkb(dk: i64) -> String {
    let sign = if dk >= 0 { "+" } else { "" };
    if dk.unsigned_abs() < 1_024 {
        format!("{sign}{dk} KB")
    } else {
        format!("{sign}{:.1} MB", dk as f64 / 1_024.0)
    }
}

fn print_text(run: &Run) {
    const W: usize = 62;
    println!("\n{}", "─".repeat(W));
    println!(
        "  {:<26} [{:<10}]  {:>10} ops",
        run.label,
        run.backend,
        fmt_int(run.ops)
    );
    println!("{}", "─".repeat(W));
    println!(
        "  {:<22} {:>16}",
        "Throughput",
        format!("{} ops/s", fmt_int(run.throughput() as usize))
    );
    println!("  {:<22} {:>16}", "Duration", fmt_dur(run.elapsed));
    println!("  Latency (µs)");
    for (name, p) in [
        ("p50", 50.0),
        ("p95", 95.0),
        ("p99", 99.0),
        ("p99.9", 99.9),
        ("max", 100.0),
    ] {
        println!("    {:<20} {:>10.2}", name, run.pct_us(p));
    }
    if run.rss_before_kb.is_some() || run.rss_after_kb.is_some() {
        println!("  Memory (RSS)");
        if let Some(kb) = run.rss_before_kb {
            println!("    {:<20} {:>10}", "before", fmt_kb(kb));
        }
        if let Some(kb) = run.rss_after_kb {
            println!("    {:<20} {:>10}", "after", fmt_kb(kb));
        }
        if let Some(dk) = run.rss_delta_kb() {
            println!("    {:<20} {:>10}", "delta", fmt_dkb(dk));
        }
    }
}

fn print_json(runs: &[Run]) {
    println!("[");
    for (i, r) in runs.iter().enumerate() {
        let comma = if i + 1 < runs.len() { "," } else { "" };
        println!("  {{");
        println!("    \"label\": \"{}\",", r.label);
        println!("    \"backend\": \"{}\",", r.backend);
        println!("    \"ops\": {},", r.ops);
        println!("    \"throughput_ops_per_sec\": {:.1},", r.throughput());
        println!(
            "    \"duration_ms\": {:.3},",
            r.elapsed.as_secs_f64() * 1_000.0
        );
        println!("    \"latency_us\": {{");
        println!("      \"p50\": {:.3},", r.pct_us(50.0));
        println!("      \"p95\": {:.3},", r.pct_us(95.0));
        println!("      \"p99\": {:.3},", r.pct_us(99.0));
        println!("      \"p999\": {:.3},", r.pct_us(99.9));
        println!("      \"max\": {:.3}", r.pct_us(100.0));
        print!("    }}");
        if r.rss_before_kb.is_some() || r.rss_after_kb.is_some() {
            println!(",");
            println!("    \"memory_kb\": {{");
            println!("      \"before\": {},", r.rss_before_kb.unwrap_or(0));
            println!("      \"after\": {},", r.rss_after_kb.unwrap_or(0));
            println!("      \"delta\": {}", r.rss_delta_kb().unwrap_or(0));
            print!("    }}");
        }
        println!("\n  }}{comma}");
    }
    println!("]");
}

fn print_csv(runs: &[Run]) {
    println!(
        "label,backend,ops,throughput_ops_per_sec,duration_ms,\
         p50_us,p95_us,p99_us,p999_us,max_us,\
         rss_before_kb,rss_after_kb,rss_delta_kb"
    );
    for r in runs {
        println!(
            "{},{},{},{:.1},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{},{},{}",
            r.label,
            r.backend,
            r.ops,
            r.throughput(),
            r.elapsed.as_secs_f64() * 1_000.0,
            r.pct_us(50.0),
            r.pct_us(95.0),
            r.pct_us(99.0),
            r.pct_us(99.9),
            r.pct_us(100.0),
            r.rss_before_kb.unwrap_or(0),
            r.rss_after_kb.unwrap_or(0),
            r.rss_delta_kb().unwrap_or(0),
        );
    }
}

const ALL: &[Workload] = &[
    Workload::AppendSeq,
    Workload::AppendCorrection,
    Workload::AtLookup,
    Workload::RangeScan,
    Workload::RangeFilter,
    Workload::Mixed,
    Workload::Derived,
    Workload::IotIngest,
];

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let cli = Cli::parse();

    let suite: Vec<&Workload> = if cli.workload == Workload::All {
        ALL.iter().collect()
    } else {
        vec![&cli.workload]
    };

    let backends: Vec<(&str, bool)> = {
        let mut b = vec![("InMemory", false)];
        if cli.with_wal {
            b.push(("WAL", true));
        }
        b
    };

    let mut runs: Vec<Run> = Vec::new();

    for wl in &suite {
        for &(backend_name, use_wal) in &backends {
            // WAL workloads are slow — warn upfront.
            if use_wal
                && matches!(
                    *wl,
                    Workload::AppendSeq | Workload::AppendCorrection | Workload::IotIngest
                )
            {
                warn!(
                    workload = %wl.to_possible_value().unwrap().get_name(),
                    backend = backend_name,
                    "many fsyncs ahead — consider --ops 10000 for WAL runs"
                );
            }
            info!(
                workload = %wl.to_possible_value().unwrap().get_name(),
                backend = backend_name,
                ops = %fmt_int(cli.ops),
                "running workload"
            );

            let (mut exec, _guard) = make_executor(use_wal);
            let mut rng = Rng::seed(cli.seed);
            let run = run_one(&cli, wl, backend_name, &mut exec, &mut rng);
            runs.push(run);
        }
    }

    match cli.fmt {
        Fmt::Text => {
            for run in &runs {
                print_text(run);
            }
            println!();
        }
        Fmt::Json => print_json(&runs),
        Fmt::Csv => print_csv(&runs),
    }
}
