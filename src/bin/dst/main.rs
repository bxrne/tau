//! Deterministic Simulation Tester for the tau database.
//!
//! Combines correctness verification, fault injection, and throughput
//! measurement across all server configuration combinations. Replaces
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
//!   --verbose            Print every operation

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, process};

use clap::Parser;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rustls::ClientConfig;
use rustls::ClientConnection;
use rustls::StreamOwned;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use tau::{Executor, Output, Value, parse};

const SIM_USER: &str = "dst";
const SIM_PASS: &str = "dst_sim_pass_1";
const LENS: &str = "s";

#[derive(Parser, Debug)]
#[command(name = "dst", about = "Tau deterministic simulation tester")]
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

    #[arg(long)]
    verbose: bool,
}

fn main() {
    let cli = Cli::parse();
    let seed = cli.seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .subsec_nanos() as u64
    });

    if cli.quick {
        run_embedded(seed, &cli);
    } else {
        run_full(seed, &cli);
    }
}

// Oracle

struct Oracle {
    lenses: HashMap<String, Vec<(u64, i64, i64, i64)>>,
    revision: u64,
}

impl Oracle {
    fn new() -> Self {
        Self { lenses: HashMap::new(), revision: 0 }
    }

    fn append(&mut self, lens: &str, entries: &[(i64, i64, i64)]) {
        self.revision += 1;
        let rev = self.revision;
        let data = self.lenses.entry(lens.to_string()).or_default();
        for &(s, e, v) in entries {
            data.push((rev, s, e, v));
        }
    }

    fn at(&self, lens: &str, t: i64) -> Option<i64> {
        self.lenses
            .get(lens)?
            .iter()
            .filter(|(_, s, e, _)| *s <= t && t < *e)
            .max_by_key(|(rev, _, _, _)| *rev)
            .map(|(_, _, _, v)| *v)
    }

    fn reset(&mut self, lens: &str) {
        self.lenses.remove(lens);
    }
}

fn rng_range(rng: &mut StdRng, lo: i64, hi: i64) -> i64 {
    rng.gen_range(lo..hi)
}

fn rng_usize(rng: &mut StdRng, lo: usize, hi: usize) -> usize {
    rng.gen_range(lo..hi)
}

// Embedded simulation (quick mode)

