//! Execution metrics collected by the [`crate::Executor`].
//!
//! All counters and histograms are lock-free and safe for concurrent access.
//! `prometheus_text()` renders the snapshot in the Prometheus exposition
//! format so the server can serve it over HTTP.
//!
//! ## Metric families
//!
//! - **Counters** (per statement type): total statements, total nanoseconds.
//! - **Histograms**: latency in microseconds, per statement type, with the
//!   default OpenMetrics-style bucket layout `1, 5, 10, 25, 50, 100, 250,
//!   500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000, +Inf`.
//! - **Process gauges**: resident set size (RSS), virtual size (VMS), open
//!   file descriptors, thread count.  Read from `/proc/self/...` on Linux;
//!   on other platforms the gauges report `0`.
//! - **Server counters**: total / rejected connections, AUTH attempts /
//!   failures, total error responses.

use std::fmt::Write as FmtWrite;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Histogram bucket upper bounds in microseconds.
const LATENCY_BUCKETS_US: &[u64] = &[
    1, 5, 10, 25, 50, 100, 250, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000,
];

/// A simple cumulative histogram backed by atomic counters.
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

/// Per-operation stats: request count, total nanoseconds, and latency histogram.
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
    fn record(&self, elapsed_ns: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        self.hist.observe_ns(elapsed_ns);
    }
}

const OP_COUNT: usize = 6;
const OP_LABELS: [&str; OP_COUNT] = ["append", "at", "range", "reduce", "history", "ddl"];

/// Index into [`Metrics::ops`].
#[derive(Clone, Copy)]
pub enum Op {
    Append = 0,
    At = 1,
    Range = 2,
    Reduce = 3,
    History = 4,
    Ddl = 5,
}

