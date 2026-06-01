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
    /// Bulk-load a local CSV into a lens.
    Load {
        lens: String,
        path: String,
        chunk: usize,
    },
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
    /// A multi-step operation finished successfully (clears pending).
    Done(String),
}

pub struct NetHandle {
    pub tx: Sender<IoRequest>,
    pub rx: Receiver<IoResponse>,
}

impl Drop for NetHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(IoRequest::Quit);
    }
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

fn handle_connect(
    mgr: &mut TcpManager,
    name: String,
    addr: String,
    tls: bool,
    tx: &Sender<IoResponse>,
) {
    let result = if tls {
        let sni = addr.split(':').next().unwrap_or("localhost").to_string();
        mgr.connect_tls(&name, &addr, &sni)
    } else {
        mgr.connect(&name, &addr)
    };
    match result {
        Ok(()) => {
            let mode = if tls { "TLS" } else { "plain" };
            let _ = tx.send(IoResponse::Info(format!(
                "connected to {addr} as {name} ({mode})"
            )));
            let _ = tx.send(IoResponse::Connections(mgr.list()));
        }
        Err(e) => {
            let _ = tx.send(IoResponse::Error(format!("connect: {e}")));
        }
    }
}

fn handle_disconnect(mgr: &mut TcpManager, name: String, tx: &Sender<IoResponse>) {
    match mgr.disconnect(&name) {
        Ok(()) => {
            let _ = tx.send(IoResponse::Info(format!("disconnected {name}")));
            let _ = tx.send(IoResponse::Connections(mgr.list()));
        }
        Err(e) => {
            let _ = tx.send(IoResponse::Error(e));
        }
    }
}

fn handle_use(mgr: &mut TcpManager, name: String, tx: &Sender<IoResponse>) {
    match mgr.set_active(&name) {
        Ok(()) => {
            let _ = tx.send(IoResponse::Connections(mgr.list()));
        }
        Err(e) => {
            let _ = tx.send(IoResponse::Error(e));
        }
    }
}

fn handle_query(mgr: &mut TcpManager, line: String, tx: &Sender<IoResponse>) {
    match mgr.active_mut() {
        Some(conn) => match conn.send(&line) {
            Ok(resp) => {
                let _ = tx.send(IoResponse::Response(resp));
            }
            Err(e) => {
                let _ = tx.send(IoResponse::Error(format!("io: {e}")));
            }
        },
        None => {
            let _ = tx.send(IoResponse::Error(
                "no active connection — use `connect <name> <host:port>`".into(),
            ));
        }
    }
}

fn handle_load(
    mgr: &mut TcpManager,
    lens: String,
    path: String,
    chunk: usize,
    tx: &Sender<IoResponse>,
) {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    use crate::commands::{flush_chunk, parse_csv_line};

    let conn = match mgr.active_mut() {
        Some(c) => c,
        None => {
            let _ = tx.send(IoResponse::Error(
                "no active connection — use `connect <name> <host:port>`".into(),
            ));
            return;
        }
    };

    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.send(IoResponse::Error(format!("open {path}: {e}")));
            return;
        }
    };

    match conn.send("START TRANSACTION") {
        Err(e) => {
            let _ = tx.send(IoResponse::Error(format!("io: {e}")));
            return;
        }
        Ok(r) if r.is_err() => {
            let _ = tx.send(IoResponse::Error(format!("START TRANSACTION: {r}")));
            return;
        }
        Ok(_) => {}
    }

    let reader = BufReader::new(file);
    let mut buffer: Vec<String> = Vec::with_capacity(chunk);
    let mut total: u64 = 0;
    let mut chunks_sent: u64 = 0;
    let mut load_err: Option<String> = None;

    'rows: for (lineno, raw) in reader.lines().enumerate() {
        let raw = match raw {
            Ok(r) => r,
            Err(e) => {
                load_err = Some(format!("read {path} line {}: {e}", lineno + 1));
                break 'rows;
            }
        };
        match parse_csv_line(&raw, &path, lineno) {
            Err(e) => {
                load_err = Some(e);
                break 'rows;
            }
            Ok(None) => continue,
            Ok(Some(triple)) => buffer.push(triple),
        }
        if buffer.len() >= chunk
            && let Err(e) = flush_chunk(conn, &lens, &mut buffer, &mut total, &mut chunks_sent)
        {
            load_err = Some(e);
            break 'rows;
        }
    }

    if load_err.is_none()
        && let Err(e) = flush_chunk(conn, &lens, &mut buffer, &mut total, &mut chunks_sent)
    {
        load_err = Some(e);
    }

    if let Some(e) = load_err {
        let _ = conn.send("ROLLBACK");
        let _ = tx.send(IoResponse::Error(e));
        return;
    }

    match conn.send("COMMIT") {
        Err(e) => {
            let _ = tx.send(IoResponse::Error(format!("io: {e}")));
            return;
        }
        Ok(r) if r.is_err() => {
            let _ = tx.send(IoResponse::Error(format!("COMMIT: {r}")));
            return;
        }
        Ok(_) => {}
    }

    let s = format!(
        "loaded {} rows into {} ({} chunk{})",
        total,
        lens,
        chunks_sent,
        if chunks_sent == 1 { "" } else { "s" }
    );
    let _ = tx.send(IoResponse::Done(s));
}

fn io_thread(rx: Receiver<IoRequest>, tx: Sender<IoResponse>) {
    let mut mgr = TcpManager::new();
    for req in rx {
        match req {
            IoRequest::Quit => break,
            IoRequest::Connect { name, addr, tls } => {
                handle_connect(&mut mgr, name, addr, tls, &tx)
            }
            IoRequest::Disconnect(name) => handle_disconnect(&mut mgr, name, &tx),
            IoRequest::Use(name) => handle_use(&mut mgr, name, &tx),
            IoRequest::Query(line) => handle_query(&mut mgr, line, &tx),
            IoRequest::Load { lens, path, chunk } => handle_load(&mut mgr, lens, path, chunk, &tx),
        }
    }
}
