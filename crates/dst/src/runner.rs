//! 1BRC deterministic simulation runner.
//!
//! Three backends:
//! - `Embedded`: calls the executor directly (fastest, default).
//! - `Wal`: executor backed by a real WAL on disk; also exercises WAL replay
//!   by restarting the executor as a fault-injection variant.
//! - `Tcp`: spins up an in-process TCP server and drives it over the loopback,
//!   exercising the full line-protocol and lock-routing stack.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use libharness::{OneBrcGen, Oracle, SeedTree, Tier};
use libtau::{Executor, Output, Response, Value, needs_registry_lock, parse};
use tracing::{error, info};

use crate::report::{Report, Result as RunResult};

/// Rows between fault injections (0 = no faults).
const FAULT_EVERY: u64 = 5_000;

pub enum Backend {
    Embedded,
    Wal,
    Tcp,
}

pub struct RunConfig {
    pub tier: Tier,
    pub seed: u64,
    pub fault_inject: bool,
    pub backend: Backend,
    /// Override row count; `None` means use `tier.row_count()`. Used in tests
    /// to keep TCP round-trip overhead manageable.
    pub rows_override: Option<u64>,
}

struct IngestStats {
    rows: u64,
    faults: u64,
    ingest_ms: u64,
}

pub fn run(cfg: &RunConfig) -> Report {
    match cfg.backend {
        Backend::Embedded => run_embedded(cfg),
        Backend::Wal => run_wal(cfg),
        Backend::Tcp => run_tcp(cfg),
    }
}

pub fn run_embedded(cfg: &RunConfig) -> Report {
    let tree = SeedTree::new(cfg.seed);
    let mut oracle = Oracle::new();
    let exec = Arc::new(RwLock::new(Executor::new()));

    assert_matches_ok(
        &exec_write(&exec, "CREATE DATABASE brc"),
        "CREATE DATABASE brc",
    );

    let mut datagen = OneBrcGen::new(&tree, "datagen");
    let stations: Vec<String> = datagen
        .station_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    for station in &stations {
        let stmt = format!("CREATE LENS {} int", escaped(station));
        assert_matches_ok(&exec_write(&exec, &stmt), &stmt);
    }

    info!(
        tier = cfg.tier.name(),
        total = cfg.tier.row_count(),
        seed = cfg.seed,
        "starting 1BRC embedded"
    );

    let total_started = Instant::now();
    let stats = match ingest_embedded(&exec, &mut oracle, &mut datagen, &stations, cfg) {
        Ok(s) => s,
        Err(r) => return r,
    };

    info!(
        rows = stats.rows,
        faults = stats.faults,
        ingest_ms = stats.ingest_ms,
        "ingest done, verifying"
    );

    if let Some(r) = verify_at(&exec, &oracle, &stations) {
        return r;
    }

    let query_started = Instant::now();
    if let Some(r) = verify_reduce(&exec, &oracle, &stations) {
        return r;
    }

    let query_ms = query_started.elapsed().as_millis() as u64;
    let total_ms = total_started.elapsed().as_millis() as u64;
    info!(
        rows = stats.rows,
        faults = stats.faults,
        ingest_ms = stats.ingest_ms,
        query_ms,
        total_ms,
        "PASS"
    );

    Report::success(RunResult {
        tier: cfg.tier,
        seed: cfg.seed,
        rows: stats.rows,
        faults: stats.faults,
        ingest_ms: stats.ingest_ms,
        query_ms,
    })
}