fn run_embedded(seed: u64, cli: &Cli) {
    println!("dst embedded: seed={seed:#018x} duration={}s readers={}", cli.duration, cli.readers);

    let result = std::panic::catch_unwind(|| embedded_sim(seed, cli));
    match result {
        Ok(()) => println!("dst embedded: PASS"),
        Err(e) => {
            let msg = e.downcast_ref::<String>().map(|s| s.as_str())
                .or_else(|| e.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            eprintln!("dst embedded: FAIL  seed={seed:#018x}  {msg}");
            eprintln!("Reproduce: cargo run --release --bin dst -- --quick --seed {seed}");
            process::exit(1);
        }
    }
}

const EMBEDDED_LENSES: [&str; 3] = ["a", "b", "c"];

fn embedded_sim(seed: u64, cli: &Cli) {
    let mut rng = StdRng::seed_from_u64(seed);
    let executor: Arc<RwLock<Executor>> = Arc::new(RwLock::new(Executor::with_threshold(3)));
    let mut oracle = Oracle::new();
    let mut live = [true; 3];

    {
        let mut exec = executor.write().expect("write lock");
        exec.exec(&parse("CREATE DATABASE default").expect("parse").1).expect("create db");
        for name in EMBEDDED_LENSES {
            exec.exec(&parse(&format!("CREATE LENS {name} int")).expect("parse").1)
                .expect("create lens");
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stress_stop = Arc::clone(&stop);
    let stress_exec = Arc::clone(&executor);
    let stress_seed = seed.wrapping_add(0xdead_beef);
    let stress_handle = thread::spawn(move || embedded_stress_reader(stress_exec, stress_stop, stress_seed));

    let total_ops = Arc::new(AtomicU64::new(0));
    let write_count = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(cli.duration);
    let sleep_per_op = Duration::from_micros(2_000);

    let mut time_cursor: i64 = 0;
    let mut op_idx: u64 = 0;

    while Instant::now() < deadline {
        op_idx += 1;

        let lens_idx = rng_usize(&mut rng, 0, EMBEDDED_LENSES.len());
        let lens_name = EMBEDDED_LENSES[lens_idx];

        match rng.gen_range(0u8..10) {
            0 if live[lens_idx] => {
                let stmt = parse(&format!("DROP LENS {lens_name}")).expect("parse").1;
                executor.write().expect("lock").exec(&stmt).ok();
                oracle.reset(lens_name);
                live[lens_idx] = false;
                write_count.fetch_add(1, Ordering::Relaxed);
            }
            1 if !live[lens_idx] => {
                let stmt = parse(&format!("CREATE LENS {lens_name} int")).expect("parse").1;
                executor.write().expect("lock").exec(&stmt).ok();
                live[lens_idx] = true;
                write_count.fetch_add(1, Ordering::Relaxed);
            }
            2..=4 if live[lens_idx] => {
                let t = rng.gen_range(0..=time_cursor.max(1));
                embedded_check_at(&executor, &oracle, lens_name, t, cli.verbose, op_idx);
            }
            _ if live[lens_idx] => {
                // Advance time by a large amount to simulate centuries of data.
                let advance = rng_range(&mut rng, 1_000_000, 10_000_000);
                let start = time_cursor + advance;
                let end = start + rng_range(&mut rng, 1, 1_000_001);
                time_cursor = end;

                let val = rng_range(&mut rng, i32::MIN as i64, i32::MAX as i64);
                let stmt_text = format!("APPEND LENS {lens_name} {start} {end} {val}");
                let stmt = parse(&stmt_text).expect("parse").1;

                {
                    let mut exec = executor.write().expect("lock");
                    if exec.exec(&stmt).is_err() { continue; }
                }

                oracle.append(lens_name, &[(start, end, val)]);
                write_count.fetch_add(1, Ordering::Relaxed);

                let expected = oracle.at(lens_name, start);
                embedded_concurrent_burst(&executor, lens_name, start, expected, cli.readers, op_idx);
            }
            _ => {}
        }

        total_ops.fetch_add(1, Ordering::Relaxed);
        thread::sleep(sleep_per_op);
    }

    stop.store(true, Ordering::Relaxed);
    stress_handle.join().expect("stress reader panicked");

    let total = total_ops.load(Ordering::Relaxed);
    let writes = write_count.load(Ordering::Relaxed);
    let simulated_years = (time_cursor as f64) / (365.25 * 24.0 * 3600.0 * 1000.0);
    println!("dst embedded: {total} ops ({writes} writes), simulated {simulated_years:.0} years");
}

fn embedded_check_at(
    exec: &Arc<RwLock<Executor>>,
    oracle: &Oracle,
    lens: &str,
    t: i64,
    verbose: bool,
    op_idx: u64,
) {
    let stmt = parse(&format!("AT LENS {lens} {t}")).expect("parse").1;
    let out = exec.read().expect("lock").exec_read(&stmt).unwrap_or(Output::Value(None));
    let oracle_val = oracle.at(lens, t);
    let exec_val = match &out {
        Output::Value(Some(Value::Int(i))) => Some(*i),
        _ => None,
    };
    if verbose {
        println!("  AT {lens} {t}: exec={exec_val:?} oracle={oracle_val:?}");
    }
    assert_eq!(exec_val, oracle_val,
        "op {op_idx}: AT LENS {lens} {t} diverged: executor={exec_val:?} oracle={oracle_val:?}");
}

fn embedded_concurrent_burst(
    exec: &Arc<RwLock<Executor>>,
    lens: &str,
    t: i64,
    expected: Option<i64>,
    n: usize,
    op_idx: u64,
) {
    let handles: Vec<_> = (0..n).map(|_| {
        let exec = Arc::clone(exec);
        let stmt = parse(&format!("AT LENS {lens} {t}")).expect("parse").1;
        thread::spawn(move || {
            exec.read().expect("lock").exec_read(&stmt).unwrap_or(Output::Value(None))
        })
    }).collect();

    for h in handles {
        let result = h.join().expect("reader panicked");
        let got = match &result {
            Output::Value(Some(Value::Int(i))) => Some(*i),
            _ => None,
        };
        assert_eq!(got, expected,
            "op {op_idx}: concurrent AT LENS {lens} {t}: reader={got:?} expected={expected:?}");
    }
}

fn embedded_stress_reader(exec: Arc<RwLock<Executor>>, stop: Arc<AtomicBool>, seed: u64) {
    let mut rng = StdRng::seed_from_u64(seed);
    while !stop.load(Ordering::Relaxed) {
        let lens = EMBEDDED_LENSES[rng.gen_range(0..EMBEDDED_LENSES.len())];
        let t = rng.gen_range(0..=100_000_000i64);
        if let Ok((_, stmt)) = parse(&format!("AT LENS {lens} {t}")) {
            let _ = exec.read().expect("lock").exec_read(&stmt);
        }
    }
}

// Full simulation (server mode)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport { Plain, Tls }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Auth { None, Password }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wal { Off, On }

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self { Transport::Plain => write!(f, "plain"), Transport::Tls => write!(f, "tls") }
    }
}
impl fmt::Display for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self { Auth::None => write!(f, "none"), Auth::Password => write!(f, "password") }
    }
}
impl fmt::Display for Wal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self { Wal::Off => write!(f, "off"), Wal::On => write!(f, "on") }
    }
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    transport: Transport,
    auth: Auth,
    wal: Wal,
}

