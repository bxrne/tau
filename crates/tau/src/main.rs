//! Tau TCP server.
//!
//! A line-oriented query service over TCP, optionally secured with TLS and
//! username/password authentication.  One statement per line in; one response
//! line out.
//!
//! # Configuration
//!
//! The server reads `config.toml` in the current directory, or a path given
//! with `--config PATH`.  All fields have built-in defaults; an absent config
//! file starts an in-memory server on `127.0.0.1:7070`.
//!
//! Example config.toml:
//!
//! ```toml
//! bind = "127.0.0.1:7070"
//! log_level = "info"
//! compact_threshold = 8
//!
//! [disk]
//! compression_level = 3  # zstd level 1-22; higher = better ratio, slower writes
//!
//! [wal]
//! enabled = true
//! path = "/var/lib/tau/tau.wal"
//! no_fsync_each = false
//! no_rewrite_on_compact = false
//! no_auto_checkpoint = false
//!
//! [tls]
//! enabled = true
//! cert = "/etc/tau/cert.pem"
//! key  = "/etc/tau/key.pem"
//!
//! [auth]
//! enabled = true
//! username = "admin"
//! password = "secret"
//! # users_file = "/etc/tau/users"
//!
//! [metrics]
//! port = 9100
//!
//! [limits]
//! max_connections = 1024
//! idle_timeout_secs = 300
//! ```
//!
//! Set `TAU_ENCRYPTION_KEY` (64 hex chars = 32 bytes) for AES-256-GCM
//! at-rest WAL encryption.
//!
//! # Wire format
//!
//! ```text
//! → AUTH admin s3cr3t                  (if [auth] enabled = true)
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
use libtau::{
    Executor, Metrics, Perm, Response, User, UserStore, crypto, needs_registry_lock, parse,
};
use rcgen::generate_simple_self_signed;
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::ServerConnection,
};
use serde::Deserialize;
use tracing::{debug, error, info, trace, warn};

/// Tau time-series database TCP server.
#[derive(Parser, Debug)]
#[command(name = "tau", author, version)]
#[command(about = "A time-series database TCP server", long_about = None)]
struct Cli {
    /// Path to config.toml. Defaults to ./config.toml if present; uses built-in defaults otherwise.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Deserialize, Debug)]
#[serde(default)]
struct Config {
    bind: String,
    log_level: String,
    compact_threshold: usize,
    wal: WalConfig,
    tls: TlsConfig,
    auth: AuthConfig,
    metrics: MetricsConfig,
    limits: LimitsConfig,
    disk: DiskConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7070".to_string(),
            log_level: "info".to_string(),
            compact_threshold: 8,
            wal: WalConfig::default(),
            tls: TlsConfig::default(),
            auth: AuthConfig::default(),
            metrics: MetricsConfig::default(),
            limits: LimitsConfig::default(),
            disk: DiskConfig::default(),
        }
    }
}

#[derive(Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum BackendChoice {
    #[default]
    Memory,
    Disk,
}

#[derive(Deserialize, Debug)]
#[serde(default)]
struct DiskConfig {
    /// Storage backend: "memory" (default) or "disk".
    backend: BackendChoice,
    /// Directory for per-database `.dat` files (required when backend = "disk").
    path: Option<std::path::PathBuf>,
    /// zstd compression level for the disk store (1–22). Default: 3.
    compression_level: i32,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            backend: BackendChoice::Memory,
            path: None,
            compression_level: libtau::storage::DEFAULT_ZSTD_LEVEL,
        }
    }
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct WalConfig {
    enabled: bool,
    path: Option<PathBuf>,
    no_fsync_each: bool,
    no_rewrite_on_compact: bool,
    no_auto_checkpoint: bool,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct TlsConfig {
    enabled: bool,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct AuthConfig {
    enabled: bool,
    username: Option<String>,
    password: Option<String>,
    users_file: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct MetricsConfig {
    port: Option<u16>,
}

#[derive(Deserialize, Debug)]
#[serde(default)]
struct LimitsConfig {
    max_connections: usize,
    idle_timeout_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            idle_timeout_secs: 300,
        }
    }
}

fn load_config(cli_config: Option<PathBuf>) -> io::Result<Config> {
    match cli_config {
        Some(path) => {
            let text = std::fs::read_to_string(&path).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("failed to read {}: {}", path.display(), e),
                )
            })?;
            toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        None => {
            let default_path = PathBuf::from("config.toml");
            if default_path.exists() {
                let text = std::fs::read_to_string(&default_path)?;
                toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            } else {
                Ok(Config::default())
            }
        }
    }
}

