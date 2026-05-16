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
//! Set `TAU_ENCRYPTION_KEY` (64 hex chars = 32 bytes) for AES-256-GCM
//! at-rest WAL encryption.

use std::collections::HashMap;
use std::io::{self};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use clap::Parser;
use libtau::{Executor, Perm, User, UserStore, crypto};
use tracing::{debug, info, warn};

mod config;
mod handler;
mod metrics;
mod server;

use config::{BackendChoice, Config, load_config};
use metrics::serve_metrics_http;
use server::{accept_loop, build_tls_config};

/// Tau time-series database TCP server.
#[derive(Parser, Debug)]
#[command(name = "tau", author, version)]
#[command(about = "A time-series database TCP server", long_about = None)]
struct Cli {
    /// Path to config.toml. Defaults to ./config.toml if present; uses built-in defaults otherwise.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
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
                if let Some(mb) = config.wal.max_size_mb {
                    e.set_wal_max_bytes(mb * 1024 * 1024);
                    info!(max_size_mb = mb, "WAL size cap configured");
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
    executor
        .write()
        .expect("executor lock poisoned")
        .set_users(store);
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

    let tls_config: Option<Arc<rustls::ServerConfig>> = if config.tls.enabled {
        let server_cfg = build_tls_config(config.tls.cert.as_deref(), config.tls.key.as_deref())?;
        info!(cert = ?config.tls.cert, key = ?config.tls.key, "TLS enabled");
        Some(Arc::new(server_cfg))
    } else {
        debug!("TLS disabled (plain TCP)");
        None
    };

    if let Some(port) = config.metrics.port {
        let metrics = executor.read().expect("executor lock poisoned").metrics();
        thread::Builder::new()
            .name("tau-metrics".into())
            .spawn(move || serve_metrics_http(port, metrics))
            .expect("failed to spawn metrics thread");
        info!(port, "metrics endpoint enabled");
    }

    let bind = config.bind.clone();
    let listener = TcpListener::bind(&bind)?;
    info!(%bind, "tau server listening");

    let idle_timeout = config.limits.idle_timeout_secs.map(Duration::from_secs);

    accept_loop(
        listener,
        executor,
        tls_config,
        auth_enabled,
        config.limits.max_connections,
        idle_timeout,
    )
}
