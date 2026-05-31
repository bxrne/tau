//! Background I/O thread for the TUI.
//!
//! The render loop must never block.  This module runs a dedicated thread that
//! owns the `TcpManager` and handles all blocking socket I/O.  The TUI sends
//! requests through `tx_req` and receives responses through `rx_resp`.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use libtau::Response;

use crate::tcpmgr::TcpManager;

/// A request from the TUI to the I/O thread.
#[derive(Debug)]
#[allow(dead_code)]
pub enum IoRequest {
    /// Run a TauQL statement or built-in command on the active connection.
    Query(String),
    /// Connect to a new server: (name, addr, tls).
    Connect {
        name: String,
        addr: String,
        tls: bool,
    },
    /// Disconnect a named connection.
    Disconnect(String),
    /// Switch the active connection.
    Use(String),
    /// Terminate the I/O thread.
    Quit,
}

/// A response from the I/O thread back to the TUI.
#[derive(Debug)]
pub enum IoResponse {
    /// A successful wire response.
    Response(Response),
    /// An I/O or connection error.
    Error(String),
    /// Informational status update (e.g. "connected to ...").
    Info(String),
    /// The list of connections changed (re-render the connection pane).
    Connections(Vec<(String, String, bool, bool)>),
}

pub struct NetHandle {
    pub tx: Sender<IoRequest>,
    pub rx: Receiver<IoResponse>,
}

/// Spawn the background I/O thread.  Returns handles to communicate with it.
pub fn spawn() -> NetHandle {
    let (tx_req, rx_req) = mpsc::channel::<IoRequest>();
    let (tx_resp, rx_resp) = mpsc::channel::<IoResponse>();

    thread::spawn(move || io_thread(rx_req, tx_resp));

    NetHandle {
        tx: tx_req,
        rx: rx_resp,
    }
}

fn io_thread(rx: Receiver<IoRequest>, tx: Sender<IoResponse>) {
    let mut mgr = TcpManager::new();

    for req in rx {
        match req {
            IoRequest::Quit => break,

            IoRequest::Connect { name, addr, tls } => {
                let result = if tls {
                    let sni = addr.split(':').next().unwrap_or("localhost").to_string();
                    mgr.connect_tls(&name, &addr, &sni)
                } else {
                    mgr.connect(&name, &addr)
                };
                match result {
                    Ok(()) => {
                        let _ = tx.send(IoResponse::Info(format!(
                            "connected to {addr} as {name} ({})",
                            if tls { "TLS" } else { "plain" }
                        )));
                        let _ = tx.send(IoResponse::Connections(mgr.list()));
                    }
                    Err(e) => {
                        let _ = tx.send(IoResponse::Error(format!("connect: {e}")));
                    }
                }
            }

            IoRequest::Disconnect(name) => match mgr.disconnect(&name) {
                Ok(()) => {
                    let _ = tx.send(IoResponse::Info(format!("disconnected {name}")));
                    let _ = tx.send(IoResponse::Connections(mgr.list()));
                }
                Err(e) => {
                    let _ = tx.send(IoResponse::Error(e));
                }
            },

            IoRequest::Use(name) => match mgr.set_active(&name) {
                Ok(()) => {
                    let _ = tx.send(IoResponse::Connections(mgr.list()));
                }
                Err(e) => {
                    let _ = tx.send(IoResponse::Error(e));
                }
            },

            IoRequest::Query(line) => {
                if let Some(conn) = mgr.active_mut() {
                    match conn.send(&line) {
                        Ok(resp) => {
                            let _ = tx.send(IoResponse::Response(resp));
                        }
                        Err(e) => {
                            let _ = tx.send(IoResponse::Error(format!("io: {e}")));
                        }
                    }
                } else {
                    let _ = tx.send(IoResponse::Error(
                        "no active connection — use `connect <name> <host:port>`".into(),
                    ));
                }
            }
        }
    }
}
