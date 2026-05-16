//! Tau TCP server.
//!
//! A line-oriented query service over plain TCP.  One statement per line in;
//! one response line out.  Designed to be friendly to `nc`, scriptable from
//! any language, and fast enough that the protocol is the bottleneck rather
//! than the server.
//!
//! # Optimisations
//!
//! * `TCP_NODELAY` on every accepted socket — disables Nagle so small
//!   single-statement requests aren't delayed.
//! * Buffered I/O on both directions per connection.
//! * Per-connection thread, sharing one [`Executor`] behind an [`RwLock`].
//!   `AT` and `RANGE` take the **read** lock and run concurrently; mutations
//!   take the **write** lock.
//! * `SO_REUSEADDR` so restarts don't hit "address already in use".
//!
//! # Wire format
//!
//! ```text
//! → CREATE DATABASE main
//! ← OK
//! → AT LENS x 5
//! ← VAL i42
//! → RANGE LENS x 0 10
//! ← RANGE 2; 0:5:i1; 5:10:i2
//! → BOGUS
//! ← ERR parse: ...
//! ```
//!
//! Values are encoded via the [`Codec`] trait: `i<int>`, `f<float>`,
//! `s<percent-escaped>`, `b<0|1>`, `n`.  Missing-value responses use the
//! sentinel `NIL`.
//!
//! `QUIT` / `EXIT` (case-insensitive) close the connection cleanly.

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;

use clap::Parser;
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
}

fn main() -> io::Result<()> {
    let config = Config::parse();

    // Initialize tracing with configured level
    let level = config.log_level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    let bind = config.bind.clone();
    let listener = TcpListener::bind(&bind)?;
    info!(%bind, "tau server listening");

    // Create executor based on configuration
    let executor: Arc<RwLock<Executor>> = if config.wal {
        let wal_path = config.wal_path.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL enabled but no path provided (use -w/--wal-path)",
            )
        })?;
        info!(wal_path = %wal_path.display(), "starting with in-memory store and WAL enabled");
        Arc::new(RwLock::new(Executor::with_wal(wal_path)?))
    } else {
        info!("starting with in-memory store (no WAL)");
        Arc::new(RwLock::new(Executor::new()))
    };

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let exec = executor.clone();
                // One thread per connection.  Connections are long-lived
                // pipelines of statements, so creation cost is amortised.
                thread::Builder::new()
                    .name("tau-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle(stream, exec) {
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

/// Drive a single client connection: read line, execute, write response,
/// repeat until the client disconnects or sends `QUIT`/`EXIT`.
fn handle(stream: TcpStream, exec: Arc<RwLock<Executor>>) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    stream.set_nodelay(true)?;
    info!(%peer, "client connected");

    let writer_stream = stream.try_clone()?;
    let reader = BufReader::new(stream);
    let mut writer = BufWriter::new(writer_stream);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                warn!(%peer, error = %e, "read error");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("QUIT") || trimmed.eq_ignore_ascii_case("EXIT") {
            writeln!(writer, "OK BYE")?;
            writer.flush()?;
            break;
        }

        let response = handle_query(trimmed, &exec);
        writeln!(writer, "{response}")?;
        writer.flush()?;
    }

    info!(%peer, "client disconnected");
    Ok(())
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
            // "RANGE <n>" followed by "; <s>:<e>:<codec>" per segment.
            let mut out = String::with_capacity(16 + segments.len() * 24);
            out.push_str(&format!("RANGE {}", segments.len()));
            for (s, e, v) in segments {
                out.push_str(&format!("; {}:{}:{}", s, e, v.encode()));
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
        } => format!("type mismatch on {lens}: expected {expected:?}, got {got}"),
        ExecError::InvalidExpr(m) => format!("invalid expression: {m}"),
        ExecError::InvalidRange => "invalid range (start >= end)".into(),
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
        // Smoke-check: a write then concurrent reads complete.
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
}