fn ingest_embedded(
    exec: &Arc<RwLock<Executor>>,
    oracle: &mut Oracle,
    datagen: &mut OneBrcGen,
    stations: &[String],
    cfg: &RunConfig,
) -> Result<IngestStats, Report> {
    let total = cfg.rows_override.unwrap_or_else(|| cfg.tier.row_count());
    let batch_size: u64 = match cfg.tier {
        Tier::Nano => 100,
        Tier::Micro => 1_000,
        _ => 10_000,
    };
    let started = Instant::now();
    let mut rows_written: u64 = 0;
    let mut faults: u64 = 0;

    while rows_written < total {
        let n = batch_size.min(total - rows_written) as usize;
        for reading in &datagen.batch(n) {
            let t = rows_written as i64;
            let stmt = format!(
                "APPEND LENS {} {} {} {}",
                escaped(&reading.station),
                t,
                t + 1,
                reading.temp_x10
            );
            let r = exec_write(exec, &stmt);
            if let Err(e) = exec_result(&r) {
                error!(stmt, error = %e, "APPEND failed");
                return Err(Report::failure(format!("APPEND failed: {e}")));
            }
            oracle.append(&reading.station, &[(t, t + 1, reading.temp_x10)]);
            rows_written += 1;
            if cfg.fault_inject && rows_written.is_multiple_of(FAULT_EVERY) {
                inject_fault_embedded(exec, oracle, stations, rows_written);
                faults += 1;
            }
        }
    }

    Ok(IngestStats {
        rows: rows_written,
        faults,
        ingest_ms: started.elapsed().as_millis() as u64,
    })
}

fn inject_fault_embedded(
    exec: &Arc<RwLock<Executor>>,
    oracle: &mut Oracle,
    stations: &[String],
    rows_written: u64,
) {
    let victim = &stations[(rows_written as usize).wrapping_rem(stations.len())];
    let _ = exec_write(exec, &format!("DROP LENS {}", escaped(victim)));
    let _ = exec_write(exec, &format!("CREATE LENS {} int", escaped(victim)));
    oracle.reset(victim);
}

fn run_wal(cfg: &RunConfig) -> Report {
    let dir = tempfile::tempdir().expect("tempdir for WAL DST");
    let wal_path = dir.path().join("dst.wal");

    let tree = SeedTree::new(cfg.seed);
    let mut oracle = Oracle::new();

    let exec = Arc::new(RwLock::new(
        Executor::with_wal(&wal_path, None).expect("executor::with_wal"),
    ));

    exec_write_ok(&exec, "CREATE DATABASE brc");

    let mut datagen = OneBrcGen::new(&tree, "datagen");
    let stations: Vec<String> = datagen
        .station_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    for station in &stations {
        exec_write_ok(&exec, &format!("CREATE LENS {} int", escaped(station)));
    }

    info!(tier = cfg.tier.name(), seed = cfg.seed, "starting 1BRC WAL");

    let total = cfg.rows_override.unwrap_or_else(|| cfg.tier.row_count());
    let batch_size: u64 = match cfg.tier {
        Tier::Nano => 100,
        Tier::Micro => 1_000,
        _ => 10_000,
    };
    let started = Instant::now();
    let mut rows_written: u64 = 0;
    let mut faults: u64 = 0;

    while rows_written < total {
        let n = batch_size.min(total - rows_written) as usize;
        for reading in &datagen.batch(n) {
            let t = rows_written as i64;
            let stmt = format!(
                "APPEND LENS {} {} {} {}",
                escaped(&reading.station),
                t,
                t + 1,
                reading.temp_x10
            );
            if let Err(e) = exec_result(&exec_write(&exec, &stmt)) {
                return Report::failure(format!("WAL APPEND failed: {e}"));
            }
            oracle.append(&reading.station, &[(t, t + 1, reading.temp_x10)]);
            rows_written += 1;

            if cfg.fault_inject && rows_written.is_multiple_of(FAULT_EVERY) {
                // Restart executor from WAL — tests replay correctness.
                match Executor::with_wal(&wal_path, None) {
                    Ok(new_exec) => *exec.write().expect("lock") = new_exec,
                    Err(e) => return Report::failure(format!("WAL reload failed: {e}")),
                }
                faults += 1;
            }
        }
    }

    let ingest_ms = started.elapsed().as_millis() as u64;
    info!(
        rows = rows_written,
        faults, ingest_ms, "WAL ingest done, verifying"
    );

    if let Some(r) = verify_at(&exec, &oracle, &stations) {
        return r;
    }

    let query_started = Instant::now();
    if let Some(r) = verify_reduce(&exec, &oracle, &stations) {
        return r;
    }

    Report::success(RunResult {
        tier: cfg.tier,
        seed: cfg.seed,
        rows: rows_written,
        faults,
        ingest_ms,
        query_ms: query_started.elapsed().as_millis() as u64,
    })
}

