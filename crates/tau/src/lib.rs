//! Tau server library (TCP/TLS query service). The `tau` binary is a thin wrapper.

pub mod config;
pub mod handler;
pub mod harness;
pub mod metrics;
pub mod server;

use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use clap::Parser;
use libtau::{Executor, Perm, User, UserStore, crypto};
use tracing::{info, warn};

use crate::config::{BackendChoice, Config, load_config};
use metrics::serve_metrics_http;
use server::{accept_loop, build_tls_config};

/// CLI for the `tau` binary.
#[derive(Parser, Debug)]
#[command(name = "tau", author, version)]
#[command(about = "A time-series database TCP server", long_about = None)]
pub struct Cli {
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

pub fn build_executor(
    config: &Config,
    enc_key: Option<[u8; 32]>,
) -> io::Result<Arc<RwLock<Executor>>> {
    let exec = match config.disk.backend {
        BackendChoice::Disk => {
            let dir = config.disk.path.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "[disk] backend = \"disk\" requires a path (add path = \"...\" under [disk])",
                )
            })?;
            if config.wal.enabled && config.wal.path.is_some() {
                warn!(
                    "disk backend is active; [wal].path is ignored - each database gets its own \
                     <dir>/<name>.wal alongside its manifest/run files"
                );
            }
            info!(
                dir = %dir.display(),
                compression_level = config.disk.compression_level,
                compact_threshold = config.compact_threshold,
                "starting with disk backend (each database durably WAL-backed)"
            );
            Executor::with_disk_backend(
                dir,
                config.compact_threshold,
                config.disk.compression_level,
                enc_key,
                !config.wal.no_fsync_each,
                config.wal.max_size_mb.map(|mb| mb * 1024 * 1024),
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
                    e.set_wal_fsync_each(false);
                }
                if let Some(mb) = config.wal.max_size_mb {
                    e.set_wal_max_bytes(mb * 1024 * 1024);
                }
                e
            } else {
                Executor::with_threshold(config.compact_threshold)
            }
        }
    };

    Ok(Arc::new(RwLock::new(exec)))
}

pub fn setup_auth(config: &Config, executor: &Arc<RwLock<Executor>>) -> io::Result<()> {
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
    }
    if store.names().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "[auth] enabled = true but no users configured",
        ));
    }
    executor
        .write()
        .expect("executor lock poisoned")
        .set_users(store);
    Ok(())
}

/// Run the production TCP server (blocks until the listener is closed).
pub fn run_server(cli: Cli) -> io::Result<()> {
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

    let executor = build_executor(&config, enc_key)?;

    if config.auth.enabled {
        setup_auth(&config, &executor)?;
    }

    let tls_config: Option<Arc<rustls::ServerConfig>> = if config.tls.enabled {
        Some(Arc::new(build_tls_config(
            config.tls.cert.as_deref(),
            config.tls.key.as_deref(),
        )?))
    } else {
        None
    };

    if let Some(port) = config.metrics.port {
        let metrics = executor.read().expect("executor lock poisoned").metrics();
        thread::Builder::new()
            .name("tau-metrics".into())
            .spawn(move || serve_metrics_http(port, metrics))
            .expect("failed to spawn metrics thread");
    }

    let listener = TcpListener::bind(&config.bind)?;
    info!(bind = %config.bind, "tau server listening");

    accept_loop(
        listener,
        executor,
        tls_config,
        config.auth.enabled,
        config.limits.max_connections,
        config.limits.idle_timeout_secs.map(Duration::from_secs),
    )
}
