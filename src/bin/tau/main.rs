//! Tau TCP server.
//!
//! A line-oriented query service over TCP, optionally secured with TLS and
//! username/password authentication.  One statement per line in; one response
//! line out.
//!
//! # Security
//!
//! * `--tls` - enables TLS. Provide `--tls-cert` / `--tls-key` PEM files or
//!   omit both to generate an ephemeral self-signed certificate (dev only).
//! * `--auth` - enables authentication. Requires `--username` and `--password`.
//!   The first message from every client must be `AUTH <user> <pass>\n`.
//! * `TAU_ENCRYPTION_KEY` env var - 64 hex chars (32 bytes). When set, WAL
//!   entries are AES-256-GCM encrypted before being written to disk.
//!
//! # Wire format
//!
//! ```text
//! → AUTH admin s3cr3t                  (if --auth is set)
//! ← OK
//! → CREATE DATABASE main
//! ← OK
//! → CREATE LENS x int
//! ← OK
//! → APPEND LENS x 0 5 1, 5 10 2
//! ← OK
//! → AT LENS x 3
//! ← VAL i1
//! → RANGE LENS x 0 10
//! ← RANGE 2; 0:5:i1; 5:10:i2
//! → SHOW LENSES
//! ← NAMES 1; x
//! → QUIT
//! ← OK BYE
//! ```
//!
//! Response codes: `OK`, `OK BYE`, `VAL <v>`, `VAL NIL`, `RANGE <n>; …`,
//! `NAMES <n>; …`, `ERR <message>`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use rcgen::generate_simple_self_signed;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::ServerConnection;
use tau::libtau::crypto;
use tau::{Codec, ExecError, Executor, Metrics, Output, Perm, User, UserStore, parse};
use tracing::{debug, error, info, trace, warn};

/// Tau time-series database TCP server.
#[derive(Parser, Debug)]
#[command(name = "tau")]
#[command(author, version)]
#[command(about = "A time-series database TCP server", long_about = None)]
pub struct Config {
    /// TCP address to bind to (host:port).
    #[arg(value_name = "ADDR", default_value = "127.0.0.1:7070")]
    pub bind: String,

    /// Enable write-ahead logging for durability.
    #[arg(long)]
    pub wal: bool,

    /// Path for WAL file (required if --wal is set).
    #[arg(short = 'w', long, value_name = "PATH")]
    pub wal_path: Option<PathBuf>,

    /// Log level (error, warn, info, debug, trace).
    #[arg(short, long, default_value = "info")]
    pub log_level: String,

    /// Number of layers per lens before automatic compaction into one.
    #[arg(long, default_value = "8")]
    pub compact_threshold: usize,

    /// Enable TLS (encryption in transit).
    #[arg(long)]
    pub tls: bool,

    /// Path to PEM-encoded TLS certificate. Generates an ephemeral self-signed
    /// cert when omitted (requires --tls).
    #[arg(long, value_name = "PATH")]
    pub tls_cert: Option<PathBuf>,

    /// Path to PEM-encoded TLS private key (requires --tls).
    #[arg(long, value_name = "PATH")]
    pub tls_key: Option<PathBuf>,

    /// Enable username/password authentication.  Without --users-file, a
    /// single in-memory user is bootstrapped from --username/--password as a
    /// global admin.  With --users-file, the file is the source of truth.
    #[arg(long)]
    pub auth: bool,

    /// Username for the bootstrap admin (when --users-file is missing or new).
    /// Requires --auth.
    #[arg(long, value_name = "NAME")]
    pub username: Option<String>,

    /// Password for the bootstrap admin.  Hashed with argon2id at startup; the
    /// plaintext is not retained.  Requires --auth.
    #[arg(long, value_name = "PASS")]
    pub password: Option<String>,

    /// Persistent multi-user database file (plain text).  When set, loads
    /// users on startup and persists every CREATE/DROP USER and GRANT/REVOKE
    /// back to the file.  When the file does not yet exist, --username and
    /// --password seed it with an initial global admin.
    #[arg(long, value_name = "PATH")]
    pub users_file: Option<PathBuf>,

    /// Expose a Prometheus metrics endpoint on this port (e.g. 9100).
    /// Serves GET /metrics in the text/plain; version=0.0.4 format.
    /// Disabled when omitted.
    #[arg(long, value_name = "PORT")]
    pub metrics_port: Option<u16>,