fn run_tcp(cfg: &RunConfig) -> Report {
    let tree = SeedTree::new(cfg.seed);
    let mut oracle = Oracle::new();

    let exec = Arc::new(RwLock::new(Executor::new()));

    // Start in-process server on an OS-assigned port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind DST server");
    let addr = listener.local_addr().expect("local_addr");
    let server_exec = exec.clone();
    std::thread::spawn(move || serve_tcp(listener, server_exec));

    let mut client = TcpClient::connect(addr).expect("connect DST client");

    send_ok(&mut client, "CREATE DATABASE brc");

    let mut datagen = OneBrcGen::new(&tree, "datagen");
    let stations: Vec<String> = datagen
        .station_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    for station in &stations {
        send_ok(
            &mut client,
            &format!("CREATE LENS {} int", escaped(station)),
        );
    }

    info!(tier = cfg.tier.name(), seed = cfg.seed, "starting 1BRC TCP");

    let total = cfg.rows_override.unwrap_or_else(|| cfg.tier.row_count());
    let batch_size: u64 = match cfg.tier {
        Tier::Nano => 100,
        Tier::Micro => 1_000,
        _ => 10_000,
    };
    let started = Instant::now();
    let mut rows_written: u64 = 0;
    let mut faults: u64 = 0;

    while rows_written < total {
        let n = batch_size.min(total - rows_written) as usize;
        for reading in &datagen.batch(n) {
            let t = rows_written as i64;
            let stmt = format!(
                "APPEND LENS {} {} {} {}",
                escaped(&reading.station),
                t,
                t + 1,
                reading.temp_x10
            );
            let resp = client.send(&stmt);
            if resp.starts_with("ERR") {
                return Report::failure(format!("TCP APPEND failed: {resp}"));
            }
            oracle.append(&reading.station, &[(t, t + 1, reading.temp_x10)]);
            rows_written += 1;

            if cfg.fault_inject && rows_written.is_multiple_of(FAULT_EVERY) {
                // Drop + recreate victim lens, reconnect client.
                let victim = &stations[(rows_written as usize).wrapping_rem(stations.len())];
                let _ = client.send(&format!("DROP LENS {}", escaped(victim)));
                let _ = client.send(&format!("CREATE LENS {} int", escaped(victim)));
                oracle.reset(victim);
                client = TcpClient::connect(addr).expect("TCP reconnect");
                faults += 1;
            }
        }
    }

    let ingest_ms = started.elapsed().as_millis() as u64;
    info!(
        rows = rows_written,
        faults, ingest_ms, "TCP ingest done, verifying"
    );

    // Verify via TCP queries.
    let n_segs = oracle.total_segments() as i64 / stations.len() as i64;
    if n_segs > 0 {
        for station in stations.iter().take(10) {
            let resp = client.send(&format!(
                "REDUCE LENS {} 0 {n_segs} USING min",
                escaped(station)
            ));
            let engine = parse_val_response(&resp);
            let expected = oracle.reduce_min(station, 0, n_segs);
            if expected != engine {
                return Report::failure(format!(
                    "TCP REDUCE mismatch on {station}: oracle={expected:?} engine={engine:?}"
                ));
            }
        }
    }

    Report::success(RunResult {
        tier: cfg.tier,
        seed: cfg.seed,
        rows: rows_written,
        faults,
        ingest_ms,
        query_ms: 0,
    })
}

fn serve_tcp(listener: TcpListener, exec: Arc<RwLock<Executor>>) {
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        let exec = exec.clone();
        std::thread::spawn(move || handle_tcp_conn(stream, exec));
    }
}

fn handle_tcp_conn(stream: TcpStream, exec: Arc<RwLock<Executor>>) {
    stream.set_nodelay(true).ok();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        let trimmed = line.trim().to_string();
        if !trimmed.is_empty() {
            let resp = dispatch_tcp_line(&trimmed, &exec);
            let _ = writeln!(reader.get_mut(), "{resp}");
            let _ = reader.get_mut().flush();
        }
        line.clear();
    }
}