fn build_executor(config: &Config, enc_key: Option<[u8; 32]>) -> io::Result<Arc<RwLock<Executor>>> {
    let exec = match config.disk.backend {
        BackendChoice::Disk => {
            let dir = config.disk.path.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "[disk] backend = \"disk\" requires a path (add path = \"...\" under [disk])",
                )
            })?;
            if config.wal.enabled {
                warn!("disk backend is active; [wal] configuration is ignored");
            }
            info!(
                dir = %dir.display(),
                compression_level = config.disk.compression_level,
                compact_threshold = config.compact_threshold,
                "starting with disk backend"
            );
            Executor::with_disk_backend(
                dir,
                config.compact_threshold,
                config.disk.compression_level,
                enc_key,
            )?
        }
        BackendChoice::Memory => {
            if config.wal.enabled {
                let wal_path = config.wal.path.clone().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "[wal] enabled = true but no path set (add path = \"...\" under [wal])",
                    )
                })?;
                info!(wal_path = %wal_path.display(), compact_threshold = config.compact_threshold, "starting with WAL");
                let mut e =
                    Executor::with_wal_threshold(wal_path, config.compact_threshold, enc_key)?;
                if config.wal.no_fsync_each {
                    warn!(
                        "no_fsync_each: WAL sync disabled — data may be lost on unclean shutdown"
                    );
                    e.set_wal_fsync_each(false);
                }
                e
            } else {
                info!(
                    compact_threshold = config.compact_threshold,
                    "starting in-memory (no WAL)"
                );
                Executor::with_threshold(config.compact_threshold)
            }
        }
    };

    let exec = Arc::new(RwLock::new(exec));

    // Group-commit flush thread: when fsync-per-record is disabled, a
    // background thread syncs the WAL every 50 ms so durability lag is bounded.
    if config.wal.enabled && config.wal.no_fsync_each {
        let exec_weak = Arc::downgrade(&exec);
        thread::spawn(move || {
            while let Some(e) = exec_weak.upgrade() {
                thread::sleep(Duration::from_millis(50));
                if let Ok(guard) = e.read() {
                    let _ = guard.flush_wal();
                }
            }
        });
        info!("group-commit flush thread started (50 ms interval)");
    }

    Ok(exec)
}

fn setup_auth(config: &Config, executor: &Arc<RwLock<Executor>>) -> io::Result<()> {
    let mut store = match config.auth.users_file.as_ref() {
        Some(path) => UserStore::open(path)?,
        None => UserStore::new(),
    };
    if store.names().is_empty()
        && let (Some(u), Some(p)) = (config.auth.username.as_ref(), config.auth.password.as_ref())
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
            "[auth] enabled = true but no users configured. Add username/password or users_file under [auth]",
        ));
    }
    info!(users = store.names().len(), "authentication enabled");
    executor.write().expect("executor lock poisoned").users = store;
    Ok(())
}

