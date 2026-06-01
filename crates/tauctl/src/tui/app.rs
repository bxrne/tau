//! TUI application state.

use libtau::Response;

use super::net::{IoRequest, IoResponse, NetHandle};

/// One entry in the query log pane.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub query: String,
    pub response: String,
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
            ["load", lens, path] => {
                self.pending = true;
                self.status = "loading…".into();
                self.send_io(IoRequest::Load {
                    lens: lens.to_string(),
                    path: path.to_string(),
                    chunk: 256,
                });
            }
            ["load", lens, path, chunk_str] => match chunk_str.parse::<usize>() {
                Ok(chunk) if chunk > 0 => {
                    self.pending = true;
                    self.status = "loading…".into();
                    self.send_io(IoRequest::Load {
                        lens: lens.to_string(),
                        path: path.to_string(),
                        chunk,
                    });
                }
                _ => {
                    let msg = format!("invalid chunk size {chunk_str:?}");
                    self.status = msg.clone();
                    self.log.push(LogEntry {
                        query: String::new(),
                        response: msg,
                        is_err: true,
                    });
                }
            },
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
                    is_err: false,
                });
            }
            IoResponse::Error(e) => {
                self.pending = false;
                self.status = format!("ERR {e}");
                self.log.push(LogEntry {
                    query: String::new(),
                    response: format!("ERR {e}"),
                    is_err: true,
                });
            }
            IoResponse::Done(s) => {
                self.pending = false;
                self.status = s.clone();
                self.log.push(LogEntry {
                    query: String::new(),
                    response: s,
                    is_err: false,
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
                    is_err,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use libtau::Response;
    use pretty_assertions::assert_eq;

    use super::*;

    fn test_app() -> (App, mpsc::Sender<IoResponse>) {
        let (tx_req, _rx_req) = mpsc::channel::<IoRequest>();
        let (tx_resp, rx_resp) = mpsc::channel::<IoResponse>();
        let app = App {
            net: NetHandle {
                tx: tx_req,
                rx: rx_resp,
            },
            connections: vec![],
            log: vec![],
            last_response: None,
            pending: false,
            status: "Ready".into(),
            should_quit: false,
        };
        (app, tx_resp)
    }

    #[test]
    fn submit_empty_line_is_noop() {
        let (mut app, _tx) = test_app();
        app.submit(String::new());
        app.submit("   ".to_string());
        assert!(!app.pending);
        assert!(app.log.is_empty());
    }

    #[test]
    fn submit_load_sets_pending_and_status() {
        let (mut app, _tx) = test_app();
        app.submit("load pressure examples/data/pressure.csv".to_string());
        assert!(app.pending);
        assert_eq!(app.status, "loading…");
    }

    #[test]
    fn submit_load_with_explicit_chunk_sets_pending() {
        let (mut app, _tx) = test_app();
        app.submit("load cpu examples/data/cpu-load.csv 128".to_string());
        assert!(app.pending);
    }

    #[test]
    fn submit_load_with_invalid_chunk_logs_error() {
        let (mut app, _tx) = test_app();
        app.submit("load lens path notanumber".to_string());
        assert!(!app.pending);
        assert!(app.log.last().unwrap().is_err);
    }

    #[test]
    fn submit_load_with_zero_chunk_logs_error() {
        let (mut app, _tx) = test_app();
        app.submit("load lens path 0".to_string());
        assert!(!app.pending);
        assert!(app.log.last().unwrap().is_err);
    }

    #[test]
    fn submit_exit_sets_should_quit() {
        let (mut app, _tx) = test_app();
        app.submit("exit".to_string());
        assert!(app.should_quit);
    }

    #[test]
    fn submit_quit_sets_should_quit() {
        let (mut app, _tx) = test_app();
        app.submit("quit".to_string());
        assert!(app.should_quit);
    }

    #[test]
    fn submit_query_sets_pending_and_sending_status() {
        let (mut app, _tx) = test_app();
        app.submit("AT LENS x 42".to_string());
        assert!(app.pending);
        assert_eq!(app.status, "sending…");
    }

    #[test]
    fn handle_io_done_clears_pending_and_logs() {
        let (mut app, tx) = test_app();
        app.pending = true;
        tx.send(IoResponse::Done(
            "loaded 288 rows into pressure (2 chunks)".to_string(),
        ))
        .unwrap();
        app.drain();
        assert!(!app.pending);
        assert_eq!(app.status, "loaded 288 rows into pressure (2 chunks)");
        let entry = app.log.last().unwrap();
        assert_eq!(entry.response, "loaded 288 rows into pressure (2 chunks)");
        assert!(!entry.is_err);
    }

    #[test]
    fn handle_io_error_clears_pending_and_logs_error() {
        let (mut app, tx) = test_app();
        app.pending = true;
        tx.send(IoResponse::Error("connection refused".to_string()))
            .unwrap();
        app.drain();
        assert!(!app.pending);
        assert!(app.status.contains("ERR"));
        assert!(app.log.last().unwrap().is_err);
    }

    #[test]
    fn handle_io_info_updates_status_without_clearing_pending() {
        let (mut app, tx) = test_app();
        app.pending = true;
        tx.send(IoResponse::Info(
            "connected to 127.0.0.1:7070 as dev (plain)".to_string(),
        ))
        .unwrap();
        app.drain();
        assert!(app.pending, "info must not clear pending");
        assert_eq!(app.status, "connected to 127.0.0.1:7070 as dev (plain)");
        assert!(!app.log.last().unwrap().is_err);
    }

    #[test]
    fn handle_io_ok_response_clears_pending_and_sets_last_response() {
        let (mut app, tx) = test_app();
        app.pending = true;
        tx.send(IoResponse::Response(Response::Ok)).unwrap();
        app.drain();
        assert!(!app.pending);
        assert!(app.last_response.is_some());
        assert_eq!(app.status, "OK");
    }

    #[test]
    fn handle_io_err_response_clears_pending_and_sets_err_status() {
        let (mut app, tx) = test_app();
        app.pending = true;
        tx.send(IoResponse::Response(Response::Err(
            "no active database".to_string(),
        )))
        .unwrap();
        app.drain();
        assert!(!app.pending);
        assert!(app.log.last().unwrap().is_err);
    }

    #[test]
    fn handle_io_connections_updates_list() {
        let (mut app, tx) = test_app();
        tx.send(IoResponse::Connections(vec![
            ("dev".into(), "127.0.0.1:7070".into(), true, false),
            ("prod".into(), "10.0.0.1:7070".into(), false, true),
        ]))
        .unwrap();
        app.drain();
        assert_eq!(app.connections.len(), 2);
        assert_eq!(app.connections[0].0, "dev");
        assert!(app.connections[0].2, "first entry should be active");
        assert!(!app.connections[1].2, "second entry should not be active");
    }

    #[test]
    fn drain_processes_multiple_messages_in_order() {
        let (mut app, tx) = test_app();
        tx.send(IoResponse::Info("first".to_string())).unwrap();
        tx.send(IoResponse::Info("second".to_string())).unwrap();
        tx.send(IoResponse::Info("third".to_string())).unwrap();
        app.drain();
        assert_eq!(app.log.len(), 3);
        assert_eq!(app.status, "third");
    }

    #[test]
    fn log_entry_fields_are_accessible() {
        let e = LogEntry {
            query: "AT LENS x 0".into(),
            response: "VAL i42".into(),
            is_err: false,
        };
        assert_eq!(e.query, "AT LENS x 0");
        assert_eq!(e.response, "VAL i42");
        assert!(!e.is_err);
    }
}