fn dispatch_tcp_line(query: &str, exec: &Arc<RwLock<Executor>>) -> String {
    let stmt = match parse(query) {
        Ok((rest, s)) if rest.trim().is_empty() => s,
        Ok((rest, _)) => return format!("ERR trailing input: {rest:?}"),
        Err(e) => return format!("ERR parse: {e}"),
    };
    let needs_write = !stmt.is_read_only();
    let needs_exclusive = needs_write
        && (needs_registry_lock(&stmt) || exec.read().expect("lock").is_in_transaction());
    let result = match (stmt.is_read_only(), needs_exclusive) {
        (true, _) => exec.read().expect("lock").exec_read(&stmt),
        (false, true) => exec.write().expect("lock").exec(&stmt),
        (false, false) => exec.read().expect("lock").exec_db_write(&stmt),
    };
    Response::from_result(&result).to_string()
}

struct TcpClient {
    reader: BufReader<TcpStream>,
}

impl TcpClient {
    fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    fn send(&mut self, stmt: &str) -> String {
        let w = self.reader.get_mut();
        let _ = writeln!(w, "{stmt}");
        let _ = w.flush();
        let mut resp = String::new();
        self.reader.read_line(&mut resp).unwrap_or(0);
        resp.trim_end_matches(['\r', '\n']).to_string()
    }
}

fn send_ok(client: &mut TcpClient, stmt: &str) {
    let r = client.send(stmt);
    assert!(r == "OK", "expected OK for {stmt:?}, got {r:?}");
}

fn verify_at(exec: &Arc<RwLock<Executor>>, oracle: &Oracle, stations: &[String]) -> Option<Report> {
    let mut mismatches = 0u64;
    for station in stations {
        for t in oracle.sample_midpoints(station, 5) {
            let r = exec_read(exec, &format!("AT LENS {} {t}", escaped(station)));
            let expected = oracle.at(station, t);
            let got = parse_at_output(&r);
            if expected != got {
                mismatches += 1;
                if mismatches <= 3 {
                    error!(station, t, ?expected, ?got, "AT mismatch");
                }
            }
        }
    }
    (mismatches > 0).then(|| Report::failure(format!("{mismatches} AT mismatches detected")))
}

fn verify_reduce(
    exec: &Arc<RwLock<Executor>>,
    oracle: &Oracle,
    stations: &[String],
) -> Option<Report> {
    let n = oracle.total_segments() as i64 / stations.len() as i64;
    if n == 0 {
        return None;
    }
    for station in stations.iter().take(10) {
        let r = exec_read(
            exec,
            &format!("REDUCE LENS {} 0 {n} USING min", escaped(station)),
        );
        let oracle_min = oracle.reduce_min(station, 0, n);
        let engine_min = parse_at_output(&r);
        if oracle_min != engine_min {
            return Some(Report::failure(format!(
                "REDUCE MIN mismatch on {station}: oracle={oracle_min:?} engine={engine_min:?}"
            )));
        }
    }
    None
}