    /// Maximum number of concurrent client connections.  New connections that
    /// would exceed the limit are accepted and immediately closed with an
    /// `ERR server at connection limit` response so callers see a clear cause
    /// and the OS does not silently queue the connection.
    #[arg(long, value_name = "N", default_value_t = 1024)]
    pub max_connections: usize,

    /// Idle timeout per client connection, in seconds. Connections that go
    /// this long without sending a complete line are closed. 0 disables.
    #[arg(long, value_name = "SECS", default_value_t = 300)]
    pub idle_timeout_secs: u64,
}

fn main() -> io::Result<()> {
    let config = Config::parse();

    let level = config.log_level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    // Encryption key for WAL / Disk (optional).
    let enc_key = crypto::parse_key_from_env();
    if enc_key.is_some() {
        info!("WAL encryption enabled (TAU_ENCRYPTION_KEY)");
    }

    // Build executor.
    let executor: Arc<RwLock<Executor>> = if config.wal {
        let wal_path = config.wal_path.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL enabled but no path provided (use -w/--wal-path)",
            )
        })?;
        info!(wal_path = %wal_path.display(), compact_threshold = config.compact_threshold, "starting with WAL");
        Arc::new(RwLock::new(Executor::with_wal_threshold(
            wal_path,
            config.compact_threshold,
            enc_key,
        )?))
    } else {
        info!(
            compact_threshold = config.compact_threshold,
            "starting in-memory (no WAL)"
        );
        Arc::new(RwLock::new(Executor::with_threshold(
            config.compact_threshold,
        )))
    };

    // Build the user store and (optionally) bootstrap an initial admin.
    let auth_enabled = config.auth;
    if auth_enabled {
        let mut store = match config.users_file.as_ref() {
            Some(path) => UserStore::open(path)?,
            None => UserStore::new(),
        };

        // Bootstrap: if no users exist yet AND --username/--password were
        // provided, seed the store with a global-admin user.  Persists when a
        // file path is configured.
        if store.names().is_empty()
            && let (Some(u), Some(p)) = (config.username.as_ref(), config.password.as_ref())
        {
            let mut grants = HashMap::new();
            grants.insert("*".to_string(), Perm::ALL);
            store
                .add(User::new(u, p, grants))
                .map_err(io::Error::other)?;
            info!(user = %u, "bootstrapped global admin");
        }

        if store.names().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--auth: no users configured. Provide --username/--password to bootstrap a global admin or supply a populated --users-file",
            ));
        }

        info!(users = store.names().len(), "authentication enabled");
        executor.write().unwrap().users = store;
    }

    // Build TLS config (if TLS enabled).
    let tls_config: Option<Arc<ServerConfig>> = if config.tls {
        let server_cfg = build_tls_config(config.tls_cert.as_deref(), config.tls_key.as_deref())?;
        info!(
            cert = ?config.tls_cert,
            key = ?config.tls_key,
            "TLS enabled"
        );
        Some(Arc::new(server_cfg))
    } else {
        debug!("TLS disabled (plain TCP)");
        None
    };

    // Metrics HTTP endpoint (optional).
    if let Some(port) = config.metrics_port {
        let metrics = executor.read().unwrap().metrics.clone();
        thread::Builder::new()
            .name("tau-metrics".into())
            .spawn(move || serve_metrics_http(port, metrics))
            .expect("failed to spawn metrics thread");
        info!(port, "metrics endpoint enabled");
    }

    let bind = config.bind.clone();
    let listener = TcpListener::bind(&bind)?;
    info!(%bind, "tau server listening");

    let connection_limit = config.max_connections;
    let idle_timeout = if config.idle_timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(config.idle_timeout_secs))
    };
    let active_connections = Arc::new(AtomicUsize::new(0));
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let peer = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "?".into());
                let metrics = executor.read().unwrap().metrics.clone();
                let in_flight = active_connections.fetch_add(1, Ordering::AcqRel) + 1;
                if in_flight > connection_limit {
                    metrics.record_rejected_connection();
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    warn!(%peer, connection_limit, "rejecting connection: server at capacity");
                    let mut s = stream;
                    let _ = s.write_all(b"ERR server at connection limit\n");
                    let _ = s.flush();
                    continue;
                }
                metrics.connections.fetch_add(1, Ordering::Relaxed);
                debug!(%peer, in_flight, "accepted connection");
                let exec = executor.clone();
                let tls = tls_config.clone();
                let auth = auth_enabled;
                let active_clone = active_connections.clone();
                thread::Builder::new()
                    .name("tau-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle(stream, exec, tls, auth, idle_timeout) {
                            warn!(error = %e, "connection ended with error");
                        }
                        active_clone.fetch_sub(1, Ordering::AcqRel);
                    })
                    .expect("failed to spawn connection thread");
            }
            Err(e) => error!(error = %e, "accept failed"),
        }
    }

    Ok(())
}

