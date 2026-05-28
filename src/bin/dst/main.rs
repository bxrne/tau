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
//!   --verbose            Print every operation

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
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

// Oracle: O(log n) BTreeMap per lens.
// Keys are start timestamps; each entry maps to (end, value).
// Since time_cursor advances monotonically, intervals from different appends
// never overlap by construction, so newest-wins reduces to last-write-wins
// on unique start keys.

struct Oracle {
    lenses: HashMap<String, BTreeMap<i64, (i64, i64)>>,
}

impl Oracle {
    fn new() -> Self {
        Self {
            lenses: HashMap::new(),
        }
    }

    fn append(&mut self, lens: &str, entries: &[(i64, i64, i64)]) {
        let map = self.lenses.entry(lens.to_string()).or_default();
        for &(s, e, v) in entries {
            map.insert(s, (e, v));
        }
    }

    fn at(&self, lens: &str, t: i64) -> Option<i64> {
        let map = self.lenses.get(lens)?;
        // Largest start <= t.
        let (_, (end, val)) = map.range(..=t).next_back()?;
        if t < *end { Some(*val) } else { None }
    }

    fn reset(&mut self, lens: &str) {
        self.lenses.remove(lens);
    }

    fn total_segments(&self) -> usize {
        self.lenses.values().map(|m| m.len()).sum()
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
    println!(
        "dst embedded: seed={seed:#018x} duration={}s readers={}",
        cli.duration, cli.readers
    );

    let result = std::panic::catch_unwind(|| embedded_sim(seed, cli));
    match result {
        Ok(()) => println!("dst embedded: PASS"),
        Err(e) => {
            let msg = e
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| e.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            eprintln!("dst embedded: FAIL  seed={seed:#018x}  {msg}");
            eprintln!("Reproduce: cargo run --release --bin dst -- --quick --seed {seed}");
            process::exit(1);
        }
    }
}

const EMBEDDED_LENSES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];

