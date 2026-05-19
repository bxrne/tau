//! Tau TCP server.
//!
//! A line-oriented query service over TCP, optionally secured with TLS and
//! username/password authentication.  One statement per line in; one response
//! line out.
//!
//! # Security
//!
//! * `--tls` — enables TLS. Provide `--tls-cert` / `--tls-key` PEM files or
//!   omit both to generate an ephemeral self-signed certificate (dev only).
//! * `--auth` — enables authentication. Requires `--username` and `--password`.
//!   The first message from every client must be `AUTH <user> <pass>\n`.
//! * `TAU_ENCRYPTION_KEY` env var — 64 hex chars (32 bytes). When set, WAL
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

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;

use clap::Parser;
use rcgen::generate_simple_self_signed;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::ServerConnection;
use tau::libtau::auth::Credentials;
use tau::libtau::crypto;
use tau::{Codec, ExecError, Executor, Output, parse};
use tracing::{error, info, warn};

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

    /// Enable username/password authentication.
    #[arg(long)]
    pub auth: bool,

    /// Username for authentication (requires --auth).
    #[arg(long, value_name = "NAME")]
    pub username: Option<String>,

    /// Password for authentication (requires --auth). Hashed with argon2id at
    /// startup; the plaintext is not retained.
    #[arg(long, value_name = "PASS")]
    pub password: Option<String>,
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

    // Build credentials (if auth enabled).
    let credentials: Option<Arc<Credentials>> = if config.auth {
        let username = config.username.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "--auth requires --username")
        })?;
        let password = config.password.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "--auth requires --password")
        })?;
        info!(%username, "authentication enabled");
        Some(Arc::new(Credentials::new(&username, &password)))
    } else {
        None
    };

    // Build TLS config (if TLS enabled).
    let tls_config: Option<Arc<ServerConfig>> = if config.tls {
        let server_cfg = build_tls_config(config.tls_cert.as_deref(), config.tls_key.as_deref())?;
        info!("TLS enabled");
        Some(Arc::new(server_cfg))
    } else {
        None
    };

    let bind = config.bind.clone();
    let listener = TcpListener::bind(&bind)?;
    info!(%bind, "tau server listening");

    // TODO: gate connection acceptance with a semaphore bounded to a max
    // connection count; unbounded thread::spawn will exhaust OS resources
    // under a connection flood.
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let exec = executor.clone();
                let creds = credentials.clone();
                let tls = tls_config.clone();
                thread::Builder::new()
                    .name("tau-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle(stream, exec, tls, creds) {
                            warn!(error = %e, "connection ended with error");
                        }
                    })
                    .expect("failed to spawn connection thread");
            }
            Err(e) => error!(error = %e, "accept failed"),
        }
    }

    Ok(())
}

fn build_tls_config(
    cert_path: Option<&std::path::Path>,
    key_path: Option<&std::path::Path>,
) -> io::Result<ServerConfig> {
    let (certs, private_key) = match (cert_path, key_path) {
        (Some(cp), Some(kp)) => {
            let cert_file = std::fs::File::open(cp)?;
            let certs: Vec<CertificateDer<'static>> =
                rustls_pemfile::certs(&mut std::io::BufReader::new(cert_file))
                    .collect::<Result<_, _>>()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let key_file = std::fs::File::open(kp)?;
            let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_file))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "no private key found in file")
                })?;
            (certs, key)
        }
        (None, None) => {
            warn!(
                "no TLS cert/key provided — generating ephemeral self-signed certificate (not for production)"
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
/// Enforces authentication (if credentials are provided) as the very first
/// exchange before any query is processed.
fn run_query_loop<S: Read + Write>(
    reader: &mut BufReader<S>,
    peer: std::net::SocketAddr,
    exec: &Arc<RwLock<Executor>>,
    credentials: Option<&Credentials>,
) -> io::Result<()> {
    let mut authenticated = credentials.is_none();
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

        if !authenticated {
            match (parse_auth_line(trimmed), credentials) {
                (Some((u, p)), Some(creds)) if creds.verify(&u, &p) => {
                    reader.get_mut().write_all(b"OK\n")?;
                    reader.get_mut().flush()?;
                    authenticated = true;
                }
                _ => {
                    warn!(%peer, "authentication failed");
                    reader.get_mut().write_all(b"ERR authentication failed\n")?;
                    reader.get_mut().flush()?;
                    break;
                }
            }
            continue;
        }

        if trimmed.eq_ignore_ascii_case("QUIT") || trimmed.eq_ignore_ascii_case("EXIT") {
            reader.get_mut().write_all(b"OK BYE\n")?;
            reader.get_mut().flush()?;
            break;
        }

        let response = handle_query(trimmed, exec);
        let response_line = format!("{response}\n");
        reader.get_mut().write_all(response_line.as_bytes())?;
        reader.get_mut().flush()?;
    }

    info!(%peer, "client disconnected");
    Ok(())
}

fn handle(
    stream: TcpStream,
    exec: Arc<RwLock<Executor>>,
    tls_config: Option<Arc<ServerConfig>>,
    credentials: Option<Arc<Credentials>>,
) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    stream.set_nodelay(true)?;
    info!(%peer, "client connected");

    let creds = credentials.as_deref();

    if let Some(cfg) = tls_config {
        let conn = ServerConnection::new(cfg).map_err(io::Error::other)?;
        let tls_stream = rustls::StreamOwned::new(conn, stream);
        let mut reader = BufReader::new(tls_stream);
        run_query_loop(&mut reader, peer, &exec, creds)
    } else {
        let mut reader = BufReader::new(stream);
        run_query_loop(&mut reader, peer, &exec, creds)
    }
}