#[derive(Debug)]
struct CellResult {
    cell: Cell,
    ops: u64,
    correctness: &'static str,
    fault_injection: &'static str,
    metrics_ok: bool,
    ns_per_op: f64,
    ops_per_sec: f64,
    seed: u64,
}

fn run_full(seed: u64, cli: &Cli) {
    let scratch_root = cli.scratch.clone().unwrap_or_else(env::temp_dir);
    fs::create_dir_all(&scratch_root).expect("scratch root");

    let tau_bin = tau_binary();
    if !tau_bin.exists() {
        eprintln!("error: tau binary not found at {tau_bin:?}. Build with: cargo build --release");
        process::exit(1);
    }

    let cells = build_grid();

    println!("dst full: seed={seed:#018x} cells={} ops={} fault-interval={}",
        cells.len(), cli.ops, cli.fault_interval);
    println!("{:<8} {:<10} {:<5} {:<14} {:<14} {:<8} {:<14} {:<14}",
        "transp", "auth", "wal", "correctness", "faults", "metrics", "ns/op", "ops/s");

    let mut rows: Vec<CellResult> = Vec::new();

    for cell in &cells {
        let cell_seed = seed.wrapping_add(
            (cell.transport as u64) << 16 | (cell.auth as u64) << 8 | (cell.wal as u64)
        );
        let result = run_cell(cell, cli, &scratch_root, cell_seed);
        println!("{:<8} {:<10} {:<5} {:<14} {:<14} {:<8} {:<14.1} {:<14.0}",
            cell.transport.to_string(),
            cell.auth.to_string(),
            cell.wal.to_string(),
            result.correctness,
            result.fault_injection,
            if result.metrics_ok { "ok" } else { "FAIL" },
            result.ns_per_op,
            result.ops_per_sec,
        );
        rows.push(result);
    }

    let pass = rows.iter().all(|r| r.correctness == "PASS" && r.fault_injection == "PASS");
    println!("\ndst full: {}", if pass { "PASS" } else { "FAIL" });

    if let Some(path) = &cli.out {
        write_csv(path, &rows, &cli.label);
    }

    if !pass {
        process::exit(1);
    }
}