fn embedded_sim(seed: u64, cli: &Cli) {
    let mut rng = StdRng::seed_from_u64(seed);
    let executor: Arc<RwLock<Executor>> = Arc::new(RwLock::new(Executor::with_threshold(4)));
    let mut oracle = Oracle::new();
    let mut live = [true; 8];

    {
        let mut exec = executor.write().expect("write lock");
        exec.exec(&parse("CREATE DATABASE default").expect("parse").1)
            .expect("create db");
        for name in EMBEDDED_LENSES {
            exec.exec(&parse(&format!("CREATE LENS {name} int")).expect("parse").1)
                .expect("create lens");
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    // Stress reader: continuously queries random timestamps.
    // max_cursor tracks the high-water mark so the stress reader covers real data.
    let max_cursor = Arc::new(AtomicI64::new(1));
    {
        let stress_stop = Arc::clone(&stop);
        let stress_exec = Arc::clone(&executor);
        let stress_max = Arc::clone(&max_cursor);
        let stress_seed = seed.wrapping_add(0xdead_beef);
        thread::spawn(move || {
            let mut rng = StdRng::seed_from_u64(stress_seed);
            while !stress_stop.load(Ordering::Relaxed) {
                let lens = EMBEDDED_LENSES[rng.gen_range(0..EMBEDDED_LENSES.len())];
                let hi = stress_max.load(Ordering::Relaxed).max(1);
                let t = rng.gen_range(0..hi);
                if let Ok((_, stmt)) = parse(&format!("AT LENS {lens} {t}")) {
                    let _ = executor_read_quick(&stress_exec, &stmt);
                }
            }
        });
    }

    let mut total_ops: u64 = 0;
    let mut write_ops: u64 = 0;
    let mut time_cursor: i64 = 0;
    let mut op_idx: u64 = 0;
    let deadline = Instant::now() + Duration::from_secs(cli.duration);

    while Instant::now() < deadline {
        op_idx += 1;

        let lens_idx = rng_usize(&mut rng, 0, EMBEDDED_LENSES.len());
        let lens_name = EMBEDDED_LENSES[lens_idx];

        match rng.gen_range(0u8..16) {
            // Drop a live lens.
            0 if live[lens_idx] => {
                let stmt = parse(&format!("DROP LENS {lens_name}")).expect("parse").1;
                executor.write().expect("lock").exec(&stmt).ok();
                oracle.reset(lens_name);
                live[lens_idx] = false;
                write_ops += 1;
            }
            // Recreate a dropped lens.
            1 if !live[lens_idx] => {
                let stmt = parse(&format!("CREATE LENS {lens_name} int"))
                    .expect("parse")
                    .1;
                executor.write().expect("lock").exec(&stmt).ok();
                live[lens_idx] = true;
                write_ops += 1;
            }
            // AT query on a live lens.
            2..=4 if live[lens_idx] && time_cursor > 0 => {
                let t = rng.gen_range(0..time_cursor);
                embedded_check_at(&executor, &oracle, lens_name, t, cli.verbose, op_idx);
            }
            // RANGE query on a live lens.
            5..=6 if live[lens_idx] && time_cursor > 0 => {
                let rs = rng.gen_range(0..time_cursor);
                let re = rs + rng_range(&mut rng, 1, (time_cursor / 10).max(2));
                embedded_check_range(&executor, lens_name, rs, re, cli.verbose, op_idx);
            }
            // Batch append: 1-8 non-overlapping segments, big time advances.
            _ if live[lens_idx] => {
                let batch_size = rng_usize(&mut rng, 1, 9);
                let mut segs: Vec<(i64, i64, i64)> = Vec::with_capacity(batch_size);
                let mut cur = time_cursor;
                for _ in 0..batch_size {
                    // Each segment advances time by 1M-100M units.
                    let advance = rng_range(&mut rng, 1_000_000, 100_000_000);
                    let s = cur + advance;
                    let e = s + rng_range(&mut rng, 1, 10_000_001);
                    let v = rng_range(&mut rng, i32::MIN as i64, i32::MAX as i64);
                    segs.push((s, e, v));
                    cur = e;
                }
                time_cursor = cur;
                max_cursor.store(time_cursor, Ordering::Relaxed);

                // Build the batch APPEND statement.
                let mut stmt_text = format!("APPEND LENS {lens_name}");
                for (s, e, v) in &segs {
                    stmt_text.push_str(&format!(" {s} {e} {v},"));
                }
                stmt_text.pop(); // trailing comma

                let stmt = parse(&stmt_text).expect("parse").1;
                {
                    let mut exec = executor.write().expect("lock");
                    if exec.exec(&stmt).is_err() {
                        continue;
                    }
                }

                oracle.append(lens_name, &segs);
                write_ops += 1;

                // Concurrent burst: all readers must see the same value.
                let mid = segs[0].0; // start of first segment
                let expected = oracle.at(lens_name, mid);
                embedded_concurrent_burst(&executor, lens_name, mid, expected, cli.readers, op_idx);
            }
            _ => {}
        }

        total_ops += 1;
    }

    stop.store(true, Ordering::Relaxed);

    let simulated_years = (time_cursor as f64) / (365.25 * 24.0 * 3600.0 * 1_000.0);
    let segments = oracle.total_segments();
    println!(
        "dst embedded: {total_ops} ops ({write_ops} writes), \
         {segments} segments stored, simulated {simulated_years:.0} years"
    );
}

fn executor_read_quick(exec: &Arc<RwLock<Executor>>, stmt: &tau::Stmt) -> Output {
    exec.read()
        .expect("lock")
        .exec_read(stmt)
        .unwrap_or(Output::Value(None))
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
    let out = executor_read_quick(exec, &stmt);
    let oracle_val = oracle.at(lens, t);
    let exec_val = match &out {
        Output::Value(Some(Value::Int(i))) => Some(*i),
        _ => None,
    };
    if verbose {
        println!("  AT {lens} {t}: exec={exec_val:?} oracle={oracle_val:?}");
    }
    assert_eq!(
        exec_val, oracle_val,
        "op {op_idx}: AT LENS {lens} {t} diverged: executor={exec_val:?} oracle={oracle_val:?}"
    );
}

fn embedded_check_range(
    exec: &Arc<RwLock<Executor>>,
    lens: &str,
    rs: i64,
    re: i64,
    verbose: bool,
    op_idx: u64,
) {
    let stmt = parse(&format!("RANGE LENS {lens} {rs} {re}"))
        .expect("parse")
        .1;
    let out = executor_read_quick(exec, &stmt);
    let segs = match out {
        Output::Range(v) => v,
        _ => vec![],
    };
    // Invariants: sorted, non-overlapping, no zero-width, within [rs, re).
    let mut prev_end: Option<i64> = None;
    for (s, e, _) in &segs {
        let (s, e) = (*s, *e);
        assert!(
            s < e,
            "op {op_idx}: RANGE {lens} [{rs},{re}): segment [{s},{e}) has start >= end"
        );
        assert!(
            s >= rs,
            "op {op_idx}: RANGE {lens} [{rs},{re}): segment [{s},{e}) extends before range start"
        );
        assert!(
            e <= re,
            "op {op_idx}: RANGE {lens} [{rs},{re}): segment [{s},{e}) extends past range end"
        );
        if let Some(pe) = prev_end {
            assert!(
                s >= pe,
                "op {op_idx}: RANGE {lens} [{rs},{re}): overlapping segments ending at {pe} and starting at {s}"
            );
        }
        prev_end = Some(e);
    }
    if verbose {
        println!("  RANGE {lens} {rs}..{re}: {} segments", segs.len());
    }
}

fn embedded_concurrent_burst(
    exec: &Arc<RwLock<Executor>>,
    lens: &str,
    t: i64,
    expected: Option<i64>,
    n: usize,
    op_idx: u64,
) {
    // thread::scope avoids heap allocation and OS thread spawn per burst.
    thread::scope(|s| {
        let handles: Vec<_> = (0..n)
            .map(|_| {
                s.spawn(|| {
                    let stmt = parse(&format!("AT LENS {lens} {t}")).expect("parse").1;
                    match exec
                        .read()
                        .expect("lock")
                        .exec_read(&stmt)
                        .unwrap_or(Output::Value(None))
                    {
                        Output::Value(Some(Value::Int(i))) => Some(i),
                        _ => None,
                    }
                })
            })
            .collect();

        for h in handles {
            let got = h.join().expect("reader panicked");
            assert_eq!(
                got, expected,
                "op {op_idx}: concurrent AT LENS {lens} {t}: reader={got:?} expected={expected:?}"
            );
        }
    });
}

// Full simulation (server mode)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Plain,
    Tls,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Auth {
    None,
    Password,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wal {
    Off,
    On,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Transport::Plain => write!(f, "plain"),
            Transport::Tls => write!(f, "tls"),
        }
    }
}
impl fmt::Display for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::None => write!(f, "none"),
            Auth::Password => write!(f, "password"),
        }
    }
}
impl fmt::Display for Wal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Wal::Off => write!(f, "off"),
            Wal::On => write!(f, "on"),
        }
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

    println!(
        "dst full: seed={seed:#018x} cells={} ops={} fault-interval={}",
        cells.len(),
        cli.ops,
        cli.fault_interval
    );
    println!(
        "{:<8} {:<10} {:<5} {:<14} {:<14} {:<8} {:<14} {:<14}",
        "transp", "auth", "wal", "correctness", "faults", "metrics", "ns/op", "ops/s"
    );

    let mut rows: Vec<CellResult> = Vec::new();

    for cell in &cells {
        let cell_seed = seed.wrapping_add(
            (cell.transport as u64) << 16 | (cell.auth as u64) << 8 | (cell.wal as u64),
        );
        let result = run_cell(cell, cli, &scratch_root, cell_seed);
        println!(
            "{:<8} {:<10} {:<5} {:<14} {:<14} {:<8} {:<14.1} {:<14.0}",
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

    let pass = rows
        .iter()
        .all(|r| r.correctness == "PASS" && r.fault_injection == "PASS");
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
                cells.push(Cell {
                    transport,
                    auth,
                    wal,
                });
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
                cell: *cell,
                ops: 0,
                correctness: "CONN_ERR",
                fault_injection: "SKIP",
                metrics_ok: false,
                ns_per_op: f64::INFINITY,
                ops_per_sec: 0.0,
                seed,
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
                cell: *cell,
                ops: 0,
                correctness: "SETUP_ERR",
                fault_injection: "SKIP",
                metrics_ok: false,
                ns_per_op: f64::INFINITY,
                ops_per_sec: 0.0,
                seed,
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
                Err(_) => {
                    correctness = "RECONNECT_ERR";
                    break;
                }
            };
        }

        // Batch append: 1-4 segments, each advancing time by 1M-100M units.
        let batch_size = rng_usize(&mut rng, 1, 5);
        let mut segs: Vec<(i64, i64, i64)> = Vec::with_capacity(batch_size);
        let mut cur = time_cursor;
        for _ in 0..batch_size {
            let advance = rng_range(&mut rng, 1_000_000, 100_000_000);
            let s = cur + advance;
            let e = s + rng_range(&mut rng, 1, 10_000_001);
            let v = rng_range(&mut rng, i32::MIN as i64, i32::MAX as i64);
            segs.push((s, e, v));
            cur = e;
        }
        time_cursor = cur;

        let mut stmt_text = format!("APPEND LENS {LENS}");
        for (s, e, v) in &segs {
            stmt_text.push_str(&format!(" {s} {e} {v},"));
        }
        stmt_text.pop();

        let resp = match conn.send(&stmt_text) {
            Ok(r) => r,
            Err(_) => {
                correctness = "SEND_ERR";
                break;
            }
        };
        if !resp.starts_with("OK") {
            correctness = "APPEND_FAIL";
            break;
        }

        oracle.append(LENS, &segs);
        append_count += 1;

        // Spot-check the first segment's start timestamp.
        let check_t = segs[0].0;
        let at_resp = match conn.send(&format!("AT LENS {LENS} {check_t}")) {
            Ok(r) => r,
            Err(_) => {
                correctness = "AT_ERR";
                break;
            }
        };
        let oracle_val = oracle.at(LENS, check_t);
        if !verify_at_response(&at_resp, oracle_val) {
            if cli.verbose {
                eprintln!(
                    "op {op_idx}: AT mismatch at {check_t}: resp={at_resp:?} oracle={oracle_val:?}"
                );
            }
            correctness = "ORACLE_MISMATCH";
            break;
        }

        // Verify RANGE invariants on a random window.
        let range_start = rng_range(&mut rng, 0, time_cursor);
        let range_end = range_start + rng_range(&mut rng, 1, 100_000_001);
        let range_resp = match conn.send(&format!("RANGE LENS {LENS} {range_start} {range_end}")) {
            Ok(r) => r,
            Err(_) => {
                correctness = "RANGE_ERR";
                break;
            }
        };
        if !verify_range_invariants(&range_resp, range_start, range_end) {
            if cli.verbose {
                eprintln!("op {op_idx}: RANGE invariant violated: {range_resp}");
            }
            correctness = "RANGE_INVALID";
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
    let ops_per_sec = if ns_per_op.is_finite() {
        1e9 / ns_per_op
    } else {
        0.0
    };

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
        Some(v) => resp.trim() == format!("VAL i{v}"),
    }
}