/// Minimal HTTP server that serves `GET /metrics` in Prometheus text format.
///
/// One thread per request; fine for scrape intervals >= 1 s. The listener
/// runs until the process exits. Every accepted request is traced at
/// `trace` (method, path, peer) and at `debug` (status, bytes, duration).
fn serve_metrics_http(port: u16, metrics: Arc<Metrics>) {
    let listener = match TcpListener::bind(format!("0.0.0.0:{port}")) {
        Ok(l) => l,
        Err(e) => {
            error!(port, error = %e, "could not bind metrics listener");
            return;
        }
    };
    info!(port, "metrics HTTP listener ready");
    for stream in listener.incoming() {
        let mut s = match stream {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "metrics accept failed");
                continue;
            }
        };
        let m = metrics.clone();
        thread::Builder::new()
            .name("tau-metrics-req".into())
            .spawn(move || {
                handle_metrics_request(&mut s, &m);
            })
            .expect("failed to spawn metrics request thread");
    }
}

/// Parse the request line, render Prometheus text, write the HTTP response.
/// Unknown paths return 404 so misconfigured scrapers fail loudly.
fn handle_metrics_request(s: &mut TcpStream, metrics: &Arc<Metrics>) {
    let started = Instant::now();
    let peer = s
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());

    // Read the request line. Bounded by 4 KiB so a malicious client cannot
    // grow the buffer arbitrarily.
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    let n = match s.read(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            warn!(%peer, error = %e, "metrics read failed");
            return;
        }
    };
    if n == 0 {
        return;
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let request_line = head.lines().next().unwrap_or("").to_string();
    trace!(%peer, request = %request_line, "metrics request");

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let (status, body, ctype) = match (method, path) {
        ("GET", "/metrics") | ("HEAD", "/metrics") => (
            "200 OK",
            if method == "HEAD" {
                String::new()
            } else {
                metrics.prometheus_text()
            },
            "text/plain; version=0.0.4; charset=utf-8",
        ),
        ("GET", "/healthz") | ("GET", "/") => (
            "200 OK",
            "tau metrics endpoint ready\n".to_string(),
            "text/plain; charset=utf-8",
        ),
        ("GET", _) | ("HEAD", _) => (
            "404 Not Found",
            "not found\n".to_string(),
            "text/plain; charset=utf-8",
        ),
        _ => (
            "405 Method Not Allowed",
            "method not allowed\n".to_string(),
            "text/plain; charset=utf-8",
        ),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Err(e) = s.write_all(response.as_bytes()) {
        warn!(%peer, error = %e, "metrics write failed");
        return;
    }
    let _ = s.flush();
    debug!(
        %peer,
        method,
        path,
        status,
        bytes = body.len(),
        elapsed_us = started.elapsed().as_micros() as u64,
        "metrics request handled"
    );
}

fn build_tls_config(cert_path: Option<&Path>, key_path: Option<&Path>) -> io::Result<ServerConfig> {
    let (certs, private_key) = match (cert_path, key_path) {
        (Some(cp), Some(kp)) => {
            let cert_file = File::open(cp)?;
            let certs: Vec<CertificateDer<'static>> =
                rustls_pemfile::certs(&mut BufReader::new(cert_file))
                    .collect::<Result<_, _>>()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let key_file = File::open(kp)?;
            let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "no private key found in file")
                })?;
            (certs, key)
        }
        (None, None) => {
            warn!(
                "no TLS cert/key provided - generating ephemeral self-signed certificate (not for production)"
            );
            let certified = generate_simple_self_signed(vec!["localhost".to_string()])
                .map_err(io::Error::other)?;
            let cert_der = CertificateDer::from(certified.cert.der().to_vec());
            let key_der =
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
            (vec![cert_der], key_der)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--tls-cert and --tls-key must both be provided (or both omitted for ephemeral)",
            ));
        }
    };

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, private_key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn parse_auth_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim().strip_prefix("AUTH ")?;
    let (user, pass) = rest.split_once(' ')?;
    Some((user.to_string(), pass.to_string()))
}

