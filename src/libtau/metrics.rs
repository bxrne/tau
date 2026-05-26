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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Histogram bucket upper bounds in microseconds.  Chosen so the buckets
/// reasonably span the latency range tau workloads produce: sub-microsecond
/// in-memory point lookups through half-second disk-fsync compactions.
const LATENCY_BUCKETS_US: &[u64] = &[
    1, 5, 10, 25, 50, 100, 250, 500, 1_000, 5_000, 10_000, 50_000, 100_000, 500_000,
];

/// A simple cumulative histogram backed by atomic counters.  Each bucket
/// stores the count of observations whose value is `<= bound`. The final
/// virtual `+Inf` bucket is `count`.
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

    /// Observe a single value in nanoseconds; converted to microseconds for
    /// bucketing so the buckets remain meaningful for cross-system comparison.
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

/// Per-operation execution counters and histograms for an [`crate::Executor`].
#[derive(Debug)]
pub struct Metrics {
    pub append_count: AtomicU64,
    pub at_count: AtomicU64,
    pub range_count: AtomicU64,
    pub reduce_count: AtomicU64,
    pub ddl_count: AtomicU64,

    pub append_ns: AtomicU64,
    pub at_ns: AtomicU64,
    pub range_ns: AtomicU64,
    pub reduce_ns: AtomicU64,
    pub ddl_ns: AtomicU64,

    pub append_hist: Histogram,
    pub at_hist: Histogram,
    pub range_hist: Histogram,
    pub reduce_hist: Histogram,
    pub ddl_hist: Histogram,

    /// Total TCP connections accepted by the server.
    pub connections: AtomicU64,
    /// Connections rejected because the server was at its concurrency limit.
    pub rejected_connections: AtomicU64,
    /// Total AUTH attempts (both successful and failed).
    pub auth_attempts: AtomicU64,
    /// Failed AUTH attempts (wrong password or missing AUTH before first query).
    pub auth_failures: AtomicU64,
    /// Total ERR responses sent to clients.
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
            append_count: AtomicU64::new(0),
            at_count: AtomicU64::new(0),
            range_count: AtomicU64::new(0),
            reduce_count: AtomicU64::new(0),
            ddl_count: AtomicU64::new(0),
            append_ns: AtomicU64::new(0),
            at_ns: AtomicU64::new(0),
            range_ns: AtomicU64::new(0),
            reduce_ns: AtomicU64::new(0),
            ddl_ns: AtomicU64::new(0),
            append_hist: Histogram::new(),
            at_hist: Histogram::new(),
            range_hist: Histogram::new(),
            reduce_hist: Histogram::new(),
            ddl_hist: Histogram::new(),
            connections: AtomicU64::new(0),
            rejected_connections: AtomicU64::new(0),
            auth_attempts: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record_append(&self, ns: u64) {
        self.append_count.fetch_add(1, Ordering::Relaxed);
        self.append_ns.fetch_add(ns, Ordering::Relaxed);
        self.append_hist.observe_ns(ns);
    }

    #[inline]
    pub fn record_at(&self, ns: u64) {
        self.at_count.fetch_add(1, Ordering::Relaxed);
        self.at_ns.fetch_add(ns, Ordering::Relaxed);
        self.at_hist.observe_ns(ns);
    }

    #[inline]
    pub fn record_range(&self, ns: u64) {
        self.range_count.fetch_add(1, Ordering::Relaxed);
        self.range_ns.fetch_add(ns, Ordering::Relaxed);
        self.range_hist.observe_ns(ns);
    }

    #[inline]
    pub fn record_reduce(&self, ns: u64) {
        self.reduce_count.fetch_add(1, Ordering::Relaxed);
        self.reduce_ns.fetch_add(ns, Ordering::Relaxed);
        self.reduce_hist.observe_ns(ns);
    }

