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

pub(super) struct NetHandle {
    tx: Sender<IoRequest>,
    rx: Receiver<IoResponse>,
}

impl NetHandle {
    pub(super) fn send(&self, req: IoRequest) {
        let _ = self.tx.send(req);
    }

    pub(super) fn try_recv(&self) -> Result<IoResponse, std::sync::mpsc::TryRecvError> {
        self.rx.try_recv()
    }

    #[cfg(test)]
    pub(super) fn test_pair(tx: Sender<IoRequest>, rx: Receiver<IoResponse>) -> Self {
        Self { tx, rx }
    }
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    fn ok_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        let _ = writeln!(reader.get_mut(), "OK");
                        let _ = reader.get_mut().flush();
                        line.clear();
                    }
                });
            }
        });
        addr
    }

    #[test]
    fn handle_load_no_active_connection_sends_error() {
        let mut mgr = TcpManager::new();
        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_load(&mut mgr, "x".into(), "/tmp/x.csv".into(), 256, &tx);
        assert!(matches!(rx.recv().unwrap(), IoResponse::Error(_)));
    }

    #[test]
    fn handle_load_missing_file_sends_error() {
        let addr = ok_server();
        let mut mgr = TcpManager::new();
        mgr.connect("t", &addr.to_string()).unwrap();
        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_load(
            &mut mgr,
            "x".into(),
            "/tmp/tauctl-no-such-file-xyz123.csv".into(),
            256,
            &tx,
        );
        let resp = rx.recv().unwrap();
        assert!(matches!(resp, IoResponse::Error(_)));
        if let IoResponse::Error(e) = resp {
            assert!(e.contains("open"), "expected file error, got: {e}");
        }
    }

    #[test]
    fn handle_load_success_sends_done_with_row_count() {
        let addr = ok_server();
        let mut mgr = TcpManager::new();
        mgr.connect("t", &addr.to_string()).unwrap();

        let path = format!("/tmp/tauctl-load-test-{}.csv", std::process::id());
        std::fs::write(&path, "0,1,42\n1,2,43\n2,3,44\n").unwrap();

        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_load(&mut mgr, "x".into(), path.clone(), 256, &tx);
        std::fs::remove_file(&path).ok();

        let resp = rx.recv().unwrap();
        assert!(matches!(resp, IoResponse::Done(_)));
        if let IoResponse::Done(s) = resp {
            assert!(s.contains("3"), "expected row count, got: {s}");
            assert!(s.contains("x"), "expected lens name, got: {s}");
        }
    }

    #[test]
    fn handle_load_csv_with_comments_and_blanks() {
        let addr = ok_server();
        let mut mgr = TcpManager::new();
        mgr.connect("t", &addr.to_string()).unwrap();

        let path = format!("/tmp/tauctl-load-comments-{}.csv", std::process::id());
        std::fs::write(&path, "# comment\n0,1,10\n\n1,2,20\n").unwrap();

        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_load(&mut mgr, "y".into(), path.clone(), 256, &tx);
        std::fs::remove_file(&path).ok();

        let resp = rx.recv().unwrap();
        assert!(matches!(resp, IoResponse::Done(_)));
        if let IoResponse::Done(s) = resp {
            assert!(s.contains("2"), "expected 2 data rows, got: {s}");
        }
    }

    #[test]
    fn handle_connect_failure_sends_error() {
        let mut mgr = TcpManager::new();
        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_connect(&mut mgr, "bad".into(), "127.0.0.1:1".into(), false, &tx);
        assert!(matches!(rx.recv().unwrap(), IoResponse::Error(_)));
    }

    #[test]
    fn handle_disconnect_unknown_sends_error() {
        let mut mgr = TcpManager::new();
        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_disconnect(&mut mgr, "nobody".into(), &tx);
        assert!(matches!(rx.recv().unwrap(), IoResponse::Error(_)));
    }

    #[test]
    fn handle_use_unknown_sends_error() {
        let mut mgr = TcpManager::new();
        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_use(&mut mgr, "nobody".into(), &tx);
        assert!(matches!(rx.recv().unwrap(), IoResponse::Error(_)));
    }

    #[test]
    fn handle_query_no_active_connection_sends_error() {
        let mut mgr = TcpManager::new();
        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_query(&mut mgr, "AT LENS x 0".into(), &tx);
        assert!(matches!(rx.recv().unwrap(), IoResponse::Error(_)));
    }

    #[test]
    fn handle_connect_success_sends_info_and_connections() {
        let addr = ok_server();
        let mut mgr = TcpManager::new();
        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_connect(&mut mgr, "dev".into(), addr.to_string(), false, &tx);
        let first = rx.recv().unwrap();
        assert!(matches!(first, IoResponse::Info(_)));
        let second = rx.recv().unwrap();
        assert!(matches!(second, IoResponse::Connections(_)));
    }

    #[test]
    fn handle_use_known_connection_sends_connections() {
        let addr = ok_server();
        let mut mgr = TcpManager::new();
        mgr.connect("dev", &addr.to_string()).unwrap();
        let (tx, rx) = mpsc::channel::<IoResponse>();
        handle_use(&mut mgr, "dev".into(), &tx);
        assert!(matches!(rx.recv().unwrap(), IoResponse::Connections(_)));
    }
}