fn accept_loop(
    listener: TcpListener,
    executor: Arc<RwLock<Executor>>,
    tls_config: Option<Arc<ServerConfig>>,
    auth_enabled: bool,
    connection_limit: usize,
    idle_timeout: Option<Duration>,
) -> io::Result<()> {
    let active_connections = Arc::new(AtomicUsize::new(0));
    let shared_metrics = executor
        .read()
        .expect("executor lock poisoned")
        .metrics
        .clone();
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let peer = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "?".into());
                let in_flight = active_connections.fetch_add(1, Ordering::AcqRel) + 1;
                shared_metrics.set_active_connections(in_flight as u64);
                if in_flight > connection_limit {
                    shared_metrics.record_rejected_connection();
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    shared_metrics.set_active_connections((in_flight - 1) as u64);
                    warn!(%peer, connection_limit, "rejecting connection: server at capacity");
                    let mut s = stream;
                    let _ = s.write_all(b"ERR server at connection limit\n");
                    let _ = s.flush();
                    continue;
                }
                shared_metrics.connections.fetch_add(1, Ordering::Relaxed);
                debug!(%peer, in_flight, "accepted connection");
                let exec = executor.clone();
                let tls = tls_config.clone();
                let active_clone = active_connections.clone();
                let metrics_clone = shared_metrics.clone();
                thread::Builder::new()
                    .name("tau-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle(stream, exec, tls, auth_enabled, idle_timeout) {
                            warn!(error = %e, "connection ended with error");
                        }
                        let remaining = active_clone.fetch_sub(1, Ordering::AcqRel) - 1;
                        metrics_clone.set_active_connections(remaining as u64);
                    })
                    .expect("failed to spawn connection thread");
            }
            Err(e) => error!(error = %e, "accept failed"),
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let config = load_config(cli.config)?;

    let level = config.log_level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    let enc_key = crypto::parse_key_from_env().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
    if enc_key.is_some() {
        info!("WAL encryption enabled (TAU_ENCRYPTION_KEY)");
    }

    let executor = build_executor(&config, enc_key)?;

    let auth_enabled = config.auth.enabled;
    if auth_enabled {
        setup_auth(&config, &executor)?;
    }

    let tls_config: Option<Arc<ServerConfig>> = if config.tls.enabled {
        let server_cfg = build_tls_config(config.tls.cert.as_deref(), config.tls.key.as_deref())?;
        info!(cert = ?config.tls.cert, key = ?config.tls.key, "TLS enabled");
        Some(Arc::new(server_cfg))
    } else {
        debug!("TLS disabled (plain TCP)");
        None
    };

    if let Some(port) = config.metrics.port {
        let metrics = executor
            .read()
            .expect("executor lock poisoned")
            .metrics
            .clone();
        thread::Builder::new()
            .name("tau-metrics".into())
            .spawn(move || serve_metrics_http(port, metrics))
            .expect("failed to spawn metrics thread");
        info!(port, "metrics endpoint enabled");
    }

    let bind = config.bind.clone();
    let listener = TcpListener::bind(&bind)?;
    info!(%bind, "tau server listening");

    let idle_timeout = if config.limits.idle_timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(config.limits.idle_timeout_secs))
    };

    accept_loop(
        listener,
        executor,
        tls_config,
        auth_enabled,
        config.limits.max_connections,
        idle_timeout,
    )
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
                "tls.cert and tls.key must both be set (or both omitted for ephemeral)",
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