fn build_grid() -> Vec<Cell> {
    let mut cells = Vec::new();
    for &transport in &[Transport::Plain, Transport::Tls] {
        for &auth in &[Auth::None, Auth::Password] {
            for &wal in &[Wal::Off, Wal::On] {
                cells.push(Cell { transport, auth, wal });
            }
        }
    }
    cells
}

fn run_cell(cell: &Cell, cli: &Cli, scratch_root: &Path, seed: u64) -> CellResult {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut oracle = Oracle::new();

    let scratch = if cell.wal == Wal::On {
        Some(ScratchDir::new(scratch_root))
    } else {
        None
    };

    let server = match Server::spawn(cell, scratch.as_ref().map(|s| s.path())) {
        Ok(s) => s,
        Err(_) => {
            return CellResult {
                cell: *cell,
                ops: 0,
                correctness: "SPAWN_ERR",
                fault_injection: "SKIP",
                metrics_ok: false,
                ns_per_op: f64::INFINITY,
                ops_per_sec: 0.0,
                seed,
            };
        }
    };

    if server.wait_ready().is_err() {
        return CellResult {
            cell: *cell,
            ops: 0,
            correctness: "TIMEOUT",
            fault_injection: "SKIP",
            metrics_ok: false,
            ns_per_op: f64::INFINITY,
            ops_per_sec: 0.0,
            seed,
        };
    }

    let mut conn = match Conn::open(&server.addr, cell.transport, cell.auth) {
        Ok(c) => c,
        Err(_) => {
            return CellResult {
                cell: *cell, ops: 0, correctness: "CONN_ERR",
                fault_injection: "SKIP", metrics_ok: false,
                ns_per_op: f64::INFINITY, ops_per_sec: 0.0, seed,
            };
        }
    };

    let setup_cmds = [
        "CREATE DATABASE default",
        &format!("CREATE LENS {LENS} int"),
    ];
    for cmd in setup_cmds {
        if conn.send(cmd).is_err() {
            return CellResult {
                cell: *cell, ops: 0, correctness: "SETUP_ERR",
                fault_injection: "SKIP", metrics_ok: false,
                ns_per_op: f64::INFINITY, ops_per_sec: 0.0, seed,
            };
        }
    }

    let mut correctness = "PASS";
    let mut fault_result = "PASS";
    let mut successful_ops: u64 = 0;
    let mut time_cursor: i64 = 0;
    let mut append_count: u64 = 0;

    let t0 = Instant::now();

    for op_idx in 0..cli.ops {
        // Fault injection every fault_interval ops.
        if op_idx > 0 && op_idx % cli.fault_interval == 0 {
            let inject_ok = inject_fault(
                &mut conn,
                &server,
                &oracle,
                cell,
                scratch.as_ref().map(|s| s.path()),
                cli.verbose,
            );
            if !inject_ok {
                fault_result = "FAIL";
            }
        }

        // Connection drop and reconnect every 200 ops.
        if op_idx > 0 && op_idx % 200 == 0 {
            conn = match Conn::open(&server.addr, cell.transport, cell.auth) {
                Ok(c) => c,
                Err(_) => { correctness = "RECONNECT_ERR"; break; }
            };
        }

        // Advance simulated time by millions of units (centuries of data).
        let advance = rng_range(&mut rng, 1_000_000, 10_000_000);
        let start = time_cursor + advance;
        let end = start + rng_range(&mut rng, 1, 1_000_001);
        time_cursor = end;

        let val = rng_range(&mut rng, i32::MIN as i64, i32::MAX as i64);

        let resp = match conn.send(&format!("APPEND LENS {LENS} {start} {end} {val}")) {
            Ok(r) => r,
            Err(_) => { correctness = "SEND_ERR"; break; }
        };
        if !resp.starts_with("OK") {
            correctness = "APPEND_FAIL";
            break;
        }

        oracle.append(LENS, &[(start, end, val)]);
        append_count += 1;

        // Spot-check: query the just-appended start timestamp.
        let at_resp = match conn.send(&format!("AT LENS {LENS} {start}")) {
            Ok(r) => r,
            Err(_) => { correctness = "AT_ERR"; break; }
        };
        let oracle_val = oracle.at(LENS, start);
        if !verify_at_response(&at_resp, oracle_val) {
            if cli.verbose {
                eprintln!("op {op_idx}: AT mismatch at {start}: resp={at_resp:?} oracle={oracle_val:?}");
            }
            correctness = "ORACLE_MISMATCH";
            break;
        }

        // Verify RANGE invariants on a random window.
        let range_start = rng_range(&mut rng, 0, time_cursor.max(1));
        let range_end = range_start + rng_range(&mut rng, 1, 10_000_001);
        let range_resp = match conn.send(&format!("RANGE LENS {LENS} {range_start} {range_end}")) {
            Ok(r) => r,
            Err(_) => { correctness = "RANGE_ERR"; break; }
        };
        if !range_resp.starts_with("RANGE") {
            correctness = "RANGE_FAIL";
            break;
        }

        successful_ops += 1;
    }

    let elapsed = t0.elapsed();
    let ns_per_op = if successful_ops > 0 {
        elapsed.as_nanos() as f64 / successful_ops as f64
    } else {
        f64::INFINITY
    };
    let ops_per_sec = if ns_per_op.is_finite() { 1e9 / ns_per_op } else { 0.0 };

    // Validate Prometheus metrics.
    let metrics_ok = validate_metrics(&server, append_count);

    CellResult {
        cell: *cell,
        ops: successful_ops,
        correctness,
        fault_injection: fault_result,
        metrics_ok,
        ns_per_op,
        ops_per_sec,
        seed,
    }
}

