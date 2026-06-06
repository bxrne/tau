//! Execution metrics collected by the [`crate::Executor`].
//!
//! All counters and histograms use lock-free atomics and are safe for
//! concurrent access from many connection threads.
//!
//! ## Metric families
//!
//! - **Per-op counters + histograms** (13 statement categories): total count,
//!   total nanoseconds, latency histogram (1 µs … 500 ms).
//! - **Compaction events**: counter incremented each time the store sweep-line
//!   compactor merges a lens's layer stack.
//! - **WAL write latency**: histogram of time spent writing a single entry to
//!   the WAL file (including optional fsync).
//! - **Connection gauges/counters**: total accepted, current active, rejected.
//! - **Auth counters**: attempts, failures.
//! - **Error counter**: total ERR responses.
//! - **Per-database op counts**: lightweight per-name counters (no histograms)
//!   for the same 13 op categories.
//! - **Process gauges**: RSS, virtual size, open FDs, threads, uptime.
//!   Read from `/proc/self/…` on Linux; zero on other platforms.

use std::sync::Arc;

use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

const LATENCY_BUCKETS_US: &[f64] = &[
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 5_000.0, 10_000.0, 50_000.0,
    100_000.0, 500_000.0,
];

pub const OP_COUNT: usize = 13;

pub const OP_LABELS: [&str; OP_COUNT] = [
    "append",
    "copy",
    "at",
    "range",
    "reduce",
    "history",
    "create_lens",
    "drop_lens",
    "show",
    "database",
    "user",
    "backup",
    "transaction",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Append = 0,
    Copy = 1,
    At = 2,
    Range = 3,
    Reduce = 4,
    History = 5,
    CreateLens = 6,
    DropLens = 7,
    Show = 8,
    Database = 9,
    User = 10,
    Backup = 11,
    Transaction = 12,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OpLabel {
    #[allow(dead_code)]
    r#type: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct DbOpLabel {
    db: String,
    r#type: String,
}

fn buckets() -> impl Iterator<Item = f64> {
    LATENCY_BUCKETS_US.iter().copied()
}

/// Per-op recording handles (cloned from the families registered in the registry).
pub struct OpStats {
    pub count: Counter<u64>,
    pub ns: Counter<u64>,
    pub hist: Histogram,
}

impl OpStats {
    #[inline]
    pub fn record(&self, elapsed_ns: u64) {
        self.count.inc();
        self.ns.inc_by(elapsed_ns);
        self.hist.observe(elapsed_ns as f64 / 1_000.0);
    }
}

pub struct Metrics {
    registry: Registry,

    pub ops: [OpStats; OP_COUNT],

    pub compactions: Counter<u64>,
    pub wal_write_latency: Histogram,

    pub connections: Counter<u64>,
    pub connections_active: Gauge,
    pub rejected_connections: Counter<u64>,

    pub auth_attempts: Counter<u64>,
    pub auth_failures: Counter<u64>,
    pub errors: Counter<u64>,

    per_db: Family<DbOpLabel, Counter<u64>>,

    // Process gauges — updated in prometheus_text() before encoding.
    proc_rss: Gauge,
    proc_vsz: Gauge,
    proc_fds: Gauge,
    proc_threads: Gauge,
    proc_uptime: Gauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let stmt_count: Family<OpLabel, Counter<u64>> = Family::default();
        registry.register(
            "tau_statements",
            "Total statements processed, by type",
            stmt_count.clone(),
        );

        let stmt_ns: Family<OpLabel, Counter<u64>> = Family::default();
        registry.register(
            "tau_statement_nanoseconds",
            "Cumulative executor time per statement type, in nanoseconds",
            stmt_ns.clone(),
        );

        let stmt_hist: Family<OpLabel, Histogram> =
            Family::new_with_constructor(|| Histogram::new(buckets()));
        registry.register(
            "tau_statement_duration_microseconds",
            "Executor latency per statement type, in microseconds",
            stmt_hist.clone(),
        );

        let ops = std::array::from_fn(|i| {
            let label = OpLabel {
                r#type: OP_LABELS[i].to_string(),
            };
            OpStats {
                count: stmt_count.get_or_create(&label).clone(),
                ns: stmt_ns.get_or_create(&label).clone(),
                hist: stmt_hist.get_or_create(&label).clone(),
            }
        });

        let compactions: Counter<u64> = Counter::default();
        registry.register(
            "tau_compactions",
            "Times the sweep-line compactor merged a lens layer stack",
            compactions.clone(),
        );

        let wal_write_latency = Histogram::new(buckets());
        registry.register(
            "tau_wal_write_duration_microseconds",
            "Time spent writing one entry to the WAL file (includes fsync when enabled)",
            wal_write_latency.clone(),
        );

        let connections: Counter<u64> = Counter::default();
        registry.register(
            "tau_connections",
            "Total TCP connections accepted by the server",
            connections.clone(),
        );

        let connections_active: Gauge = Gauge::default();
        registry.register(
            "tau_connections_active",
            "Currently open TCP connections",
            connections_active.clone(),
        );

        let rejected_connections: Counter<u64> = Counter::default();
        registry.register(
            "tau_rejected_connections",
            "Connections refused at the accept boundary (server at capacity)",
            rejected_connections.clone(),
        );

        let auth_attempts: Counter<u64> = Counter::default();
        registry.register(
            "tau_auth_attempts",
            "Total AUTH messages received",
            auth_attempts.clone(),
        );

        let auth_failures: Counter<u64> = Counter::default();
        registry.register(
            "tau_auth_failures",
            "AUTH attempts rejected (wrong credentials or missing AUTH)",
            auth_failures.clone(),
        );

        let errors: Counter<u64> = Counter::default();
        registry.register(
            "tau_errors",
            "Total ERR responses sent to clients",
            errors.clone(),
        );

        let per_db: Family<DbOpLabel, Counter<u64>> = Family::default();
        registry.register(
            "tau_db_statements",
            "Statements processed per database and type",
            per_db.clone(),
        );

        let process_rss: Gauge = Gauge::default();
        registry.register(
            "tau_process_resident_bytes",
            "Resident set size of the tau process in bytes (0 if unsupported)",
            process_rss.clone(),
        );
        let process_vsz: Gauge = Gauge::default();
        registry.register(
            "tau_process_virtual_bytes",
            "Virtual memory size of the tau process in bytes (0 if unsupported)",
            process_vsz.clone(),
        );
        let process_fds: Gauge = Gauge::default();
        registry.register(
            "tau_process_open_fds",
            "Open file descriptors held by the tau process (0 if unsupported)",
            process_fds.clone(),
        );
        let process_threads: Gauge = Gauge::default();
        registry.register(
            "tau_process_threads",
            "OS threads in the tau process (0 if unsupported)",
            process_threads.clone(),
        );
        let process_uptime: Gauge = Gauge::default();
        registry.register(
            "tau_process_uptime_seconds",
            "Wall-clock seconds since the metrics module was initialised",
            process_uptime.clone(),
        );

        Self {
            registry,
            ops,
            compactions,
            wal_write_latency,
            connections,
            connections_active,
            rejected_connections,
            auth_attempts,
            auth_failures,
            errors,
            per_db,
            proc_rss: process_rss,
            proc_vsz: process_vsz,
            proc_fds: process_fds,
            proc_threads: process_threads,
            proc_uptime: process_uptime,
        }
    }

    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }

    #[inline]
    pub fn record_op(&self, op: Op, ns: u64) {
        self.ops[op as usize].record(ns);
    }

    #[inline]
    pub fn record_compaction(&self) {
        self.compactions.inc();
    }

    #[inline]
    pub fn record_wal_write(&self, ns: u64) {
        self.wal_write_latency.observe(ns as f64 / 1_000.0);
    }

    #[inline]
    pub fn record_auth_attempt(&self) {
        self.auth_attempts.inc();
    }
    #[inline]
    pub fn record_auth_failure(&self) {
        self.auth_failures.inc();
    }
    #[inline]
    pub fn record_error(&self) {
        self.errors.inc();
    }
    #[inline]
    pub fn record_rejected_connection(&self) {
        self.rejected_connections.inc();
    }

    #[inline]
    pub fn set_active_connections(&self, n: u64) {
        self.connections_active.set(n as i64);
    }

    pub fn record_db_op(&self, db: &str, op: Op) {
        self.per_db
            .get_or_create(&DbOpLabel {
                db: db.to_string(),
                r#type: OP_LABELS[op as usize].to_string(),
            })
            .inc();
    }

    pub fn prometheus_text(&self) -> String {
        let usage = ProcessUsage::sample();
        self.proc_rss.set(usage.rss_bytes as i64);
        self.proc_vsz.set(usage.vsz_bytes as i64);
        self.proc_fds.set(usage.open_fds as i64);
        self.proc_threads.set(usage.threads as i64);
        self.proc_uptime.set(uptime_seconds() as i64);

        let mut out = String::with_capacity(8192);
        encode(&mut out, &self.registry).expect("metrics encoding cannot fail");
        out
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessUsage {
    pub rss_bytes: u64,
    pub vsz_bytes: u64,
    pub open_fds: u64,
    pub threads: u64,
}

impl ProcessUsage {
    pub fn sample() -> Self {
        #[cfg(target_os = "linux")]
        {
            sample_linux().unwrap_or_default()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::default()
        }
    }
}

#[cfg(target_os = "linux")]
fn sample_linux() -> Option<ProcessUsage> {
    use std::fs;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    let pid = Pid::from_u32(std::process::id());
    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new().with_memory()),
    );
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::new().with_memory(),
    );
    let proc = sys.process(pid)?;

    let threads = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Threads:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0);
    let open_fds = fs::read_dir("/proc/self/fd")
        .ok()
        .map(|d| d.flatten().count() as u64)
        .unwrap_or(0);

    Some(ProcessUsage {
        rss_bytes: proc.memory(),
        vsz_bytes: proc.virtual_memory(),
        threads,
        open_fds,
    })
}

