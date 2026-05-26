//! Deterministic benchmark driver for the tau database.
//!
//! Spawns a real `tau` server process for each configuration cell, drives
//! workload traffic over TCP (plain or TLS, unauthenticated or authenticated),
//! then scrapes the server's Prometheus `/metrics` endpoint for the final
//! numbers.  This gives coverage across every server dimension:
//!
//! | dimension      | values tested                      |
//! |----------------|------------------------------------|
//! | transport      | plain TCP, TLS                     |
//! | authentication | none, username/password (argon2id) |
//! | WAL            | off, on                            |
//! | compaction     | configurable threshold             |
//! | workload       | append, at-lookup, range, reduce   |
//!
//! ## Usage
//!
//! ```text
//! bench [OPTIONS]
//!
//! Options:
//!   --quick            Smaller scale + fewer cells (CI-suitable)
//!   --out PATH         Write CSV results to PATH
//!   --label NAME       Tag every row with NAME (default: "run")
//!   --repeat N         Best-of-N runs per cell (default: 3)
//!   --compact-threshold N  Compaction threshold passed to the server
//!   --ops N            Operations per workload run (default: 1000)
//!   --scratch DIR      Directory for server WAL files (default: $TMPDIR)
//! ```

use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use std::{fmt, fs};

use clap::Parser;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

/// Benchmark driver for the tau database.
///
/// Spawns a real tau server per cell configuration and measures throughput
/// and latency via the server's Prometheus /metrics endpoint.
#[derive(Parser, Debug)]
#[command(name = "bench", author, version)]
#[command(about = "tau deterministic benchmark", long_about = None)]
struct Cli {
    /// Run a small fast grid suitable for CI (fewer cells, lower op counts).
    #[arg(long)]
    quick: bool,

    /// Write CSV results to this path.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,

    /// Label attached to every CSV row.
    #[arg(long, default_value = "run")]
    label: String,

    /// Best-of-N repeats per cell (higher = more reliable "best" pick).
    #[arg(long, default_value_t = 3)]
    repeat: usize,

    /// Compaction threshold passed to each server instance.
    #[arg(long, default_value_t = 64)]
    compact_threshold: usize,

    /// Number of data-plane operations per workload run.
    #[arg(long, default_value_t = 1000)]
    ops: usize,

    /// Directory for server WAL files (tmpfs by default - use a real disk
    /// path if you want to measure durable-write throughput).
    #[arg(long, value_name = "DIR")]
    scratch: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Transport {
    Plain,
    Tls,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain => f.write_str("plain"),
            Self::Tls => f.write_str("tls"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Auth {
    None,
    Password,
}

impl fmt::Display for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("none"),
            Self::Password => f.write_str("password"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Wal {
    Off,
    On,
}

impl fmt::Display for Wal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::On => f.write_str("on"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Workload {
    Append,
    AtLookup,
    Range,
    Reduce,
}

impl fmt::Display for Workload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Append => f.write_str("append"),
            Self::AtLookup => f.write_str("at-lookup"),
            Self::Range => f.write_str("range"),
            Self::Reduce => f.write_str("reduce"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    transport: Transport,
    auth: Auth,
    wal: Wal,
    compact_threshold: usize,
}

const BENCH_USER: &str = "bench";
const BENCH_PASS: &str = "bench_password_1";

/// A running tau server subprocess.
struct Server {
    child: Child,
    addr: String,
    metrics_port: u16,
    transport: Transport,
    auth: Auth,
    /// RAII guard: keeps the scratch directory alive until the server is dropped.
    scratch: Option<ScratchDir>,
}

impl Server {
    fn spawn(cell: &Cell, scratch_root: &Path) -> io::Result<Self> {
        let query_port = free_port()?;
        let metrics_port = free_port()?;

        let mut scratch: Option<ScratchDir> = None;
        let mut cmd = Command::new(tau_binary());
        cmd.arg(format!("127.0.0.1:{query_port}"));
        cmd.arg("--metrics-port").arg(metrics_port.to_string());
        cmd.arg("--compact-threshold")
            .arg(cell.compact_threshold.to_string());
        cmd.arg("--log-level").arg("error"); // suppress server output during bench

        if cell.wal == Wal::On {
            let dir = ScratchDir::new(scratch_root);
            cmd.arg("--wal");
            cmd.arg("-w").arg(dir.path().join("bench.wal"));
            scratch = Some(dir);
        }

        if cell.transport == Transport::Tls {
            cmd.arg("--tls"); // server generates ephemeral self-signed cert
        }

        if cell.auth == Auth::Password {
            cmd.arg("--auth")
                .arg("--username")
                .arg(BENCH_USER)
                .arg("--password")
                .arg(BENCH_PASS);
        }

        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "could not spawn tau binary at {:?}: {}. Build with `cargo build --release` first.",
                        tau_binary(),
                        e
                    ),
                )
            })?;

        let addr = format!("127.0.0.1:{query_port}");
        Ok(Self {
            child,
            addr,
            metrics_port,
            transport: cell.transport,
            auth: cell.auth,
            scratch,
        })
    }

    /// Block until the server is accepting connections (up to 5 s).
    fn wait_ready(&self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "server did not become ready within 5 s",
                ));
            }
            match TcpStream::connect(&self.addr) {
                Ok(_) => return Ok(()),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// Open a fresh client connection (plain or TLS).  Sends AUTH if needed.
    fn connect(&self) -> io::Result<Conn> {
        Conn::open(&self.addr, self.transport, self.auth)
    }

    /// GET /metrics and return the parsed key->value map.
    fn scrape_metrics(&self) -> io::Result<HashMap<String, u64>> {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", self.metrics_port))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        write!(s, "GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        s.flush()?;
        let mut body = String::new();
        BufReader::new(s).read_to_string(&mut body)?;
        Ok(parse_prometheus_text(&body))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Explicit take so ScratchDir's Drop runs before we exit and so that
        // the field is observably read (dead_code lint).
        drop(self.scratch.take());
    }
}