/// Parse + dispatch one query line.  Returns the response line (without the
/// trailing newline).  Lock-routing: read-only statements take the shared
/// lock; everything else takes the exclusive lock.
fn handle_query(query: &str, exec: &Arc<RwLock<Executor>>) -> String {
    let stmt = match parse(query) {
        Ok((rest, s)) if rest.trim().is_empty() => s,
        Ok((rest, _)) => return format!("ERR trailing input: {rest:?}"),
        Err(e) => return format!("ERR parse: {e}"),
    };
    let result = if stmt.is_read_only() {
        exec.read().unwrap().exec_read(&stmt)
    } else {
        exec.write().unwrap().exec(&stmt)
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tau::Stmt;

    fn exec() -> Arc<RwLock<Executor>> {
        Arc::new(RwLock::new(Executor::new()))
    }

    #[test]
    fn empty_response_for_ddl() {
        let e = exec();
        assert_eq!(handle_query("CREATE DATABASE main", &e), "OK");
        assert_eq!(handle_query("CREATE LENS x int", &e), "OK");
    }

    #[test]
    fn value_response_round_trips_through_codec() {
        let e = exec();
        handle_query("CREATE DATABASE main", &e);
        handle_query("CREATE LENS x int", &e);
        handle_query("APPEND LENS x 0 10 42", &e);
        assert_eq!(handle_query("AT LENS x 5", &e), "VAL i42");
    }

    #[test]
    fn nil_response_for_uncovered_lookup() {
        let e = exec();
        handle_query("CREATE DATABASE main", &e);
        handle_query("CREATE LENS x int", &e);
        assert_eq!(handle_query("AT LENS x 5", &e), "VAL NIL");
    }

    #[test]
    fn range_response_lists_segments() {
        let e = exec();
        handle_query("CREATE DATABASE main", &e);
        handle_query("CREATE LENS x int", &e);
        handle_query("APPEND LENS x 0 5 1", &e);
        handle_query("APPEND LENS x 5 10 2", &e);
        assert_eq!(
            handle_query("RANGE LENS x 0 10", &e),
            "RANGE 2; 0:5:i1; 5:10:i2"
        );
    }

    #[test]
    fn empty_range_response_has_zero_count() {
        let e = exec();
        handle_query("CREATE DATABASE main", &e);
        handle_query("CREATE LENS x int", &e);
        assert_eq!(handle_query("RANGE LENS x 0 10", &e), "RANGE 0");
    }

    #[test]
    fn parse_error_is_reported() {
        let e = exec();
        let r = handle_query("BOGUS QUERY", &e);
        assert!(r.starts_with("ERR parse:"), "got: {r}");
    }

    #[test]
    fn trailing_input_is_rejected() {
        let e = exec();
        let r = handle_query("CREATE DATABASE a JUNK", &e);
        assert!(r.starts_with("ERR trailing input"), "got: {r}");
    }

    #[test]
    fn execution_error_is_reported() {
        let e = exec();
        let r = handle_query("CREATE LENS x int", &e);
        assert!(r.starts_with("ERR no active database"), "got: {r}");
    }

    #[test]
    fn read_only_router_picks_shared_lock() {
        let e = exec();
        handle_query("CREATE DATABASE main", &e);
        handle_query("CREATE LENS x int", &e);
        handle_query("APPEND LENS x 0 100 7", &e);

        let mut handles = vec![];
        for _ in 0..8 {
            let e = e.clone();
            handles.push(thread::spawn(move || {
                assert_eq!(handle_query("AT LENS x 50", &e), "VAL i7");
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
        let res = guard.exec_read(&stmt);
        assert!(matches!(res, Err(ExecError::InvalidExpr(_))));
    }

    #[test]
    fn parse_auth_line_splits_user_and_pass() {
        let result = parse_auth_line("AUTH admin s3cr3t");
        assert_eq!(result, Some(("admin".into(), "s3cr3t".into())));
    }

    #[test]
    fn parse_auth_line_returns_none_for_non_auth() {
        assert_eq!(parse_auth_line("CREATE DATABASE x"), None);
        assert_eq!(parse_auth_line("AUTH"), None);
        assert_eq!(parse_auth_line(""), None);
    }
}