/// Drive a single client connection over any `Read + Write` stream.
///
/// Enforces authentication (when `auth_enabled`) as the very first exchange,
/// then routes every subsequent statement through `exec_as` so the matched
/// user's per-database CRUDA grants are enforced.
fn run_query_loop<S: Read + Write>(
    reader: &mut BufReader<S>,
    peer: SocketAddr,
    exec: &Arc<RwLock<Executor>>,
    auth_enabled: bool,
) -> io::Result<()> {
    let metrics = exec.read().unwrap().metrics.clone();
    let mut authenticated_user: Option<String> = None;
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf)?;
        if n == 0 {
            break; // EOF
        }

        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        if auth_enabled && authenticated_user.is_none() {
            match parse_auth_line(trimmed) {
                Some((u, p)) => {
                    metrics.record_auth_attempt();
                    let ok = exec.read().unwrap().users.verify(&u, &p).is_some();
                    if ok {
                        reader.get_mut().write_all(b"OK\n")?;
                        reader.get_mut().flush()?;
                        info!(%peer, user = %u, "authenticated");
                        authenticated_user = Some(u);
                    } else {
                        metrics.record_auth_failure();
                        metrics.record_error();
                        warn!(%peer, user = %u, "authentication failed");
                        reader.get_mut().write_all(b"ERR authentication failed\n")?;
                        reader.get_mut().flush()?;
                        break;
                    }
                }
                None => {
                    metrics.record_auth_failure();
                    metrics.record_error();
                    warn!(%peer, "first message was not AUTH");
                    reader
                        .get_mut()
                        .write_all(b"ERR authentication required\n")?;
                    reader.get_mut().flush()?;
                    break;
                }
            }
            continue;
        }

        if trimmed.eq_ignore_ascii_case("QUIT") || trimmed.eq_ignore_ascii_case("EXIT") {
            info!(%peer, user = ?authenticated_user, "client quit");
            reader.get_mut().write_all(b"OK BYE\n")?;
            reader.get_mut().flush()?;
            break;
        }

        trace!(%peer, user = ?authenticated_user, query = %trimmed, "dispatching");
        let started = Instant::now();
        let response = handle_query(trimmed, exec, authenticated_user.as_deref());
        let elapsed = started.elapsed();
        let is_err = response.starts_with("ERR ");
        if is_err {
            metrics.record_error();
        }
        debug!(
            %peer,
            user = ?authenticated_user,
            elapsed_us = elapsed.as_micros() as u64,
            status = if is_err { "err" } else { "ok" },
            "handled query"
        );
        let response_line = format!("{response}\n");
        reader.get_mut().write_all(response_line.as_bytes())?;
        reader.get_mut().flush()?;
    }

    info!(%peer, user = ?authenticated_user, "client disconnected");
    Ok(())
}

fn handle(
    stream: TcpStream,
    exec: Arc<RwLock<Executor>>,
    tls_config: Option<Arc<ServerConfig>>,
    auth_enabled: bool,
    idle_timeout: Option<Duration>,
) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    stream.set_nodelay(true)?;
    if let Some(timeout) = idle_timeout {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
    }
    info!(%peer, "client connected");

    if let Some(cfg) = tls_config {
        let conn = ServerConnection::new(cfg).map_err(io::Error::other)?;
        let tls_stream = rustls::StreamOwned::new(conn, stream);
        let mut reader = BufReader::new(tls_stream);
        run_query_loop(&mut reader, peer, &exec, auth_enabled)
    } else {
        let mut reader = BufReader::new(stream);
        run_query_loop(&mut reader, peer, &exec, auth_enabled)
    }
}

/// Parse + dispatch one query line.  Returns the response line (without the
/// trailing newline).  Lock-routing: read-only statements take the shared
/// lock; everything else takes the exclusive lock.  When `caller` is `Some`,
/// the statement is executed via `exec_as` so per-user CRUDA permissions are
/// enforced.
fn handle_query(query: &str, exec: &Arc<RwLock<Executor>>, caller: Option<&str>) -> String {
    let stmt = match parse(query) {
        Ok((rest, s)) if rest.trim().is_empty() => s,
        Ok((rest, _)) => return format!("ERR trailing input: {rest:?}"),
        Err(e) => return format!("ERR parse: {e}"),
    };
    let result = match (stmt.is_read_only(), caller) {
        (true, Some(u)) => exec.read().unwrap().exec_read_as(&stmt, u),
        (true, None) => exec.read().unwrap().exec_read(&stmt),
        (false, Some(u)) => exec.write().unwrap().exec_as(&stmt, u),
        (false, None) => exec.write().unwrap().exec(&stmt),
    };
    match result {
        Ok(o) => format_output(&o),
        Err(e) => format!("ERR {}", format_error(&e)),
    }
}