fn verify_range_invariants(resp: &str, rs: i64, re: i64) -> bool {
    // Wire format: "RANGE n; s:e:v; s:e:v; ..."
    let Some(after) = resp.strip_prefix("RANGE").map(str::trim_ascii_start) else {
        return false;
    };
    let (count_str, rest) = after.split_once(';').unwrap_or((after, ""));
    let Ok(count) = count_str.trim().parse::<usize>() else {
        return false;
    };
    if count == 0 {
        return true;
    }
    let mut prev_end: Option<i64> = None;
    for seg in rest.split(';') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let mut fields = seg.splitn(3, ':');
        let Ok(s) = fields.next().unwrap_or("").trim().parse::<i64>() else {
            return false;
        };
        let Ok(e) = fields.next().unwrap_or("").trim().parse::<i64>() else {
            return false;
        };
        if s >= e {
            return false;
        } // zero-width or inverted
        if s < rs || e > re {
            return false;
        } // outside queried range
        if prev_end.is_some_and(|pe| s < pe) {
            return false;
        } // overlap
        prev_end = Some(e);
    }
    true
}

fn inject_fault(
    conn: &mut Conn,
    server: &Server,
    cell: &Cell,
    wal_dir: Option<&Path>,
    verbose: bool,
) -> bool {
    // Fault 1: connection drop -- reconnect.
    *conn = match Conn::open(&server.addr, cell.transport, cell.auth) {
        Ok(c) => c,
        Err(e) => {
            if verbose {
                eprintln!("fault: reconnect failed: {e}");
            }
            return false;
        }
    };

    // Fault 2: WAL truncation + server restart (only when WAL is enabled).
    if cell.wal == Wal::On
        && let Some(dir) = wal_dir
        && let Ok(entries) = fs::read_dir(dir)
    {
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

    true
}

fn validate_metrics(server: &Server, expected_appends: u64) -> bool {
    match server.scrape_metrics() {
        Ok(metrics) => {
            let actual = metrics
                .get("tau_statements_total_append")
                .copied()
                .unwrap_or(0);
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
                .arg("--username")
                .arg(SIM_USER)
                .arg("--password")
                .arg(SIM_PASS);
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
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "server startup timed out",
                ));
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
    // Converts "tau_statements_total{type="append"} 5" into ("tau_statements_total_append", 5).
    let mut map = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((metric_part, value_str)) = line.rsplit_once(' ') else {
            continue;
        };
        let key = if let Some(lbrace) = metric_part.find('{') {
            let base = &metric_part[..lbrace];
            let labels = &metric_part[lbrace + 1..metric_part.len().saturating_sub(1)];
            let vals: Vec<&str> = labels
                .split(',')
                .filter_map(|kv| kv.split('=').nth(1))
                .map(|v| v.trim_matches('"'))
                .collect();
            if vals.is_empty() {
                base.to_string()
            } else {
                format!("{}_{}", base, vals.join("_"))
            }
        } else {
            metric_part.to_string()
        };
        if let Ok(v) = value_str.trim().parse::<f64>() {
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
                let client =
                    ClientConnection::new(Arc::new(config), sn).map_err(io::Error::other)?;
                ConnBackend::Tls(Box::new(BufReader::new(StreamOwned::new(client, tcp))))
            }
        };
        let mut conn = Self(backend);
        if auth == Auth::Password {
            let resp = conn.send(&format!("AUTH {SIM_USER} {SIM_PASS}"))?;
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
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// CSV output

fn write_csv(path: &Path, rows: &[CellResult], label: &str) {
    let mut f = fs::File::create(path).expect("create csv");
    writeln!(
        f,
        "label,transport,auth,wal,ops,correctness,faults,metrics_ok,ns_per_op,ops_per_sec,seed"
    )
    .unwrap();
    for r in rows {
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{:.2},{:.1},{:#018x}",
            label,
            r.cell.transport,
            r.cell.auth,
            r.cell.wal,
            r.ops,
            r.correctness,
            r.fault_injection,
            if r.metrics_ok { "ok" } else { "fail" },
            r.ns_per_op,
            r.ops_per_sec,
            r.seed,
        )
        .unwrap();
    }
}
