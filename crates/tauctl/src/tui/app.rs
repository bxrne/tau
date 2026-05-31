//! TUI application state.

use libtau::Response;

use super::net::{IoRequest, IoResponse, NetHandle};

/// One entry in the query log pane.
#[derive(Debug, Clone)]
pub struct LogEntry {
    #[allow(dead_code)]
    pub query: String,
    pub response: String,
    #[allow(dead_code)]
    pub elapsed_ms: u64,
    pub is_err: bool,
}

pub struct App {
    pub net: NetHandle,
    /// Rendered connection list: (name, addr, is_active, is_tls).
    pub connections: Vec<(String, String, bool, bool)>,
    /// Query/response log shown in the log pane.
    pub log: Vec<LogEntry>,
    /// Last full response for the results pane.
    pub last_response: Option<Response>,
    /// Running query (waiting for I/O response).
    pub pending: bool,
    /// Status bar message.
    pub status: String,
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            net: super::net::spawn(),
            connections: vec![],
            log: vec![],
            last_response: None,
            pending: false,
            status: "Ready".into(),
            should_quit: false,
        }
    }

    /// Submit a query line.  Called by the input handler; does not block.
    pub fn submit(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        // Intercept built-in TUI commands without going to the server.
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["connect", name, addr] => {
                self.send_io(IoRequest::Connect {
                    name: name.to_string(),
                    addr: addr.to_string(),
                    tls: false,
                });
            }
            ["connect", name, addr, "tls"] => {
                self.send_io(IoRequest::Connect {
                    name: name.to_string(),
                    addr: addr.to_string(),
                    tls: true,
                });
            }
            ["disconnect", name] => {
                self.send_io(IoRequest::Disconnect(name.to_string()));
            }
            ["use", name] => {
                self.send_io(IoRequest::Use(name.to_string()));
            }
            ["exit"] | ["quit"] => {
                self.should_quit = true;
            }
            _ => {
                self.pending = true;
                self.status = "sending…".into();
                self.send_io(IoRequest::Query(line));
            }
        }
    }

    fn send_io(&self, req: IoRequest) {
        let _ = self.net.tx.send(req);
    }

    /// Drain all pending I/O responses into app state.  Call every render tick.
    pub fn drain(&mut self) {
        use std::sync::mpsc::TryRecvError;
        loop {
            match self.net.rx.try_recv() {
                Ok(msg) => self.handle_io(msg),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.status = "I/O thread disconnected".into();
                    break;
                }
            }
        }
    }

    fn handle_io(&mut self, msg: IoResponse) {
        match msg {
            IoResponse::Connections(list) => {
                self.connections = list;
            }
            IoResponse::Info(s) => {
                self.status = s.clone();
                self.log.push(LogEntry {
                    query: String::new(),
                    response: s,
                    elapsed_ms: 0,
                    is_err: false,
                });
            }
            IoResponse::Error(e) => {
                self.pending = false;
                self.status = format!("ERR {e}");
                self.log.push(LogEntry {
                    query: String::new(),
                    response: format!("ERR {e}"),
                    elapsed_ms: 0,
                    is_err: true,
                });
            }
            IoResponse::Response(resp) => {
                self.pending = false;
                let is_err = resp.is_err();
                let text = resp.to_string();
                self.status = if is_err { text.clone() } else { "OK".into() };
                self.last_response = Some(resp);
                self.log.push(LogEntry {
                    query: String::new(),
                    response: text,
                    elapsed_ms: 0,
                    is_err,
                });
            }
        }
    }
}