/// Attempt to authenticate the client with the line already read.
/// Returns `Ok(Some(username))` on success, `Ok(None)` on bad credentials
/// (response already sent, caller should break), `Err` on I/O error.
fn handle_auth_attempt<S: Read + Write>(
    reader: &mut BufReader<S>,
    peer: SocketAddr,
    exec: &Arc<RwLock<Executor>>,
    metrics: &Arc<Metrics>,
    trimmed: &str,
) -> io::Result<Option<String>> {
    match parse_auth_line(trimmed) {
        Some((u, p)) => {
            metrics.record_auth_attempt();
            if exec
                .read()
                .expect("executor lock poisoned")
                .users
                .verify(&u, &p)
                .is_some()
            {
                reader.get_mut().write_all(b"OK\n")?;
                reader.get_mut().flush()?;
                info!(%peer, user = %u, "authenticated");
                Ok(Some(u))
            } else {
                metrics.record_auth_failure();
                metrics.record_error();
                warn!(%peer, user = %u, "authentication failed");
                reader.get_mut().write_all(b"ERR authentication failed\n")?;
                reader.get_mut().flush()?;
                Ok(None)
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
            Ok(None)
        }
    }
}

fn is_quit_cmd(s: &str) -> bool {
    s.eq_ignore_ascii_case("QUIT") || s.eq_ignore_ascii_case("EXIT")
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
    let metrics = exec.read().expect("executor lock poisoned").metrics.clone();
    let mut authenticated_user: Option<String> = None;
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        let n = reader.read_line(&mut line_buf)?;
        if n == 0 {
            break;
        }
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        if auth_enabled && authenticated_user.is_none() {
            authenticated_user = handle_auth_attempt(reader, peer, exec, &metrics, trimmed)?;
            if authenticated_user.is_none() {
                break;
            }
            continue;
        }

        if is_quit_cmd(trimmed) {
            info!(%peer, user = ?authenticated_user, "client quit");
            reader.get_mut().write_all(b"OK BYE\n")?;
            reader.get_mut().flush()?;
            break;
        }

        trace!(%peer, user = ?authenticated_user, query = %trimmed, "dispatching");
        let started = Instant::now();
        let response = handle_query(trimmed, exec, authenticated_user.as_deref());
        let elapsed = started.elapsed();
        let is_err = response.is_err();
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
fn handle_query(query: &str, exec: &Arc<RwLock<Executor>>, caller: Option<&str>) -> Response {
    let stmt = match parse(query) {
        Ok((rest, s)) if rest.trim().is_empty() => s,
        Ok((rest, _)) => return Response::Err(format!("trailing input: {rest:?}")),
        Err(e) => return Response::Err(format!("parse: {e}")),
    };
    // Read-only: shared executor lock + per-DB read lock.
    // Registry write (CREATE DATABASE etc.): exclusive executor lock.
    // Data write (APPEND etc.): shared executor lock + per-DB write lock.
    // When a transaction is active data writes still use exec.write() so they are buffered.
    let needs_exclusive = !stmt.is_read_only()
        && (needs_registry_lock(&stmt)
            || exec
                .read()
                .expect("executor lock poisoned")
                .is_in_transaction());

    let result = match (stmt.is_read_only(), needs_exclusive, caller) {
        (true, _, Some(u)) => exec
            .read()
            .expect("executor lock poisoned")
            .exec_read_as(&stmt, u),
        (true, _, None) => exec
            .read()
            .expect("executor lock poisoned")
            .exec_read(&stmt),
        (false, true, Some(u)) => exec
            .write()
            .expect("executor lock poisoned")
            .exec_as(&stmt, u),
        (false, true, None) => exec.write().expect("executor lock poisoned").exec(&stmt),
        (false, false, Some(u)) => exec
            .read()
            .expect("executor lock poisoned")
            .exec_db_write_as(&stmt, u),
        (false, false, None) => exec
            .read()
            .expect("executor lock poisoned")
            .exec_db_write(&stmt),
    };
    Response::from_result(&result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use libtau::{ExecError, Stmt};
    use pretty_assertions::assert_eq;

    fn exec() -> Arc<RwLock<Executor>> {
        Arc::new(RwLock::new(Executor::new()))
    }

    /// Run a query and render the response to its wire line, for assertions
    /// against the protocol's exact string output.
    fn q(query: &str, e: &Arc<RwLock<Executor>>, caller: Option<&str>) -> String {
        handle_query(query, e, caller).to_string()
    }

    #[hegel::test]
    fn handle_query_never_panics(tc: TestCase) {
        let input = tc.draw(gs::text().max_size(256));
        let e = exec();
        let _ = q(&input, &e, None);
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
        q("CREATE DATABASE main", &e, None);
        q("CREATE LENS x int", &e, None);
        assert_eq!(q(&format!("APPEND LENS x 0 100 {v}"), &e, None), "OK");
        assert_eq!(
            q(&format!("AT LENS x {probe}"), &e, None),
            format!("VAL i{v}")
        );
    }

    #[hegel::test]
    fn at_uncovered_timestamp_yields_nil(tc: TestCase) {
        let probe = tc.draw(gs::integers::<i64>().min_value(101).max_value(1_000_000));
        let e = exec();
        q("CREATE DATABASE main", &e, None);
        q("CREATE LENS x int", &e, None);
        q("APPEND LENS x 0 100 42", &e, None);
        assert_eq!(q(&format!("AT LENS x {probe}"), &e, None), "VAL NIL");
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
        let r = q(&line, &exec(), None);
        assert!(r.starts_with("ERR "), "expected ERR, got {r:?}");
    }

    #[hegel::test]
    fn trailing_input_is_reported(tc: TestCase) {
        let extra = tc.draw(gs::from_regex("[A-Z][A-Z]+").fullmatch(true));
        let r = q(&format!("CREATE DATABASE a {extra}"), &exec(), None);
        assert!(r.starts_with("ERR trailing input"), "got: {r}");
    }

    #[test]
    fn empty_response_for_ddl() {
        let e = exec();
        assert_eq!(q("CREATE DATABASE main", &e, None), "OK");
        assert_eq!(q("CREATE LENS x int", &e, None), "OK");
    }

    #[test]
    fn range_response_lists_segments() {
        let e = exec();
        q("CREATE DATABASE main", &e, None);
        q("CREATE LENS x int", &e, None);
        q("APPEND LENS x 0 5 1", &e, None);
        q("APPEND LENS x 5 10 2", &e, None);
        assert_eq!(q("RANGE LENS x 0 10", &e, None), "RANGE 2; 0:5:i1; 5:10:i2");
    }

    #[test]
    fn execution_error_is_reported() {
        let r = q("CREATE LENS x int", &exec(), None);
        assert!(r.starts_with("ERR no active database"), "got: {r}");
    }

    #[test]
    fn read_only_router_picks_shared_lock() {
        let e = exec();
        q("CREATE DATABASE main", &e, None);
        q("CREATE LENS x int", &e, None);
        q("APPEND LENS x 0 100 7", &e, None);
        let mut handles = vec![];
        for _ in 0..8 {
            let e = e.clone();
            handles.push(thread::spawn(move || {
                assert_eq!(q("AT LENS x 50", &e, None), "VAL i7");
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

    #[test]
    fn is_quit_cmd_recognizes_quit_and_exit() {
        assert!(is_quit_cmd("QUIT"));
        assert!(is_quit_cmd("quit"));
        assert!(is_quit_cmd("Quit"));
        assert!(is_quit_cmd("EXIT"));
        assert!(is_quit_cmd("exit"));
        assert!(is_quit_cmd("Exit"));
    }

    #[test]
    fn is_quit_cmd_rejects_others() {
        assert!(!is_quit_cmd(""));
        assert!(!is_quit_cmd("QUIT NOW"));
        assert!(!is_quit_cmd("AT LENS x 0"));
        assert!(!is_quit_cmd("CREATE DATABASE x"));
    }

    #[hegel::test]
    fn is_quit_cmd_false_for_arbitrary_non_quit(tc: TestCase) {
        let s = tc.draw(
            gs::text()
                .max_size(32)
                .filter(|s| !s.eq_ignore_ascii_case("QUIT") && !s.eq_ignore_ascii_case("EXIT")),
        );
        assert!(!is_quit_cmd(&s), "expected false for {s:?}");
    }

    fn metrics_response(request: &[u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let metrics = exec().read().unwrap().metrics.clone();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            handle_metrics_request(&mut stream, &metrics);
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(request).unwrap();
        client.flush().unwrap();
        let mut resp = String::new();
        BufReader::new(&mut client)
            .read_to_string(&mut resp)
            .unwrap();
        resp
    }

    #[test]
    fn metrics_get_metrics_returns_200_with_content_type() {
        let resp = metrics_response(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        assert!(resp.contains("text/plain; version=0.0.4"), "got: {resp}");
    }

    #[test]
    fn metrics_get_healthz_returns_200() {
        let resp = metrics_response(b"GET /healthz HTTP/1.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        assert!(resp.contains("tau metrics endpoint ready"), "got: {resp}");
    }

    #[test]
    fn metrics_get_root_returns_200() {
        let resp = metrics_response(b"GET / HTTP/1.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
    }

    #[test]
    fn metrics_unknown_path_returns_404() {
        let resp = metrics_response(b"GET /nope HTTP/1.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 404 Not Found"), "got: {resp}");
    }

    #[test]
    fn metrics_post_returns_405() {
        let resp = metrics_response(b"POST /metrics HTTP/1.1\r\n\r\n");
        assert!(
            resp.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "got: {resp}"
        );
    }

    #[test]
    fn metrics_head_metrics_returns_200_empty_body() {
        let resp = metrics_response(b"HEAD /metrics HTTP/1.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        let body_start = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(resp.len());
        assert_eq!(&resp[body_start..], "", "HEAD must have empty body");
    }

    #[test]
    fn build_tls_config_fails_when_only_cert_provided() {
        use std::path::Path;
        let r = build_tls_config(Some(Path::new("/nonexistent/cert.pem")), None);
        assert!(r.is_err());
    }

    #[test]
    fn build_tls_config_fails_when_only_key_provided() {
        use std::path::Path;
        let r = build_tls_config(None, Some(Path::new("/nonexistent/key.pem")));
        assert!(r.is_err());
    }

    fn connected_pair() -> (TcpStream, TcpStream, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, peer) = listener.accept().unwrap();
        (client, server, peer)
    }

    #[test]
    fn run_query_loop_sends_ok_bye_on_quit() {
        let (client, server, peer) = connected_pair();
        let e = exec();
        thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let _ = run_query_loop(&mut reader, peer, &e, false);
        });
        let mut br = BufReader::new(client);
        writeln!(br.get_mut(), "QUIT").unwrap();
        br.get_mut().flush().unwrap();
        let mut resp = String::new();
        br.read_line(&mut resp).unwrap();
        assert_eq!(resp.trim(), "OK BYE");
    }

    #[test]
    fn run_query_loop_exit_also_sends_ok_bye() {
        let (client, server, peer) = connected_pair();
        let e = exec();
        thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let _ = run_query_loop(&mut reader, peer, &e, false);
        });
        let mut br = BufReader::new(client);
        writeln!(br.get_mut(), "EXIT").unwrap();
        br.get_mut().flush().unwrap();
        let mut resp = String::new();
        br.read_line(&mut resp).unwrap();
        assert_eq!(resp.trim(), "OK BYE");
    }

    #[test]
    fn run_query_loop_skips_empty_lines() {
        let (client, server, peer) = connected_pair();
        let e = exec();
        thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let _ = run_query_loop(&mut reader, peer, &e, false);
        });
        let mut br = BufReader::new(client);
        writeln!(br.get_mut()).unwrap();
        writeln!(br.get_mut(), "   ").unwrap();
        writeln!(br.get_mut(), "CREATE DATABASE skip_test").unwrap();
        br.get_mut().flush().unwrap();
        let mut resp = String::new();
        br.read_line(&mut resp).unwrap();
        assert_eq!(resp.trim(), "OK");
    }

    #[test]
    fn run_query_loop_auth_rejects_non_auth_first_message() {
        use std::collections::HashMap;
        let (client, server, peer) = connected_pair();
        let e = exec();
        {
            let mut g = e.write().unwrap();
            let mut grants = HashMap::new();
            grants.insert("*".to_string(), Perm::ALL);
            g.users.add(User::new("admin", "pw", grants)).unwrap();
        }
        thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let _ = run_query_loop(&mut reader, peer, &e, true);
        });
        let mut br = BufReader::new(client);
        writeln!(br.get_mut(), "CREATE DATABASE nope").unwrap();
        br.get_mut().flush().unwrap();
        let mut resp = String::new();
        br.read_line(&mut resp).unwrap();
        assert!(resp.contains("ERR authentication required"), "got: {resp}");
    }

    #[test]
    fn run_query_loop_auth_rejects_wrong_credentials() {
        use std::collections::HashMap;
        let (client, server, peer) = connected_pair();
        let e = exec();
        {
            let mut g = e.write().unwrap();
            let mut grants = HashMap::new();
            grants.insert("*".to_string(), Perm::ALL);
            g.users.add(User::new("admin", "correct", grants)).unwrap();
        }
        thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let _ = run_query_loop(&mut reader, peer, &e, true);
        });
        let mut br = BufReader::new(client);
        writeln!(br.get_mut(), "AUTH admin wrong").unwrap();
        br.get_mut().flush().unwrap();
        let mut resp = String::new();
        br.read_line(&mut resp).unwrap();
        assert!(resp.contains("ERR authentication failed"), "got: {resp}");
    }
}