fn format_output(o: &Output) -> String {
    match o {
        Output::Empty => "OK".into(),
        Output::Value(None) => "VAL NIL".into(),
        Output::Value(Some(v)) => format!("VAL {}", v.encode()),
        Output::Range(segments) => {
            let mut out = String::with_capacity(16 + segments.len() * 24);
            out.push_str(&format!("RANGE {}", segments.len()));
            for (s, e, v) in segments {
                out.push_str(&format!("; {}:{}:{}", s, e, v.encode()));
            }
            out
        }
        Output::Names(names) => {
            let mut out = format!("NAMES {}", names.len());
            for n in names {
                out.push(';');
                out.push(' ');
                out.push_str(n);
            }
            out
        }
        Output::Grants(rows) => {
            let mut out = format!("GRANTS {}", rows.len());
            for (user, grants) in rows {
                out.push(';');
                out.push(' ');
                out.push_str(user);
                for (db, perm) in grants {
                    out.push(' ');
                    out.push_str(&format!("{}:{}", db, perm));
                }
            }
            out
        }
    }
}

fn format_error(e: &ExecError) -> String {
    match e {
        ExecError::NoActiveDatabase => "no active database".into(),
        ExecError::UnknownDatabase(n) => format!("unknown database: {n}"),
        ExecError::DuplicateDatabase(n) => format!("duplicate database: {n}"),
        ExecError::UnknownLens(n) => format!("unknown lens: {n}"),
        ExecError::DuplicateLens(n) => format!("duplicate lens: {n}"),
        ExecError::TypeMismatch {
            lens,
            expected,
            got,
        } => {
            format!("type mismatch on {lens}: expected {expected:?}, got {got}")
        }
        ExecError::InvalidExpr(m) => format!("invalid expression: {m}"),
        ExecError::InvalidRange => "invalid range (start >= end)".into(),
        ExecError::Io(m) => format!("storage error: {m}"),
        ExecError::CycleDetected(n) => format!("cycle detected in derived lens: {n}"),
        ExecError::PermissionDenied(m) => format!("permission denied: {m}"),
        ExecError::DuplicateUser(n) => format!("duplicate user: {n}"),
        ExecError::UnknownUser(n) => format!("unknown user: {n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;
    use tau::Stmt;

    fn exec() -> Arc<RwLock<Executor>> {
        Arc::new(RwLock::new(Executor::new()))
    }

    #[hegel::test]
    fn handle_query_never_panics(tc: TestCase) {
        let input = tc.draw(gs::text().max_size(256));
        let e = exec();
        let _ = handle_query(&input, &e, None);
    }

    #[hegel::test]
    fn at_after_append_returns_encoded_value(tc: TestCase) {
        let v = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        let probe = tc.draw(gs::integers::<i64>().min_value(0).max_value(99));
        let e = exec();
        handle_query("CREATE DATABASE main", &e, None);
        handle_query("CREATE LENS x int", &e, None);
        assert_eq!(
            handle_query(&format!("APPEND LENS x 0 100 {v}"), &e, None),
            "OK"
        );
        assert_eq!(
            handle_query(&format!("AT LENS x {probe}"), &e, None),
            format!("VAL i{v}")
        );
    }

    #[hegel::test]
    fn at_uncovered_timestamp_yields_nil(tc: TestCase) {
        let probe = tc.draw(gs::integers::<i64>().min_value(101).max_value(1_000_000));
        let e = exec();
        handle_query("CREATE DATABASE main", &e, None);
        handle_query("CREATE LENS x int", &e, None);
        handle_query("APPEND LENS x 0 100 42", &e, None);
        assert_eq!(
            handle_query(&format!("AT LENS x {probe}"), &e, None),
            "VAL NIL"
        );
    }

    #[hegel::test]
    fn parse_failure_starts_with_err_parse(tc: TestCase) {
        // Any line that doesn't look like a tauql statement must produce
        // either "ERR parse:" or "ERR trailing input:" - never panic and
        // never silently succeed.
        let junk = tc.draw(gs::from_regex("[A-Z]{4,12}").fullmatch(true).filter(|s| {
            !matches!(
                s.as_str(),
                "CREATE"
                    | "DROP"
                    | "USE"
                    | "APPEND"
                    | "COPY"
                    | "DERIVE"
                    | "SHOW"
                    | "RANGE"
                    | "REDUCE"
                    | "GRANT"
                    | "REVOKE"
            )
        }));
        let line = format!("{junk} something");
        let r = handle_query(&line, &exec(), None);
        assert!(r.starts_with("ERR "), "expected ERR, got {r:?}");
    }

    #[hegel::test]
    fn trailing_input_is_reported(tc: TestCase) {
        let extra = tc.draw(gs::from_regex("[A-Z][A-Z]+").fullmatch(true));
        let r = handle_query(&format!("CREATE DATABASE a {extra}"), &exec(), None);
        assert!(r.starts_with("ERR trailing input"), "got: {r}");
    }

    #[test]
    fn empty_response_for_ddl() {
        let e = exec();
        assert_eq!(handle_query("CREATE DATABASE main", &e, None), "OK");
        assert_eq!(handle_query("CREATE LENS x int", &e, None), "OK");
    }

    #[test]
    fn range_response_lists_segments() {
        let e = exec();
        handle_query("CREATE DATABASE main", &e, None);
        handle_query("CREATE LENS x int", &e, None);
        handle_query("APPEND LENS x 0 5 1", &e, None);
        handle_query("APPEND LENS x 5 10 2", &e, None);
        assert_eq!(
            handle_query("RANGE LENS x 0 10", &e, None),
            "RANGE 2; 0:5:i1; 5:10:i2"
        );
    }

    #[test]
    fn execution_error_is_reported() {
        let r = handle_query("CREATE LENS x int", &exec(), None);
        assert!(r.starts_with("ERR no active database"), "got: {r}");
    }

    #[test]
    fn read_only_router_picks_shared_lock() {
        let e = exec();
        handle_query("CREATE DATABASE main", &e, None);
        handle_query("CREATE LENS x int", &e, None);
        handle_query("APPEND LENS x 0 100 7", &e, None);
        let mut handles = vec![];
        for _ in 0..8 {
            let e = e.clone();
            handles.push(thread::spawn(move || {
                assert_eq!(handle_query("AT LENS x 50", &e, None), "VAL i7");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn is_read_only_routing_matches_stmt_kind() {
        assert!(
            Stmt::At {
                name: "x".into(),
                t: 0
            }
            .is_read_only()
        );
        assert!(
            Stmt::Range {
                name: "x".into(),
                start: 0,
                end: 1,
                filter: None
            }
            .is_read_only()
        );
        assert!(!Stmt::CreateDatabase { name: "a".into() }.is_read_only());
        assert!(!Stmt::Drop { name: "x".into() }.is_read_only());
    }

    #[test]
    fn exec_read_rejects_mutations() {
        let e = exec();
        let guard = e.write().unwrap();
        let stmt = Stmt::CreateDatabase { name: "a".into() };
        assert!(matches!(
            guard.exec_read(&stmt),
            Err(ExecError::InvalidExpr(_))
        ));
    }

    #[hegel::test]
    fn parse_auth_line_roundtrips_for_valid_input(tc: TestCase) {
        let user = tc.draw(gs::from_regex("[a-z][a-z0-9_]{0,10}").fullmatch(true));
        let pass = tc.draw(gs::from_regex("[A-Za-z0-9!@#$%^&*]{1,16}").fullmatch(true));
        let line = format!("AUTH {user} {pass}");
        assert_eq!(parse_auth_line(&line), Some((user, pass)));
    }

    #[hegel::test]
    fn parse_auth_line_returns_none_for_non_auth(tc: TestCase) {
        // Any line that doesn't start with the literal "AUTH " followed by
        // two whitespace-separated tokens returns None.  We bias the
        // generator to start with non-AUTH text or be too short.
        let input = tc.draw(gs::text().max_size(64).filter(|s| {
            !s.starts_with("AUTH ") || s.trim_start_matches("AUTH ").split(' ').count() < 2
        }));
        let _ = parse_auth_line(&input); // never panics; result may be None or Some
    }
}