    #[inline]
    pub fn record_ddl(&self, ns: u64) {
        self.ddl_count.fetch_add(1, Ordering::Relaxed);
        self.ddl_ns.fetch_add(ns, Ordering::Relaxed);
        self.ddl_hist.observe_ns(ns);
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
    ///
    /// The output is valid `text/plain; version=0.0.4`.
    pub fn prometheus_text(&self) -> String {
        let r = Ordering::Relaxed;
        let mut s = String::with_capacity(4096);

        let _ = writeln!(
            s,
            "# HELP tau_statements_total Total statements processed by the executor"
        );
        let _ = writeln!(s, "# TYPE tau_statements_total counter");
        for (label, counter) in [
            ("append", &self.append_count),
            ("at", &self.at_count),
            ("range", &self.range_count),
            ("reduce", &self.reduce_count),
            ("ddl", &self.ddl_count),
        ] {
            let _ = writeln!(
                s,
                "tau_statements_total{{type=\"{}\"}} {}",
                label,
                counter.load(r)
            );
        }

        let _ = writeln!(
            s,
            "# HELP tau_statement_nanoseconds_total Cumulative executor time per statement type, in nanoseconds"
        );
        let _ = writeln!(s, "# TYPE tau_statement_nanoseconds_total counter");
        for (label, counter) in [
            ("append", &self.append_ns),
            ("at", &self.at_ns),
            ("range", &self.range_ns),
            ("reduce", &self.reduce_ns),
            ("ddl", &self.ddl_ns),
        ] {
            let _ = writeln!(
                s,
                "tau_statement_nanoseconds_total{{type=\"{}\"}} {}",
                label,
                counter.load(r)
            );
        }

        let _ = writeln!(
            s,
            "# HELP tau_statement_duration_microseconds Histogram of executor time per statement type, in microseconds"
        );
        let _ = writeln!(s, "# TYPE tau_statement_duration_microseconds histogram");
        for (label, hist) in [
            ("append", &self.append_hist),
            ("at", &self.at_hist),
            ("range", &self.range_hist),
            ("reduce", &self.reduce_hist),
            ("ddl", &self.ddl_hist),
        ] {
            for (i, bound) in LATENCY_BUCKETS_US.iter().enumerate() {
                let _ = writeln!(
                    s,
                    "tau_statement_duration_microseconds_bucket{{type=\"{}\",le=\"{}\"}} {}",
                    label,
                    bound,
                    hist.counts[i].load(r)
                );
            }
            let _ = writeln!(
                s,
                "tau_statement_duration_microseconds_bucket{{type=\"{}\",le=\"+Inf\"}} {}",
                label,
                hist.count.load(r)
            );
            let _ = writeln!(
                s,
                "tau_statement_duration_microseconds_sum{{type=\"{}\"}} {}",
                label,
                hist.sum_us.load(r)
            );
            let _ = writeln!(
                s,
                "tau_statement_duration_microseconds_count{{type=\"{}\"}} {}",
                label,
                hist.count.load(r)
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

/// Process-level resource snapshot. All fields default to `0` on unsupported
/// platforms.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessUsage {
    pub rss_bytes: u64,
    pub vsz_bytes: u64,
    pub open_fds: u64,
    pub threads: u64,
}

impl ProcessUsage {
    /// Sample the current process. On Linux reads `/proc/self/status` and
    /// counts `/proc/self/fd/`. Returns zeros on any error or on non-Linux
    /// platforms.
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

    #[test]
    fn counters_start_at_zero() {
        let m = Metrics::new();
        let r = Ordering::Relaxed;
        assert_eq!(m.append_count.load(r), 0);
        assert_eq!(m.at_count.load(r), 0);
        assert_eq!(m.range_count.load(r), 0);
        assert_eq!(m.reduce_count.load(r), 0);
        assert_eq!(m.ddl_count.load(r), 0);
        assert_eq!(m.connections.load(r), 0);
        assert_eq!(m.rejected_connections.load(r), 0);
        assert_eq!(m.auth_attempts.load(r), 0);
        assert_eq!(m.auth_failures.load(r), 0);
        assert_eq!(m.errors.load(r), 0);
    }

    #[test]
    fn record_append_observes_histogram() {
        let m = Metrics::new();
        m.record_append(500);
        m.record_append(300);
        assert_eq!(m.append_count.load(Ordering::Relaxed), 2);
        assert_eq!(m.append_ns.load(Ordering::Relaxed), 800);
        // 500ns -> 0us (rounds down), 300ns -> 0us.
        assert_eq!(m.append_hist.count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn histogram_bucket_assignment() {
        let h = Histogram::new();
        h.observe_ns(7_000); // 7us
        h.observe_ns(70_000); // 70us
        // 7us falls into the le=10 bucket and every higher bucket.
        let i_10 = LATENCY_BUCKETS_US.iter().position(|b| *b == 10).unwrap();
        assert!(h.counts[i_10].load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn record_auth_attempt_increments() {
        let m = Metrics::new();
        m.record_auth_attempt();
        m.record_auth_attempt();
        assert_eq!(m.auth_attempts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn record_auth_failure_increments() {
        let m = Metrics::new();
        m.record_auth_failure();
        assert_eq!(m.auth_failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_error_increments() {
        let m = Metrics::new();
        m.record_error();
        m.record_error();
        m.record_error();
        assert_eq!(m.errors.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn record_rejected_connection_increments() {
        let m = Metrics::new();
        m.record_rejected_connection();
        m.record_rejected_connection();
        assert_eq!(m.rejected_connections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn prometheus_text_contains_expected_families() {
        let m = Metrics::new();
        m.record_append(100);
        m.record_at(200);
        m.record_range(300);
        m.record_reduce(400);
        m.record_ddl(50);
        m.connections.fetch_add(3, Ordering::Relaxed);
        m.record_rejected_connection();
        m.record_auth_attempt();
        m.record_auth_attempt();
        m.record_auth_failure();
        m.record_error();

        let text = m.prometheus_text();
        assert!(text.contains("tau_statements_total{type=\"append\"} 1"));
        assert!(text.contains("tau_statement_duration_microseconds_count{type=\"append\"}"));
        assert!(text.contains("tau_statement_duration_microseconds_bucket{type=\"append\","));
        assert!(text.contains("tau_connections_total 3"));
        assert!(text.contains("tau_rejected_connections_total 1"));
        assert!(text.contains("tau_auth_attempts_total 2"));
        assert!(text.contains("tau_auth_failures_total 1"));
        assert!(text.contains("tau_errors_total 1"));
        assert!(text.contains("tau_process_resident_bytes"));
        assert!(text.contains("tau_process_open_fds"));
        assert!(text.contains("tau_process_threads"));
        assert!(text.contains("tau_process_uptime_seconds"));
        assert!(text.contains("# EOF"));
    }

    #[test]
    fn prometheus_text_has_type_lines() {
        let m = Metrics::new();
        let text = m.prometheus_text();
        assert!(text.contains("# TYPE tau_statements_total counter"));
        assert!(text.contains("# TYPE tau_statement_nanoseconds_total counter"));
        assert!(text.contains("# TYPE tau_statement_duration_microseconds histogram"));
        assert!(text.contains("# TYPE tau_connections_total counter"));
        assert!(text.contains("# TYPE tau_rejected_connections_total counter"));
        assert!(text.contains("# TYPE tau_auth_attempts_total counter"));
        assert!(text.contains("# TYPE tau_auth_failures_total counter"));
        assert!(text.contains("# TYPE tau_errors_total counter"));
        assert!(text.contains("# TYPE tau_process_resident_bytes gauge"));
    }

    #[test]
    fn arc_constructor_returns_shared_metrics() {
        let m = Metrics::arc();
        m.record_append(1);
        assert_eq!(m.append_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn default_is_same_as_new() {
        let m: Metrics = Default::default();
        assert_eq!(m.append_count.load(Ordering::Relaxed), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_usage_reports_nonzero_on_linux() {
        let u = ProcessUsage::sample();
        assert!(
            u.rss_bytes > 0,
            "RSS should be observable on Linux, got {:?}",
            u
        );
        assert!(u.threads > 0, "thread count should be observable on Linux");
    }
}
