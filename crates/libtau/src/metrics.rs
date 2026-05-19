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

use rustc_hash::FxHashMap as HashMap;
use std::fmt::Write as FmtWrite;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

const LATENCY_BUCKETS_US: &[u64] = &[
    1, 5, 10, 25, 50, 100, 250, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000,
];

#[derive(Debug)]
pub struct Histogram {
    counts: Vec<AtomicU64>,
    sum_us: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            counts: LATENCY_BUCKETS_US
                .iter()
                .map(|_| AtomicU64::new(0))
                .collect(),
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn observe_ns(&self, ns: u64) {
        let us = ns / 1_000;
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        for (i, bound) in LATENCY_BUCKETS_US.iter().enumerate() {
            if us <= *bound {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Debug)]
pub struct OpStats {
    pub count: AtomicU64,
    pub ns: AtomicU64,
    pub hist: Histogram,
}

impl OpStats {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            hist: Histogram::new(),
        }
    }

    #[inline]
    pub fn record(&self, elapsed_ns: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        self.hist.observe_ns(elapsed_ns);
    }
}

/// Per-database lightweight counters (no histograms — cardinality concern).
#[derive(Debug, Default)]
pub struct PerDbCounts {
    pub counts: [AtomicU64; OP_COUNT],
}

impl PerDbCounts {
    fn new() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

pub const OP_COUNT: usize = 13;

pub const OP_LABELS: [&str; OP_COUNT] = [
    "append",      // Append, BatchAppend
    "copy",        // Copy (server-side CSV ingest)
    "at",          // At, AtAsOf, AtLayer
    "range",       // Range
    "reduce",      // Reduce
    "history",     // HistoryLens
    "create_lens", // Create (base lens), Derive
    "drop_lens",   // Drop
    "show",        // ShowDatabases, ShowLenses, ShowUsers, ShowGrants
    "database",    // CreateDatabase, DropDatabase, UseDatabase
    "user",        // CreateUser, DropUser, Grant, Revoke
    "backup",      // BackupDatabase, RestoreDatabase
    "transaction", // StartTransaction, Commit, Rollback
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

#[derive(Debug)]
pub struct Metrics {
    pub ops: [OpStats; OP_COUNT],

    /// Each compaction event (sweep-line merge of one lens's layer stack).
    pub compactions: AtomicU64,

    /// Latency of a single WAL append (bytes serialised + optional fsync).
    pub wal_write_latency: Histogram,

    /// TCP connections accepted since startup.
    pub connections: AtomicU64,
    /// Currently open TCP connections (gauge).
    pub connections_active: AtomicU64,
    /// Connections rejected at the accept boundary (at-capacity).
    pub rejected_connections: AtomicU64,

    pub auth_attempts: AtomicU64,
    pub auth_failures: AtomicU64,
    pub errors: AtomicU64,

    /// Per-database op counts. Keyed by database name; entries added lazily.
    per_db: RwLock<HashMap<String, Arc<PerDbCounts>>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            ops: std::array::from_fn(|_| OpStats::new()),
            compactions: AtomicU64::new(0),
            wal_write_latency: Histogram::new(),
            connections: AtomicU64::new(0),
            connections_active: AtomicU64::new(0),
            rejected_connections: AtomicU64::new(0),
            auth_attempts: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            per_db: RwLock::new(HashMap::default()),
        }
    }

    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }

    #[inline]
    pub fn record_op(&self, op: Op, ns: u64) {
        self.ops[op as usize].record(ns);
    }

    // Keep old call sites compiling without churn.
    #[inline]
    pub fn record_append(&self, ns: u64) {
        self.record_op(Op::Append, ns);
    }
    #[inline]
    pub fn record_at(&self, ns: u64) {
        self.record_op(Op::At, ns);
    }
    #[inline]
    pub fn record_range(&self, ns: u64) {
        self.record_op(Op::Range, ns);
    }
    #[inline]
    pub fn record_reduce(&self, ns: u64) {
        self.record_op(Op::Reduce, ns);
    }
    #[inline]
    pub fn record_history(&self, ns: u64) {
        self.record_op(Op::History, ns);
    }
    #[inline]
    pub fn record_ddl(&self, ns: u64) {
        self.record_op(Op::Database, ns);
    }

    #[inline]
    pub fn record_compaction(&self) {
        self.compactions.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_wal_write(&self, ns: u64) {
        self.wal_write_latency.observe_ns(ns);
    }

    #[inline]
    pub fn record_auth_attempt(&self) {
        self.auth_attempts.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn record_auth_failure(&self) {
        self.auth_failures.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn record_rejected_connection(&self) {
        self.rejected_connections.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn set_active_connections(&self, n: u64) {
        self.connections_active.store(n, Ordering::Relaxed);
    }

    /// Record one op against a named database.  The entry is created lazily.
    pub fn record_db_op(&self, db: &str, op: Op) {
        // Fast path: database already registered.
        if let Ok(map) = self.per_db.read()
            && let Some(entry) = map.get(db)
        {
            entry.counts[op as usize].fetch_add(1, Ordering::Relaxed);
            return;
        }
        // Slow path: first time we've seen this database.
        let mut map = self.per_db.write().expect("per_db lock poisoned");
        let entry = map
            .entry(db.to_string())
            .or_insert_with(|| Arc::new(PerDbCounts::new()));
        entry.counts[op as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn prometheus_text(&self) -> String {
        let r = Ordering::Relaxed;
        let mut s = String::with_capacity(8192);

        emit_help(
            &mut s,
            "tau_statements_total",
            "counter",
            "Total statements processed, by type",
        );
        for (label, stats) in OP_LABELS.iter().zip(self.ops.iter()) {
            let _ = writeln!(
                s,
                "tau_statements_total{{type=\"{label}\"}} {}",
                stats.count.load(r)
            );
        }

        emit_help(
            &mut s,
            "tau_statement_nanoseconds_total",
            "counter",
            "Cumulative executor time per statement type, in nanoseconds",
        );
        for (label, stats) in OP_LABELS.iter().zip(self.ops.iter()) {
            let _ = writeln!(
                s,
                "tau_statement_nanoseconds_total{{type=\"{label}\"}} {}",
                stats.ns.load(r)
            );
        }

        emit_help(
            &mut s,
            "tau_statement_duration_microseconds",
            "histogram",
            "Executor latency per statement type, in microseconds",
        );
        for (label, stats) in OP_LABELS.iter().zip(self.ops.iter()) {
            emit_histogram_lines(
                &mut s,
                "tau_statement_duration_microseconds",
                &[("type", label)],
                &stats.hist,
                r,
            );
        }

        emit_help(
            &mut s,
            "tau_compactions_total",
            "counter",
            "Times the sweep-line compactor merged a lens layer stack",
        );
        let _ = writeln!(s, "tau_compactions_total {}", self.compactions.load(r));

        emit_help(
            &mut s,
            "tau_wal_write_duration_microseconds",
            "histogram",
            "Time spent writing one entry to the WAL file (includes fsync when enabled)",
        );
        emit_histogram_lines(
            &mut s,
            "tau_wal_write_duration_microseconds",
            &[],
            &self.wal_write_latency,
            r,
        );

        emit_help(
            &mut s,
            "tau_connections_total",
            "counter",
            "Total TCP connections accepted by the server",
        );
        let _ = writeln!(s, "tau_connections_total {}", self.connections.load(r));

        emit_help(
            &mut s,
            "tau_connections_active",
            "gauge",
            "Currently open TCP connections",
        );
        let _ = writeln!(
            s,
            "tau_connections_active {}",
            self.connections_active.load(r)
        );

        emit_help(
            &mut s,
            "tau_rejected_connections_total",
            "counter",
            "Connections refused at the accept boundary (server at capacity)",
        );
        let _ = writeln!(
            s,
            "tau_rejected_connections_total {}",
            self.rejected_connections.load(r)
        );

        emit_help(
            &mut s,
            "tau_auth_attempts_total",
            "counter",
            "Total AUTH messages received",
        );
        let _ = writeln!(s, "tau_auth_attempts_total {}", self.auth_attempts.load(r));

        emit_help(
            &mut s,
            "tau_auth_failures_total",
            "counter",
            "AUTH attempts rejected (wrong credentials or missing AUTH)",
        );
        let _ = writeln!(s, "tau_auth_failures_total {}", self.auth_failures.load(r));

        emit_help(
            &mut s,
            "tau_errors_total",
            "counter",
            "Total ERR responses sent to clients",
        );
        let _ = writeln!(s, "tau_errors_total {}", self.errors.load(r));

        #[allow(clippy::collapsible_if)]
        if let Ok(map) = self.per_db.read() {
            if !map.is_empty() {
                emit_help(
                    &mut s,
                    "tau_db_statements_total",
                    "counter",
                    "Statements processed per database and type",
                );
                for (db, counts) in map.iter() {
                    for (i, label) in OP_LABELS.iter().enumerate() {
                        let v = counts.counts[i].load(r);
                        if v > 0 {
                            let _ = writeln!(
                                s,
                                "tau_db_statements_total{{db=\"{db}\",type=\"{label}\"}} {v}"
                            );
                        }
                    }
                }
            }
        }

        let usage = ProcessUsage::sample();
        emit_help(
            &mut s,
            "tau_process_resident_bytes",
            "gauge",
            "Resident set size of the tau process in bytes (0 if unsupported)",
        );
        let _ = writeln!(s, "tau_process_resident_bytes {}", usage.rss_bytes);

        emit_help(
            &mut s,
            "tau_process_virtual_bytes",
            "gauge",
            "Virtual memory size of the tau process in bytes (0 if unsupported)",
        );
        let _ = writeln!(s, "tau_process_virtual_bytes {}", usage.vsz_bytes);

        emit_help(
            &mut s,
            "tau_process_open_fds",
            "gauge",
            "Open file descriptors held by the tau process (0 if unsupported)",
        );
        let _ = writeln!(s, "tau_process_open_fds {}", usage.open_fds);

        emit_help(
            &mut s,
            "tau_process_threads",
            "gauge",
            "OS threads in the tau process (0 if unsupported)",
        );
        let _ = writeln!(s, "tau_process_threads {}", usage.threads);

        emit_help(
            &mut s,
            "tau_process_uptime_seconds",
            "gauge",
            "Wall-clock seconds since the metrics module was initialised",
        );
        let _ = writeln!(s, "tau_process_uptime_seconds {}", uptime_seconds());

        let _ = writeln!(s, "# EOF");
        s
    }
}

fn emit_help(s: &mut String, name: &str, typ: &str, help: &str) {
    let _ = writeln!(s, "# HELP {name} {help}");
    let _ = writeln!(s, "# TYPE {name} {typ}");
}

fn emit_histogram_lines(
    s: &mut String,
    name: &str,
    labels: &[(&str, &str)],
    hist: &Histogram,
    r: Ordering,
) {
    let label_str: String = if labels.is_empty() {
        String::new()
    } else {
        let inner: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        format!("{{{}}}", inner.join(","))
    };
    for (i, bound) in LATENCY_BUCKETS_US.iter().enumerate() {
        let _ = writeln!(
            s,
            "{name}_bucket{{{inner}le=\"{bound}\"}} {}",
            hist.counts[i].load(r),
            inner = if label_str.is_empty() {
                String::new()
            } else {
                format!("{},", &label_str[1..label_str.len() - 1])
            }
        );
    }
    let _ = writeln!(
        s,
        "{name}_bucket{{{inner}le=\"+Inf\"}} {}",
        hist.count.load(r),
        inner = if label_str.is_empty() {
            String::new()
        } else {
            format!("{},", &label_str[1..label_str.len() - 1])
        }
    );
    let _ = writeln!(s, "{name}_sum{label_str} {}", hist.sum_us.load(r));
    let _ = writeln!(s, "{name}_count{label_str} {}", hist.count.load(r));
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
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let mut usage = ProcessUsage::default();
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            usage.rss_bytes = parse_kib(v).unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("VmSize:") {
            usage.vsz_bytes = parse_kib(v).unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("Threads:") {
            usage.threads = v.trim().parse().unwrap_or(0);
        }
    }
    usage.open_fds = fs::read_dir("/proc/self/fd")
        .ok()
        .map(|d| d.flatten().count() as u64)
        .unwrap_or(0);
    Some(usage)
}

#[cfg(target_os = "linux")]
fn parse_kib(line: &str) -> Option<u64> {
    let kib: u64 = line.split_whitespace().next()?.parse().ok()?;
    Some(kib.saturating_mul(1024))
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
        let r = Ordering::Relaxed;
        for op in &m.ops {
            assert_eq!(op.count.load(r), 0);
        }
        assert_eq!(m.compactions.load(r), 0);
        assert_eq!(m.connections_active.load(r), 0);
    }

    #[hegel::test]
    fn record_op_increments_count_and_ns(tc: TestCase) {
        let samples = tc
            .draw(gs::vecs(gs::integers::<u64>().min_value(0).max_value(10_000_000)).max_size(16));
        let m = Metrics::new();
        for &ns in &samples {
            m.record_op(Op::Append, ns);
        }
        let r = Ordering::Relaxed;
        let append = &m.ops[Op::Append as usize];
        assert_eq!(append.count.load(r), samples.len() as u64);
        assert_eq!(append.ns.load(r), samples.iter().sum::<u64>());
    }

    #[hegel::test]
    fn histogram_observations_fall_in_the_right_bucket(tc: TestCase) {
        let h = Histogram::new();
        let ns = tc.draw(gs::integers::<u64>().min_value(0).max_value(2_000_000_000));
        h.observe_ns(ns);
        let us = ns / 1_000;
        let r = Ordering::Relaxed;
        for (i, &bound) in LATENCY_BUCKETS_US.iter().enumerate() {
            let expected = if us <= bound { 1 } else { 0 };
            assert_eq!(h.counts[i].load(r), expected, "bucket le={bound}");
        }
        assert_eq!(h.count.load(r), 1);
        assert_eq!(h.sum_us.load(r), us);
    }

    #[hegel::test]
    fn histogram_buckets_are_monotone_non_decreasing(tc: TestCase) {
        let h = Histogram::new();
        let samples =
            tc.draw(gs::vecs(gs::integers::<u64>().min_value(0).max_value(1_000_000)).max_size(64));
        for s in samples {
            h.observe_ns(s);
        }
        let r = Ordering::Relaxed;
        let counts: Vec<u64> = h.counts.iter().map(|c| c.load(r)).collect();
        for w in counts.windows(2) {
            assert!(w[0] <= w[1], "bucket counts must be cumulative");
        }
    }

    #[test]
    fn record_compaction_increments() {
        let m = Metrics::new();
        m.record_compaction();
        m.record_compaction();
        assert_eq!(m.compactions.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn wal_write_latency_records() {
        let m = Metrics::new();
        m.record_wal_write(5_000);
        assert_eq!(m.wal_write_latency.count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn per_db_counts_accumulate() {
        let m = Metrics::new();
        m.record_db_op("main", Op::Append);
        m.record_db_op("main", Op::Append);
        m.record_db_op("other", Op::At);
        let map = m.per_db.read().unwrap();
        assert_eq!(
            map["main"].counts[Op::Append as usize].load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            map["other"].counts[Op::At as usize].load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn set_active_connections_stores_value() {
        let m = Metrics::new();
        m.set_active_connections(42);
        assert_eq!(m.connections_active.load(Ordering::Relaxed), 42);
    }

    #[hegel::test]
    fn prometheus_text_exposes_every_documented_family(tc: TestCase) {
        let _ = tc;
        let m = Metrics::new();
        m.record_op(Op::Append, 100);
        m.record_compaction();
        m.record_wal_write(1000);
        m.record_db_op("main", Op::At);
        let text = m.prometheus_text();
        for line in [
            "# TYPE tau_statements_total counter",
            "# TYPE tau_statement_duration_microseconds histogram",
            "# TYPE tau_compactions_total counter",
            "# TYPE tau_wal_write_duration_microseconds histogram",
            "# TYPE tau_connections_active gauge",
            "# TYPE tau_db_statements_total counter",
            "# TYPE tau_process_resident_bytes gauge",
            "# EOF",
        ] {
            assert!(text.contains(line), "missing: {line}");
        }
    }

    #[hegel::test]
    fn prometheus_text_count_matches_counters(tc: TestCase) {
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