enum ConnBackend {
    Plain(BufReader<TcpStream>),
    Tls(Box<BufReader<StreamOwned<ClientConnection, TcpStream>>>),
}

impl ConnBackend {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(r) => r.get_mut().write_all(buf),
            Self::Tls(r) => r.get_mut().write_all(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(r) => r.get_mut().flush(),
            Self::Tls(r) => r.get_mut().flush(),
        }
    }
    fn read_line(&mut self, buf: &mut String) -> io::Result<usize> {
        match self {
            Self::Plain(r) => r.read_line(buf),
            Self::Tls(r) => r.read_line(buf),
        }
    }
}

struct Conn(ConnBackend);

impl Conn {
    fn open(addr: &str, transport: Transport, auth: Auth) -> io::Result<Self> {
        let backend = match transport {
            Transport::Plain => {
                let s = TcpStream::connect(addr)?;
                // Disable Nagle's algorithm: we send small framed lines and
                // need every write to be sent immediately to avoid the ~40 ms
                // Nagle + delayed-ACK interaction.
                s.set_nodelay(true)?;
                ConnBackend::Plain(BufReader::new(s))
            }
            Transport::Tls => {
                let tcp = TcpStream::connect(addr)?;
                let config = ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoVerify))
                    .with_no_client_auth();
                let sn = ServerName::try_from("localhost".to_string())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
                let client =
                    ClientConnection::new(Arc::new(config), sn).map_err(io::Error::other)?;
                ConnBackend::Tls(Box::new(BufReader::new(StreamOwned::new(client, tcp))))
            }
        };
        let mut conn = Self(backend);
        if auth == Auth::Password {
            let resp = conn.send(&format!("AUTH {BENCH_USER} {BENCH_PASS}"))?;
            if !resp.starts_with("OK") {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("AUTH failed: {resp}"),
                ));
            }
        }
        Ok(conn)
    }

    fn send(&mut self, line: &str) -> io::Result<String> {
        // Write line + newline in one call to avoid Nagle + delayed-ACK stalls.
        let mut msg = Vec::with_capacity(line.len() + 1);
        msg.extend_from_slice(line.as_bytes());
        msg.push(b'\n');
        self.0.write_all(&msg)?;
        self.0.flush()?;
        let mut resp = String::new();
        let n = self.0.read_line(&mut resp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed connection",
            ));
        }
        Ok(resp.trim_end_matches(['\r', '\n']).to_string())
    }
}