fn uptime_seconds() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static STARTED: OnceLock<Instant> = OnceLock::new();
    STARTED.get_or_init(Instant::now).elapsed().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use pretty_assertions::assert_eq;

    #[test]
    fn counters_start_at_zero() {
        let m = Metrics::new();
        for op in &m.ops {
            assert_eq!(op.count.get(), 0);
        }
        assert_eq!(m.compactions.get(), 0);
        assert_eq!(m.connections_active.get(), 0);
    }

    #[hegel::test]
    fn pbt_record_op_increments_count_and_ns(tc: TestCase) {
        let samples = tc
            .draw(gs::vecs(gs::integers::<u64>().min_value(0).max_value(10_000_000)).max_size(16));
        let m = Metrics::new();
        for &ns in &samples {
            m.record_op(Op::Append, ns);
        }
        let append = &m.ops[Op::Append as usize];
        assert_eq!(append.count.get(), samples.len() as u64);
        assert_eq!(append.ns.get(), samples.iter().sum::<u64>());
    }

    #[test]
    fn record_compaction_increments() {
        let m = Metrics::new();
        m.record_compaction();
        m.record_compaction();
        assert_eq!(m.compactions.get(), 2);
    }

    #[test]
    fn wal_write_latency_records() {
        let m = Metrics::new();
        m.record_wal_write(5_000);
        let text = m.prometheus_text();
        assert!(text.contains("tau_wal_write_duration_microseconds"));
    }

    #[test]
    fn per_db_counts_accumulate() {
        let m = Metrics::new();
        m.record_db_op("main", Op::Append);
        m.record_db_op("main", Op::Append);
        m.record_db_op("other", Op::At);
        let main_label = DbOpLabel {
            db: "main".into(),
            r#type: "append".into(),
        };
        let other_label = DbOpLabel {
            db: "other".into(),
            r#type: "at".into(),
        };
        assert_eq!(m.per_db.get_or_create(&main_label).get(), 2);
        assert_eq!(m.per_db.get_or_create(&other_label).get(), 1);
    }

    #[test]
    fn set_active_connections_stores_value() {
        let m = Metrics::new();
        m.set_active_connections(42);
        assert_eq!(m.connections_active.get(), 42);
    }

    #[hegel::test]
    fn pbt_prometheus_text_exposes_every_documented_family(tc: TestCase) {
        let _ = tc;
        let m = Metrics::new();
        m.record_op(Op::Append, 100);
        m.record_compaction();
        m.record_wal_write(1000);
        m.record_db_op("main", Op::At);
        let text = m.prometheus_text();
        for family in [
            "tau_statements",
            "tau_statement_duration_microseconds",
            "tau_compactions",
            "tau_wal_write_duration_microseconds",
            "tau_connections_active",
            "tau_db_statements",
            "tau_process_resident_bytes",
        ] {
            assert!(text.contains(family), "missing: {family}");
        }
    }

    #[hegel::test]
    fn pbt_prometheus_text_count_matches_counters(tc: TestCase) {
        let n = tc.draw(gs::integers::<u64>().min_value(0).max_value(20));
        let m = Metrics::new();
        for _ in 0..n {
            m.record_op(Op::Append, 100);
        }
        let text = m.prometheus_text();
        assert!(text.contains(&format!("tau_statements_total{{type=\"append\"}} {n}")));
    }

    #[test]
    fn all_13_op_labels_appear_in_output() {
        let m = Metrics::new();
        let text = m.prometheus_text();
        for label in OP_LABELS {
            assert!(
                text.contains(&format!("type=\"{label}\"")),
                "missing op: {label}"
            );
        }
    }
}