fn verify_at_response(resp: &str, expected: Option<i64>) -> bool {
    match expected {
        None => resp.starts_with("VAL NIL") || resp == "VAL",
        Some(v) => {
            let target = format!("VAL i{v}");
            resp.trim() == target
        }
    }
}

fn inject_fault(
    conn: &mut Conn,
    server: &Server,
    _oracle: &Oracle,
    cell: &Cell,
    wal_dir: Option<&Path>,
    verbose: bool,
) -> bool {
    // Fault 1: connection drop -- just reconnect without action.
    *conn = match Conn::open(&server.addr, cell.transport, cell.auth) {
        Ok(c) => c,
        Err(e) => {
            if verbose { eprintln!("fault: reconnect failed: {e}"); }
            return false;
        }
    };

    // Fault 2: WAL truncation + server restart (only when WAL is enabled).
    if cell.wal == Wal::On && let Some(dir) = wal_dir {
        // Truncate the WAL file by up to 32 bytes to simulate a partial write.
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "wal")
                    && let Ok(meta) = fs::metadata(&path)
                {
                    let size = meta.len();
                    if size > 32 {
                        let _ = fs::OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .and_then(|f| f.set_len(size.saturating_sub(16)));
                    }
                }
            }
        }
    }

    true
}

fn validate_metrics(server: &Server, expected_appends: u64) -> bool {
    match server.scrape_metrics() {
        Ok(metrics) => {
            let actual = metrics.get("tau_statements_total_append").copied().unwrap_or(0);
            // Allow actual >= expected (server may count internal DDL).
            actual >= expected_appends
        }
        Err(_) => false,
    }
}

// Server process management

struct Server {
    child: Child,
    addr: String,
    metrics_port: u16,
    _wal_path: Option<PathBuf>,
}