fn escaped(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn exec_write(exec: &Arc<RwLock<Executor>>, stmt: &str) -> Output {
    let (_, s) = parse(stmt).expect("parse");
    exec.write()
        .expect("executor lock poisoned")
        .exec(&s)
        .unwrap_or(Output::Empty)
}

fn exec_write_ok(exec: &Arc<RwLock<Executor>>, stmt: &str) {
    assert_matches_ok(&exec_write(exec, stmt), stmt);
}

fn exec_read(exec: &Arc<RwLock<Executor>>, stmt: &str) -> Output {
    let (_, s) = parse(stmt).expect("parse");
    exec.read()
        .expect("executor lock poisoned")
        .exec_read(&s)
        .unwrap_or(Output::Empty)
}

fn exec_result(o: &Output) -> std::result::Result<(), String> {
    match o {
        Output::Empty => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

fn assert_matches_ok(o: &Output, ctx: &str) {
    if !matches!(o, Output::Empty) {
        panic!("expected OK for {ctx:?}, got {o:?}");
    }
}

fn parse_at_output(o: &Output) -> Option<i64> {
    match o {
        Output::Value(Some(Value::Int(i))) => Some(*i),
        _ => None,
    }
}

fn parse_val_response(resp: &str) -> Option<i64> {
    match Response::parse(resp) {
        Ok(Response::Val(Some(Value::Int(i)))) => Some(i),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use pretty_assertions::assert_eq;

    fn cfg(backend: Backend, faults: bool, rows: Option<u64>) -> RunConfig {
        RunConfig {
            tier: Tier::Nano,
            seed: 0xdead_beef,
            fault_inject: faults,
            backend,
            rows_override: rows,
        }
    }

    #[test]
    fn embedded_nano_passes() {
        let r = run(&cfg(Backend::Embedded, false, None));
        assert!(r.ok, "embedded nano failed: {:?}", r.error);
    }

    #[test]
    fn embedded_nano_with_faults_passes() {
        let r = run(&cfg(Backend::Embedded, true, None));
        assert!(r.ok, "embedded+faults nano failed: {:?}", r.error);
        assert!(r.result.unwrap().faults > 0);
    }

    #[test]
    fn wal_nano_passes() {
        let r = run(&cfg(Backend::Wal, false, None));
        assert!(r.ok, "WAL nano failed: {:?}", r.error);
    }

    #[test]
    fn wal_nano_with_faults_exercises_replay() {
        let r = run(&cfg(Backend::Wal, true, None));
        assert!(r.ok, "WAL+faults nano failed: {:?}", r.error);
        assert!(r.result.unwrap().faults > 0);
    }

    #[test]
    fn tcp_server_handles_append_at_range() {
        let exec = Arc::new(RwLock::new(Executor::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = exec.clone();
        std::thread::spawn(move || serve_tcp(listener, srv));
        let mut c = TcpClient::connect(addr).unwrap();
        assert_eq!(c.send("CREATE DATABASE t"), "OK");
        assert_eq!(c.send("CREATE LENS x int"), "OK");
        assert_eq!(c.send("APPEND LENS x 0 10 42"), "OK");
        assert_eq!(c.send("AT LENS x 5"), "VAL i42");
        assert_eq!(c.send("AT LENS x 15"), "VAL NIL");
        assert_eq!(c.send("RANGE LENS x 0 10"), "RANGE 1; 0:10:i42");
        assert_eq!(c.send("REDUCE LENS x 0 10 USING min"), "VAL i42");
    }

    #[test]
    fn tcp_server_handles_derive_lens() {
        let exec = Arc::new(RwLock::new(Executor::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = exec.clone();
        std::thread::spawn(move || serve_tcp(listener, srv));
        let mut c = TcpClient::connect(addr).unwrap();
        c.send("CREATE DATABASE t");
        c.send("CREATE LENS base int");
        c.send("APPEND LENS base 0 100 10");
        c.send("DERIVE LENS doubled AS base * 2");
        assert_eq!(c.send("AT LENS doubled 50"), "VAL i20");
    }

    #[test]
    fn tcp_server_returns_err_for_bad_statements() {
        let exec = Arc::new(RwLock::new(Executor::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = exec.clone();
        std::thread::spawn(move || serve_tcp(listener, srv));
        let mut c = TcpClient::connect(addr).unwrap();
        let resp = c.send("COMPLETELY INVALID STATEMENT");
        assert!(resp.starts_with("ERR"), "got: {resp}");
    }

    #[test]
    fn tcp_server_reconnect_after_drop() {
        let exec = Arc::new(RwLock::new(Executor::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = exec.clone();
        std::thread::spawn(move || serve_tcp(listener, srv));
        {
            let mut c = TcpClient::connect(addr).unwrap();
            c.send("CREATE DATABASE persist");
            c.send("CREATE LENS y int");
            c.send("APPEND LENS y 0 5 77");
        }
        let mut c2 = TcpClient::connect(addr).unwrap();
        c2.send("USE DATABASE persist");
        assert_eq!(c2.send("AT LENS y 2"), "VAL i77");
    }

    #[test]
    fn escaped_replaces_non_alphanumeric() {
        assert_eq!(escaped("Addis Ababa"), "Addis_Ababa");
        assert_eq!(escaped("São Paulo"), "S_o_Paulo");
        assert_eq!(escaped("simple"), "simple");
    }

    #[test]
    fn dispatch_tcp_line_handles_unknown_stmt() {
        let exec = Arc::new(RwLock::new(Executor::new()));
        let r = dispatch_tcp_line("GARBAGE STATEMENT", &exec);
        assert!(r.starts_with("ERR"));
    }

    #[test]
    fn report_success_is_ok() {
        use crate::report::Result as RunResult;
        let r = Report::success(RunResult {
            tier: Tier::Nano,
            seed: 0,
            rows: 1,
            faults: 0,
            ingest_ms: 1,
            query_ms: 1,
        });
        assert!(r.ok);
        assert!(r.error.is_none());
    }

    #[test]
    fn report_failure_is_not_ok() {
        let r = Report::failure("oops".to_string());
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("oops"));
    }

    #[hegel::test]
    fn escaped_alphanumeric_is_identity(tc: TestCase) {
        let s = tc.draw(gs::from_regex("[a-zA-Z0-9]+").fullmatch(true));
        assert_eq!(escaped(&s), s);
    }

    #[hegel::test]
    fn escaped_maps_non_alphanumeric_to_underscore(tc: TestCase) {
        let s = tc.draw(gs::text().max_size(64));
        let result = escaped(&s);
        assert_eq!(s.chars().count(), result.chars().count());
        for (orig, esc) in s.chars().zip(result.chars()) {
            if orig.is_ascii_alphanumeric() {
                assert_eq!(esc, orig);
            } else {
                assert_eq!(esc, '_');
            }
        }
    }

    #[test]
    fn exec_result_ok_for_empty_output() {
        assert!(exec_result(&Output::Empty).is_ok());
    }

    #[test]
    fn exec_result_err_for_value_output() {
        assert!(exec_result(&Output::Value(Some(Value::Int(0)))).is_err());
    }

    #[test]
    fn parse_at_output_extracts_positive_int() {
        assert_eq!(
            parse_at_output(&Output::Value(Some(Value::Int(42)))),
            Some(42)
        );
    }

    #[test]
    fn parse_at_output_extracts_negative_int() {
        assert_eq!(
            parse_at_output(&Output::Value(Some(Value::Int(-7)))),
            Some(-7)
        );
    }

    #[test]
    fn parse_at_output_returns_none_for_nil() {
        assert_eq!(parse_at_output(&Output::Value(None)), None);
    }

    #[test]
    fn parse_at_output_returns_none_for_empty() {
        assert_eq!(parse_at_output(&Output::Empty), None);
    }

    #[hegel::test]
    fn parse_val_response_roundtrips_ints(tc: TestCase) {
        let n = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        let resp = format!("VAL i{n}");
        assert_eq!(parse_val_response(&resp), Some(n));
    }

    #[test]
    fn parse_val_response_returns_none_for_err_response() {
        assert_eq!(parse_val_response("ERR no such lens"), None);
    }

    #[test]
    fn parse_val_response_returns_none_for_nil() {
        assert_eq!(parse_val_response("VAL NIL"), None);
    }

    #[test]
    fn parse_val_response_returns_none_for_ok() {
        assert_eq!(parse_val_response("OK"), None);
    }

    #[test]
    fn tcp_nano_passes() {
        let r = run(&cfg(Backend::Tcp, false, None));
        assert!(r.ok, "TCP nano failed: {:?}", r.error);
    }

    #[test]
    fn tcp_nano_with_faults_passes() {
        let r = run(&cfg(Backend::Tcp, true, Some(6_000)));
        assert!(r.ok, "TCP+faults nano failed: {:?}", r.error);
        assert!(r.result.unwrap().faults > 0);
    }
}