#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

/// Simple xorshift RNG - no deps required.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    #[inline(always)]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    #[inline(always)]
    fn next_range(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Populate the server with `n` APPEND statements via `conn`.
/// Returns the approximate time range covered: [0, n * 10).
fn populate(conn: &mut Conn, n: usize) -> io::Result<()> {
    let mut rng = Rng::new(0xBEEF_u64.wrapping_add(n as u64));
    for i in 0..n {
        let start = i as i64 * 10;
        let end = start + 5;
        let val = (rng.next_u64() & 0xFFFF) as i64;
        let resp = conn.send(&format!("APPEND LENS s {start} {end} {val}"))?;
        if !resp.starts_with("OK") {
            return Err(io::Error::other(format!("APPEND failed: {resp}")));
        }
    }
    Ok(())
}

/// Run the append workload: `ops` individual APPEND statements.
fn run_append(conn: &mut Conn, ops: usize) -> io::Result<()> {
    let mut rng = Rng::new(0xA99E_u64.wrapping_add(ops as u64));
    for i in 0..ops {
        let start = (i as i64 + 1_000_000) * 10;
        let end = start + 5;
        let val = (rng.next_u64() & 0xFFFF) as i64;
        let resp = conn.send(&format!("APPEND LENS s {start} {end} {val}"))?;
        if !resp.starts_with("OK") {
            return Err(io::Error::other(format!("APPEND failed: {resp}")));
        }
    }
    Ok(())
}

/// Run the at-lookup workload: `ops` random AT queries against a populated lens.
fn run_at(conn: &mut Conn, ops: usize, data_n: usize) -> io::Result<()> {
    let max_t = (data_n as i64) * 10;
    let mut rng = Rng::new(0xA7_u64.wrapping_add(ops as u64));
    for _ in 0..ops {
        let t = rng.next_range(max_t.max(1) as u64) as i64;
        let resp = conn.send(&format!("AT LENS s {t}"))?;
        if !resp.starts_with("VAL") {
            return Err(io::Error::other(format!("AT failed: {resp}")));
        }
    }
    Ok(())
}

/// Run the range-scan workload: `ops` random RANGE queries.
fn run_range(conn: &mut Conn, ops: usize, data_n: usize) -> io::Result<()> {
    let max_t = (data_n as i64) * 10;
    let mut rng = Rng::new(0x8A4_u64.wrapping_add(ops as u64));
    for _ in 0..ops {
        let s = rng.next_range(max_t.max(1) as u64) as i64;
        let width = (rng.next_range(50) + 20) as i64 * 10;
        let e = s.saturating_add(width);
        let resp = conn.send(&format!("RANGE LENS s {s} {e}"))?;
        if !resp.starts_with("RANGE") {
            return Err(io::Error::other(format!("RANGE failed: {resp}")));
        }
    }
    Ok(())
}

/// Run the reduce workload: `ops` random REDUCE USING avg queries.
fn run_reduce(conn: &mut Conn, ops: usize, data_n: usize) -> io::Result<()> {
    let max_t = (data_n as i64) * 10;
    let mut rng = Rng::new(0x8ED_u64.wrapping_add(ops as u64));
    for _ in 0..ops {
        let s = rng.next_range(max_t.max(1) as u64) as i64;
        let width = (rng.next_range(100) + 50) as i64 * 10;
        let e = s.saturating_add(width);
        let resp = conn.send(&format!("REDUCE LENS s {s} {e} USING avg"))?;
        if !resp.starts_with("VAL") {
            return Err(io::Error::other(format!("REDUCE failed: {resp}")));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Row {
    label: String,
    transport: Transport,
    auth: Auth,
    wal: Wal,
    compact_threshold: usize,
    workload: Workload,
    ops: u64,
    // best run
    best_ns_per_op: f64,
    best_ops_per_sec: f64,
    runs: usize,
}

fn main() {
    let cli = Cli::parse();
    let scratch_root = cli.scratch.clone().unwrap_or_else(env::temp_dir);
    fs::create_dir_all(&scratch_root).expect("scratch root");

    // Verify tau binary exists before starting.
    let tau_bin = tau_binary();
    if !tau_bin.exists() {
        eprintln!(
            "error: tau binary not found at {:?}.\nBuild with: cargo build --release",
            tau_bin
        );
        process::exit(1);
    }

    // Build the cell grid.
    let cells = build_grid(cli.quick, cli.compact_threshold);
    let workloads = [
        Workload::Append,
        Workload::AtLookup,
        Workload::Range,
        Workload::Reduce,
    ];

    let ops = cli.ops;
    let data_n = ops; // populate size matches ops count

    println!(
        "tau bench {} - label={} cells={} workloads={} ops={} repeat={}",
        env!("CARGO_PKG_VERSION"),
        cli.label,
        cells.len(),
        workloads.len(),
        ops,
        cli.repeat,
    );
    println!(
        "{:<12} {:<8} {:<10} {:<5} {:<10} {:<10} {:<14} {:<14}",
        "workload", "transport", "auth", "wal", "compact_t", "ops", "ns/op (best)", "ops/s (best)",
    );

    let mut rows: Vec<Row> = Vec::new();
    let started = Instant::now();

    for cell in &cells {
        for &workload in &workloads {
            let mut best_ns = f64::INFINITY;
            let mut best_ops = 0u64;

            for _ in 0..cli.repeat {
                let result = run_cell(cell, workload, ops, data_n, &scratch_root);
                match result {
                    Ok((ops_count, ns_per_op)) => {
                        best_ns = best_ns.min(ns_per_op);
                        best_ops = ops_count;
                    }
                    Err(e) => {
                        eprintln!(
                            "  WARN cell={}/{}/{} workload={}: {}",
                            cell.transport, cell.auth, cell.wal, workload, e
                        );
                    }
                }
            }

            if best_ns.is_finite() {
                let ops_per_sec = 1e9 / best_ns.max(1e-9);
                println!(
                    "{:<12} {:<8} {:<10} {:<5} {:<10} {:<10} {:<14.2} {:<14.0}",
                    workload.to_string(),
                    cell.transport.to_string(),
                    cell.auth.to_string(),
                    cell.wal.to_string(),
                    cell.compact_threshold,
                    best_ops,
                    best_ns,
                    ops_per_sec,
                );
                rows.push(Row {
                    label: cli.label.clone(),
                    transport: cell.transport,
                    auth: cell.auth,
                    wal: cell.wal,
                    compact_threshold: cell.compact_threshold,
                    workload,
                    ops: best_ops,
                    best_ns_per_op: best_ns,
                    best_ops_per_sec: ops_per_sec,
                    runs: cli.repeat,
                });
            }
        }
    }

    let wall = started.elapsed();
    println!("done in {}", format_duration(wall));

    if let Some(path) = &cli.out {
        write_csv(path, &rows);
        println!("wrote {}", path.display());
    }
}

/// Spawn a server for `cell`, run `workload`, scrape metrics, return
/// `(ops_counted, ns_per_op)`.
fn run_cell(
    cell: &Cell,
    workload: Workload,
    ops: usize,
    data_n: usize,
    scratch_root: &Path,
) -> io::Result<(u64, f64)> {
    let srv = Server::spawn(cell, scratch_root)?;
    srv.wait_ready()?;

    // Setup: create database and lens.
    let mut setup = srv.connect()?;
    setup.send("CREATE DATABASE bench")?;
    setup.send("CREATE LENS s int")?;

    // Pre-populate for read workloads so there is data to query.
    if workload != Workload::Append {
        populate(&mut setup, data_n)?;
    }
    drop(setup); // close setup connection

    // Snapshot metrics before workload.
    let m_before = srv.scrape_metrics()?;
    let t_start = Instant::now();

    // Run workload on a fresh connection.
    let mut conn = srv.connect()?;
    match workload {
        Workload::Append => run_append(&mut conn, ops)?,
        Workload::AtLookup => run_at(&mut conn, ops, data_n)?,
        Workload::Range => run_range(&mut conn, ops, data_n)?,
        Workload::Reduce => run_reduce(&mut conn, ops, data_n)?,
    }
    drop(conn);

    let wall_ns = t_start.elapsed().as_nanos() as u64;

    // Snapshot metrics after workload.
    let m_after = srv.scrape_metrics()?;

    // Derive ops and ns from the metrics delta.
    let (count_key, ns_key) = workload_metric_keys(workload);
    let ops_delta = m_after
        .get(count_key)
        .unwrap_or(&0)
        .saturating_sub(*m_before.get(count_key).unwrap_or(&0));
    let ns_delta = m_after
        .get(ns_key)
        .unwrap_or(&0)
        .saturating_sub(*m_before.get(ns_key).unwrap_or(&0));

    // ns_per_op from server-side execution time (excludes TCP/TLS overhead).
    // Fall back to wall-clock / ops if the counter didn't advance (e.g. no
    // operations actually reached the executor).
    let effective_ops = ops_delta.max(1);
    let ns_per_op = if ns_delta > 0 {
        ns_delta as f64 / effective_ops as f64
    } else {
        wall_ns as f64 / effective_ops as f64
    };

    Ok((effective_ops, ns_per_op))
}

fn workload_metric_keys(w: Workload) -> (&'static str, &'static str) {
    match w {
        Workload::Append => (
            "tau_statements_total{type=\"append\"}",
            "tau_statement_nanoseconds_total{type=\"append\"}",
        ),
        Workload::AtLookup => (
            "tau_statements_total{type=\"at\"}",
            "tau_statement_nanoseconds_total{type=\"at\"}",
        ),
        Workload::Range => (
            "tau_statements_total{type=\"range\"}",
            "tau_statement_nanoseconds_total{type=\"range\"}",
        ),
        Workload::Reduce => (
            "tau_statements_total{type=\"reduce\"}",
            "tau_statement_nanoseconds_total{type=\"reduce\"}",
        ),
    }
}

fn build_grid(quick: bool, compact_threshold: usize) -> Vec<Cell> {
    let transports = if quick {
        vec![Transport::Plain]
    } else {
        vec![Transport::Plain, Transport::Tls]
    };
    let auths = if quick {
        vec![Auth::None]
    } else {
        vec![Auth::None, Auth::Password]
    };
    let wals = [Wal::Off, Wal::On];

    let mut cells = Vec::new();
    for &transport in &transports {
        for &auth in &auths {
            for &wal in &wals {
                cells.push(Cell {
                    transport,
                    auth,
                    wal,
                    compact_threshold,
                });
            }
        }
    }
    cells
}

fn parse_prometheus_text(body: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    // Find the HTTP body after the blank line.
    let text = if let Some(idx) = body.find("\r\n\r\n") {
        &body[idx + 4..]
    } else if let Some(idx) = body.find("\n\n") {
        &body[idx + 2..]
    } else {
        body
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Format: `metric_name{labels} value` or `metric_name value`
        if let Some((key, val_str)) = line.rsplit_once(' ')
            && let Ok(v) = val_str.parse::<u64>()
        {
            map.insert(key.trim().to_string(), v);
        }
    }
    map
}

fn tau_binary() -> PathBuf {
    // When running from `cargo run --bin bench`, the tau binary lives next to
    // the bench binary in the same target directory.
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("tau")))
        .unwrap_or_else(|| PathBuf::from("tau"))
}

fn free_port() -> io::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
    // Listener is dropped here; there is a small TOCTOU race but it is
    // acceptable in a controlled benchmark environment.
}

fn format_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 60.0 {
        format!("{}m{:.1}s", (s / 60.0) as u64, s % 60.0)
    } else {
        format!("{:.2}s", s)
    }
}

fn write_csv(path: &Path, rows: &[Row]) {
    let mut f = fs::File::create(path).expect("open csv");
    writeln!(
        f,
        "label,workload,transport,auth,wal,compact_threshold,ops,runs,best_ns_per_op,best_ops_per_sec"
    )
    .unwrap();
    for r in rows {
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{:.3},{:.1}",
            r.label,
            r.workload,
            r.transport,
            r.auth,
            r.wal,
            r.compact_threshold,
            r.ops,
            r.runs,
            r.best_ns_per_op,
            r.best_ops_per_sec,
        )
        .unwrap();
    }
}

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(root: &Path) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("tau-bench-{}-{}", process::id(), n));
        fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