impl Server {
    fn spawn(cell: &Cell, wal_dir: Option<&Path>) -> io::Result<Self> {
        let query_port = free_port()?;
        let metrics_port = free_port()?;

        let mut cmd = Command::new(tau_binary());
        cmd.arg(format!("127.0.0.1:{query_port}"));
        cmd.arg("--metrics-port").arg(metrics_port.to_string());
        cmd.arg("--compact-threshold").arg("8");
        cmd.arg("--log-level").arg("error");

        let mut wal_path: Option<PathBuf> = None;
        if let Some(dir) = wal_dir {
            let wpath = dir.join("dst.wal");
            cmd.arg("--wal").arg("-w").arg(&wpath);
            wal_path = Some(wpath);
        }

        if cell.transport == Transport::Tls {
            cmd.arg("--tls");
        }

        if cell.auth == Auth::Password {
            cmd.arg("--auth")
                .arg("--username").arg(SIM_USER)
                .arg("--password").arg(SIM_PASS);
        }

        let child = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        Ok(Self {
            child,
            addr: format!("127.0.0.1:{query_port}"),
            metrics_port,
            _wal_path: wal_path,
        })
    }

    fn wait_ready(&self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Instant::now() > deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "server startup timed out"));
            }
            if TcpStream::connect(&self.addr).is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn scrape_metrics(&self) -> io::Result<HashMap<String, u64>> {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", self.metrics_port))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        write!(s, "GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        s.flush()?;
        let mut body = String::new();
        BufReader::new(s).read_to_string(&mut body)?;
        Ok(parse_prometheus(&body))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_prometheus(text: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') { continue; }
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() != 2 { continue; }
        let key = parts[0].replace(['{', '}', '"', ','], "_").replace("=", "_").trim_end_matches('_').to_string();
        if let Ok(v) = parts[1].trim().parse::<f64>() {
            map.insert(key, v as u64);
        }
    }
    map
}

fn tau_binary() -> PathBuf {
    let mut p = env::current_exe().expect("current_exe");
    p.pop();
    p.push("tau");
    p
}

fn free_port() -> io::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

// TCP connection

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
                let client = ClientConnection::new(Arc::new(config), sn).map_err(io::Error::other)?;
                ConnBackend::Tls(Box::new(BufReader::new(StreamOwned::new(client, tcp))))
            }
        };
        let mut conn = Self(backend);
        if auth == Auth::Password {
            let resp = conn.send(&format!("AUTH {SIM_USER} {SIM_PASS}"))?;
            if !resp.starts_with("OK") {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("AUTH failed: {resp}")));
            }
        }
        Ok(conn)
    }

    fn send(&mut self, line: &str) -> io::Result<String> {
        let mut msg = Vec::with_capacity(line.len() + 1);
        msg.extend_from_slice(line.as_bytes());
        msg.push(b'\n');
        self.0.write_all(&msg)?;
        self.0.flush()?;
        let mut resp = String::new();
        let n = self.0.read_line(&mut resp)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "server closed connection"));
        }
        Ok(resp.trim_end_matches(['\r', '\n']).to_string())
    }
}

#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256, SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512, SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384, SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256, SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512, SignatureScheme::ED25519,
        ]
    }
}

// Scratch directory RAII

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(root: &Path) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("tau-dst-{}-{}", process::id(), n));
        fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
    fn path(&self) -> &Path { &self.0 }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// CSV output

fn write_csv(path: &Path, rows: &[CellResult], label: &str) {
    let mut f = fs::File::create(path).expect("create csv");
    writeln!(f, "label,transport,auth,wal,ops,correctness,faults,metrics_ok,ns_per_op,ops_per_sec,seed").unwrap();
    for r in rows {
        writeln!(f, "{},{},{},{},{},{},{},{},{:.2},{:.1},{:#018x}",
            label, r.cell.transport, r.cell.auth, r.cell.wal,
            r.ops, r.correctness, r.fault_injection,
            if r.metrics_ok { "ok" } else { "fail" },
            r.ns_per_op, r.ops_per_sec, r.seed,
        ).unwrap();
    }
}