/// Per-operation execution counters and histograms for an [`crate::Executor`].
#[derive(Debug)]
pub struct Metrics {
    pub ops: [OpStats; OP_COUNT],
    pub connections: AtomicU64,
    pub rejected_connections: AtomicU64,
    pub auth_attempts: AtomicU64,
    pub auth_failures: AtomicU64,
    pub errors: AtomicU64,
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
            connections: AtomicU64::new(0),
            rejected_connections: AtomicU64::new(0),
            auth_attempts: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record_append(&self, ns: u64) {
        self.ops[Op::Append as usize].record(ns);
    }
    #[inline]
    pub fn record_at(&self, ns: u64) {
        self.ops[Op::At as usize].record(ns);
    }
    #[inline]
    pub fn record_range(&self, ns: u64) {
        self.ops[Op::Range as usize].record(ns);
    }
    #[inline]
    pub fn record_reduce(&self, ns: u64) {
        self.ops[Op::Reduce as usize].record(ns);
    }
    #[inline]
    pub fn record_history(&self, ns: u64) {
        self.ops[Op::History as usize].record(ns);
    }
    #[inline]
    pub fn record_ddl(&self, ns: u64) {
        self.ops[Op::Ddl as usize].record(ns);
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

    /// Render all counters, histograms, and the current process resource
    /// snapshot in the Prometheus text exposition format.
    pub fn prometheus_text(&self) -> String {
        let r = Ordering::Relaxed;
        let mut s = String::with_capacity(4096);

        let _ = writeln!(
            s,
            "# HELP tau_statements_total Total statements processed by the executor"
        );
        let _ = writeln!(s, "# TYPE tau_statements_total counter");
        for (label, stats) in OP_LABELS.iter().zip(self.ops.iter()) {
            let _ = writeln!(
                s,
                "tau_statements_total{{type=\"{label}\"}} {}",
                stats.count.load(r)
            );
        }

        let _ = writeln!(
            s,
            "# HELP tau_statement_nanoseconds_total Cumulative executor time per statement type, in nanoseconds"
        );
        let _ = writeln!(s, "# TYPE tau_statement_nanoseconds_total counter");
        for (label, stats) in OP_LABELS.iter().zip(self.ops.iter()) {
            let _ = writeln!(
                s,
                "tau_statement_nanoseconds_total{{type=\"{label}\"}} {}",
                stats.ns.load(r)
            );
        }

        let _ = writeln!(
            s,
            "# HELP tau_statement_duration_microseconds Histogram of executor time per statement type, in microseconds"
        );
        let _ = writeln!(s, "# TYPE tau_statement_duration_microseconds histogram");
        for (label, stats) in OP_LABELS.iter().zip(self.ops.iter()) {
            for (i, bound) in LATENCY_BUCKETS_US.iter().enumerate() {
                let _ = writeln!(
                    s,
                    "tau_statement_duration_microseconds_bucket{{type=\"{label}\",le=\"{bound}\"}} {}",
                    stats.hist.counts[i].load(r)
                );
            }
            let _ = writeln!(
                s,
                "tau_statement_duration_microseconds_bucket{{type=\"{label}\",le=\"+Inf\"}} {}",
                stats.hist.count.load(r)
            );
            let _ = writeln!(
                s,
                "tau_statement_duration_microseconds_sum{{type=\"{label}\"}} {}",
                stats.hist.sum_us.load(r)
            );
            let _ = writeln!(
                s,
                "tau_statement_duration_microseconds_count{{type=\"{label}\"}} {}",
                stats.hist.count.load(r)
            );
        }

        let _ = writeln!(
            s,
            "# HELP tau_connections_total Total TCP connections accepted by the server"
        );
        let _ = writeln!(s, "# TYPE tau_connections_total counter");
        let _ = writeln!(s, "tau_connections_total {}", self.connections.load(r));

        let _ = writeln!(
            s,
            "# HELP tau_rejected_connections_total TCP connections rejected at the accept boundary because the server was at its concurrency limit"
        );
        let _ = writeln!(s, "# TYPE tau_rejected_connections_total counter");
        let _ = writeln!(
            s,
            "tau_rejected_connections_total {}",
            self.rejected_connections.load(r)
        );

        let _ = writeln!(
            s,
            "# HELP tau_auth_attempts_total Total AUTH attempts received (successful + failed)"
        );
        let _ = writeln!(s, "# TYPE tau_auth_attempts_total counter");
        let _ = writeln!(s, "tau_auth_attempts_total {}", self.auth_attempts.load(r));

        let _ = writeln!(
            s,
            "# HELP tau_auth_failures_total AUTH attempts that were rejected (wrong credentials or missing AUTH)"
        );
        let _ = writeln!(s, "# TYPE tau_auth_failures_total counter");
        let _ = writeln!(s, "tau_auth_failures_total {}", self.auth_failures.load(r));

        let _ = writeln!(
            s,
            "# HELP tau_errors_total Total ERR responses sent to clients"
        );
        let _ = writeln!(s, "# TYPE tau_errors_total counter");
        let _ = writeln!(s, "tau_errors_total {}", self.errors.load(r));

        let usage = ProcessUsage::sample();
        let _ = writeln!(
            s,
            "# HELP tau_process_resident_bytes Resident set size of the tau process, in bytes (0 if unsupported)"
        );
        let _ = writeln!(s, "# TYPE tau_process_resident_bytes gauge");
        let _ = writeln!(s, "tau_process_resident_bytes {}", usage.rss_bytes);

        let _ = writeln!(
            s,
            "# HELP tau_process_virtual_bytes Virtual memory size of the tau process, in bytes (0 if unsupported)"
        );
        let _ = writeln!(s, "# TYPE tau_process_virtual_bytes gauge");
        let _ = writeln!(s, "tau_process_virtual_bytes {}", usage.vsz_bytes);

        let _ = writeln!(
            s,
            "# HELP tau_process_open_fds Open file descriptors held by the tau process (0 if unsupported)"
        );
        let _ = writeln!(s, "# TYPE tau_process_open_fds gauge");
        let _ = writeln!(s, "tau_process_open_fds {}", usage.open_fds);

        let _ = writeln!(
            s,
            "# HELP tau_process_threads OS threads in the tau process (0 if unsupported)"
        );
        let _ = writeln!(s, "# TYPE tau_process_threads gauge");
        let _ = writeln!(s, "tau_process_threads {}", usage.threads);

        let _ = writeln!(
            s,
            "# HELP tau_process_uptime_seconds Wall-clock seconds since the metrics module was initialised"
        );
        let _ = writeln!(s, "# TYPE tau_process_uptime_seconds gauge");
        let _ = writeln!(s, "tau_process_uptime_seconds {}", uptime_seconds());

        let _ = writeln!(s, "# EOF");
        s
    }

    pub fn arc() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

/// Process-level resource snapshot.
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
    usage.open_fds = std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|d| d.flatten().count() as u64)
        .unwrap_or(0);
    Some(usage)
}

#[cfg(target_os = "linux")]
fn parse_kib(line: &str) -> Option<u64> {
    let trimmed = line.trim();
    let value = trimmed.split_whitespace().next()?;
    let kib: u64 = value.parse().ok()?;
    Some(kib.saturating_mul(1024))
}

