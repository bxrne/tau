//! Ephemeral tau server for deterministic simulation tests.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use libtau::Kernel;
use rustls::ServerConfig;

use crate::server::build_tls_config;

/// Options for an in-process test listener.
#[derive(Debug, Clone, Copy)]
pub struct HarnessOpts {
    pub tls: bool,
    pub auth: bool,
    pub connection_limit: usize,
}

impl Default for HarnessOpts {
    fn default() -> Self {
        Self {
            tls: false,
            auth: false,
            connection_limit: 64,
        }
    }
}

/// Background tau server bound to `127.0.0.1:0`.
pub struct EphemeralServer {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl EphemeralServer {
    pub fn spawn(kernel: Arc<Kernel>, opts: HarnessOpts) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true).expect("set_nonblocking");

        let tls_config: Option<Arc<ServerConfig>> = if opts.tls {
            Some(Arc::new(build_tls_config(None, None)?))
        } else {
            None
        };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let join = thread::Builder::new()
            .name("tau-dst-srv".into())
            .spawn(move || {
                loop {
                    if stop_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            let exec = kernel.clone();
                            let tls = tls_config.clone();
                            thread::Builder::new()
                                .name("tau-dst-conn".into())
                                .spawn(move || {
                                    let _ = crate::server::handle_connection(
                                        stream, peer, exec, tls, opts.auth, None,
                                    );
                                })
                                .expect("spawn conn");
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn server");

        Ok(Self {
            addr,
            stop,
            join: Some(join),
        })
    }
}

impl Drop for EphemeralServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