fn uptime_seconds() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static STARTED: OnceLock<Instant> = OnceLock::new();
    let start = STARTED.get_or_init(Instant::now);
    start.elapsed().as_secs()
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
            assert_eq!(op.ns.load(r), 0);
        }
        assert_eq!(m.connections.load(r), 0);
        assert_eq!(m.rejected_connections.load(r), 0);
        assert_eq!(m.auth_attempts.load(r), 0);
        assert_eq!(m.auth_failures.load(r), 0);
        assert_eq!(m.errors.load(r), 0);
    }

    #[hegel::test]
    fn record_methods_increment_count_and_ns(tc: TestCase) {
        let samples = tc
            .draw(gs::vecs(gs::integers::<u64>().min_value(0).max_value(10_000_000)).max_size(16));
        let m = Metrics::new();
        for &ns in &samples {
            m.record_append(ns);
        }
        let r = Ordering::Relaxed;
        let append = &m.ops[Op::Append as usize];
        assert_eq!(append.count.load(r), samples.len() as u64);
        assert_eq!(append.ns.load(r), samples.iter().sum::<u64>());
        assert_eq!(append.hist.count.load(r), samples.len() as u64);
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
        let total = h.count.load(r);
        for c in &counts {
            assert!(*c <= total);
        }
    }

    #[hegel::test]
    fn counters_accumulate_linearly(tc: TestCase) {
        let auth_attempts = tc.draw(gs::integers::<u64>().min_value(0).max_value(50));
        let auth_failures = tc.draw(gs::integers::<u64>().min_value(0).max_value(50));
        let errors = tc.draw(gs::integers::<u64>().min_value(0).max_value(50));
        let rejected = tc.draw(gs::integers::<u64>().min_value(0).max_value(50));
        let m = Metrics::new();
        for _ in 0..auth_attempts {
            m.record_auth_attempt();
        }
        for _ in 0..auth_failures {
            m.record_auth_failure();
        }
        for _ in 0..errors {
            m.record_error();
        }
        for _ in 0..rejected {
            m.record_rejected_connection();
        }
        let r = Ordering::Relaxed;
        assert_eq!(m.auth_attempts.load(r), auth_attempts);
        assert_eq!(m.auth_failures.load(r), auth_failures);
        assert_eq!(m.errors.load(r), errors);
        assert_eq!(m.rejected_connections.load(r), rejected);
    }

    #[hegel::test]
    fn prometheus_text_exposes_every_documented_family(tc: TestCase) {
        let _ = tc;
        let m = Metrics::new();
        m.record_append(100);
        m.record_at(200);
        m.record_range(300);
        m.record_reduce(400);
        m.record_history(150);
        m.record_ddl(50);
        let text = m.prometheus_text();
        for line in [
            "# TYPE tau_statements_total counter",
            "# TYPE tau_statement_nanoseconds_total counter",
            "# TYPE tau_statement_duration_microseconds histogram",
            "# TYPE tau_connections_total counter",
            "# TYPE tau_rejected_connections_total counter",
            "# TYPE tau_auth_attempts_total counter",
            "# TYPE tau_auth_failures_total counter",
            "# TYPE tau_errors_total counter",
            "# TYPE tau_process_resident_bytes gauge",
        ] {
            assert!(text.contains(line), "missing: {line}");
        }
        assert!(text.contains("# EOF"));
    }

    #[hegel::test]
    fn prometheus_text_count_matches_counters(tc: TestCase) {
        let n_append = tc.draw(gs::integers::<u64>().min_value(0).max_value(20));
        let n_at = tc.draw(gs::integers::<u64>().min_value(0).max_value(20));
        let m = Metrics::new();
        for _ in 0..n_append {
            m.record_append(100);
        }
        for _ in 0..n_at {
            m.record_at(50);
        }
        let text = m.prometheus_text();
        assert!(text.contains(&format!(
            "tau_statements_total{{type=\"append\"}} {n_append}"
        )));
        assert!(text.contains(&format!("tau_statements_total{{type=\"at\"}} {n_at}")));
    }

    #[test]
    fn arc_constructor_returns_shared_metrics() {
        let m = Metrics::arc();
        m.record_append(1);
        assert_eq!(m.ops[Op::Append as usize].count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn default_is_same_as_new() {
        let m: Metrics = Default::default();
        assert_eq!(m.ops[Op::Append as usize].count.load(Ordering::Relaxed), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_usage_reports_nonzero_on_linux() {
        let u = ProcessUsage::sample();
        assert!(
            u.rss_bytes > 0,
            "RSS should be observable on Linux, got {u:?}"
        );
        assert!(u.threads > 0, "thread count should be observable on Linux");
    }
}
