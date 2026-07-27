//! TUI application state.

use libtau::Response;

use super::net::{IoRequest, IoResponse, NetHandle};

/// One entry in the query log pane.
#[derive(Debug, Clone)]
pub(super) struct LogEntry {
    pub(super) query: String,
    pub(super) response: String,
    pub(super) is_err: bool,
}

/// Which pane currently has keyboard focus.  `Input` is the editing prompt
/// (the default); the other three are read-only panes you navigate with
/// lazygit-style number keys.  Each non-input variant carries the digit shown
/// in its title badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    Connections,
    Results,
    Log,
}

impl Focus {
    /// The pane bound to number key `n` (1-3), if any.
    pub fn from_digit(n: u32) -> Option<Self> {
        match n {
            1 => Some(Focus::Connections),
            2 => Some(Focus::Results),
            3 => Some(Focus::Log),
            _ => None,
        }
    }

    /// The badge digit rendered in this pane's title, if it is navigable.
    pub fn digit(self) -> Option<u8> {
        match self {
            Focus::Connections => Some(1),
            Focus::Results => Some(2),
            Focus::Log => Some(3),
            Focus::Input => None,
        }
    }
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
    /// Pane with keyboard focus.
    pub focus: Focus,
    /// Highlighted row in the connections pane (when focused).
    pub conn_sel: usize,
    /// Scroll offset (rows from the top) for the log pane when focused.
    pub log_scroll: u16,
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
            focus: Focus::Input,
            conn_sel: 0,
            log_scroll: 0,
        }
    }

    /// Move focus to `target`.  Resets per-pane navigation state so a freshly
    /// focused pane starts at a sensible position.
    pub fn focus_pane(&mut self, target: Focus) {
        self.focus = target;
        match target {
            Focus::Connections => {
                self.conn_sel = self.conn_sel.min(self.connections.len().saturating_sub(1))
            }
            Focus::Log => self.log_scroll = 0,
            _ => {}
        }
    }

    /// Plain-text rendering of a pane, used for clipboard copy.
    pub fn pane_text(&self, focus: Focus) -> String {
        match focus {
            Focus::Connections => self
                .connections
                .iter()
                .map(|(name, addr, active, tls)| {
                    let marker = if *active { "* " } else { "  " };
                    let tls = if *tls { " [tls]" } else { "" };
                    format!("{marker}{name}  {addr}{tls}")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Focus::Results | Focus::Input => self
                .last_response
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            Focus::Log => self
                .log
                .iter()
                .map(|e| {
                    if e.query.is_empty() {
                        e.response.clone()
                    } else {
                        format!("{} -> {}", e.query, e.response)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// Move the connection selection by `delta`, saturating at the ends.
    pub fn select_conn(&mut self, delta: isize) {
        if self.connections.is_empty() {
            return;
        }
        let last = self.connections.len() - 1;
        self.conn_sel = (self.conn_sel as isize + delta).clamp(0, last as isize) as usize;
    }

    /// Activate (`USE`) the currently highlighted connection.
    pub fn activate_selected_conn(&mut self) {
        if let Some((name, ..)) = self.connections.get(self.conn_sel) {
            self.send_io(IoRequest::Use(name.clone()));
        }
    }

    /// Scroll the log pane by `delta` rows, saturating at the top.
    pub fn scroll_log(&mut self, delta: isize) {
        let next = (self.log_scroll as isize + delta).max(0);
        self.log_scroll = next as u16;
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
            ["import", "csv", lens, path] => {
                self.pending = true;
                self.status = "loading…".into();
                self.send_io(IoRequest::ImportCsv {
                    lens: lens.to_string(),
                    path: path.to_string(),
                    chunk: 256,
                });
            }
            ["import", "csv", lens, path, chunk_str] => match chunk_str.parse::<usize>() {
                Ok(chunk) if chunk > 0 => {
                    self.pending = true;
                    self.status = "loading…".into();
                    self.send_io(IoRequest::ImportCsv {
                        lens: lens.to_string(),
                        path: path.to_string(),
                        chunk,
                    });
                }
                _ => {
                    let msg = format!("invalid chunk size {chunk_str:?}");
                    self.status = msg.clone();
                    self.push_log(msg, true);
                }
            },
            ["import", "lua", name, path, rest @ ..] => {
                let clause = if rest.is_empty() {
                    String::new()
                } else {
                    format!(" {}", rest.join(" "))
                };
                match std::fs::read_to_string(path) {
                    Ok(source) => {
                        let escaped = source
                            .replace('\\', "\\\\")
                            .replace('"', "\\\"")
                            .replace(['\n', '\r'], " ");
                        let stmt = format!(
                            "CREATE FUNCTION {name}{clause} AS \"{escaped}\""
                        );
                        self.pending = true;
                        self.status = "sending…".into();
                        self.send_io(IoRequest::Query(stmt));
                    }
                    Err(e) => {
                        let msg = format!("open {path}: {e}");
                        self.status = msg.clone();
                        self.push_log(msg, true);
                    }
                }
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
        self.net.send(req);
    }

    fn push_log(&mut self, response: String, is_err: bool) {
        self.log.push(LogEntry {
            query: String::new(),
            response,
            is_err,
        });
    }

    /// Drain all pending I/O responses into app state.  Call every render tick.
    pub fn drain(&mut self) {
        use std::sync::mpsc::TryRecvError;
        loop {
            match self.net.try_recv() {
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
                self.push_log(s, false);
            }
            IoResponse::Error(e) => {
                self.pending = false;
                self.status = format!("ERR {e}");
                self.push_log(format!("ERR {e}"), true);
            }
            IoResponse::Done(s) => {
                self.pending = false;
                self.status = s.clone();
                self.push_log(s, false);
            }
            IoResponse::Response(resp) => {
                self.pending = false;
                let is_err = resp.is_err();
                let text = resp.to_string();
                self.status = if is_err { text.clone() } else { "OK".into() };
                self.last_response = Some(resp);
                self.push_log(text, is_err);
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
            net: NetHandle::test_pair(tx_req, rx_resp),
            connections: vec![],
            log: vec![],
            last_response: None,
            pending: false,
            status: "Ready".into(),
            should_quit: false,
            focus: Focus::Input,
            conn_sel: 0,
            log_scroll: 0,
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
    fn submit_import_csv_sets_pending_and_status() {
        let (mut app, _tx) = test_app();
        app.submit("import csv pressure examples/data/pressure.csv".to_string());
        assert!(app.pending);
        assert_eq!(app.status, "loading…");
    }

    #[test]
    fn submit_import_csv_with_explicit_chunk_sets_pending() {
        let (mut app, _tx) = test_app();
        app.submit("import csv cpu examples/data/cpu-load.csv 128".to_string());
        assert!(app.pending);
    }

    #[test]
    fn submit_import_csv_with_invalid_chunk_logs_error() {
        let (mut app, _tx) = test_app();
        app.submit("import csv lens path notanumber".to_string());
        assert!(!app.pending);
        assert!(app.log.last().unwrap().is_err);
    }

    #[test]
    fn submit_import_csv_with_zero_chunk_logs_error() {
        let (mut app, _tx) = test_app();
        app.submit("import csv lens path 0".to_string());
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
    fn submit_import_lua_reads_file_and_constructs_create_function() {
        let (tx_req, rx_req) = mpsc::channel::<IoRequest>();
        let (_tx_resp, rx_resp) = mpsc::channel::<IoResponse>();
        let mut app = App {
            net: NetHandle::test_pair(tx_req, rx_resp),
            connections: vec![],
            log: vec![],
            last_response: None,
            pending: false,
            status: "Ready".into(),
            should_quit: false,
            focus: Focus::Input,
            conn_sel: 0,
            log_scroll: 0,
        };

        let path =
            format!("/tmp/tauctl-import-lua-test-{}.lua", std::process::id());
        std::fs::write(&path, "local x = 1\nreturn x * 2\n").unwrap();

        app.submit(format!(
            "import lua double {path} ON WRITE LENS returns CAPS exec,range"
        ));

        std::fs::remove_file(&path).ok();

        assert!(app.pending);
        assert_eq!(app.status, "sending…");

        let req = rx_req
            .try_recv()
            .expect("IoRequest should have been sent");
        match req {
            IoRequest::Query(stmt) => {
                assert!(
                    stmt.starts_with("CREATE FUNCTION double ON WRITE LENS returns CAPS exec,range AS \""),
                    "unexpected stmt: {stmt}"
                );
                assert!(
                    stmt.contains("local x = 1 return x * 2"),
                    "newlines should be joined with spaces: {stmt}"
                );
                assert!(
                    stmt.ends_with('"'),
                    "stmt should end with closing quote: {stmt}"
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn submit_import_lua_missing_file_logs_error() {
        let (mut app, _tx) = test_app();
        app.submit("import lua fn1 /nonexistent.lua".to_string());
        assert!(!app.pending);
        assert!(app.log.last().unwrap().is_err);
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
    fn focus_from_digit_maps_panes() {
        assert_eq!(Focus::from_digit(1), Some(Focus::Connections));
        assert_eq!(Focus::from_digit(2), Some(Focus::Results));
        assert_eq!(Focus::from_digit(3), Some(Focus::Log));
        assert_eq!(Focus::from_digit(4), None);
    }

    #[test]
    fn select_conn_saturates_at_bounds() {
        let (mut app, _tx) = test_app();
        app.connections = vec![
            ("a".into(), "x".into(), true, false),
            ("b".into(), "y".into(), false, false),
        ];
        app.select_conn(-1);
        assert_eq!(app.conn_sel, 0, "cannot go above the first row");
        app.select_conn(1);
        assert_eq!(app.conn_sel, 1);
        app.select_conn(5);
        assert_eq!(app.conn_sel, 1, "cannot go past the last row");
    }

    #[test]
    fn scroll_log_saturates_at_top() {
        let (mut app, _tx) = test_app();
        app.scroll_log(3);
        assert_eq!(app.log_scroll, 3);
        app.scroll_log(-10);
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn activate_selected_conn_sends_use() {
        let (tx_req, rx_req) = mpsc::channel::<IoRequest>();
        let (_tx_resp, rx_resp) = mpsc::channel::<IoResponse>();
        let mut app = App {
            net: NetHandle::test_pair(tx_req, rx_resp),
            connections: vec![("prod".into(), "10.0.0.1:7070".into(), false, true)],
            log: vec![],
            last_response: None,
            pending: false,
            status: "Ready".into(),
            should_quit: false,
            focus: Focus::Connections,
            conn_sel: 0,
            log_scroll: 0,
        };
        app.activate_selected_conn();
        match rx_req.try_recv() {
            Ok(IoRequest::Use(name)) => assert_eq!(name, "prod"),
            other => panic!("expected Use(prod), got {other:?}"),
        }
    }

    #[test]
    fn pane_text_renders_log_and_connections() {
        let (mut app, _tx) = test_app();
        app.connections = vec![("dev".into(), "127.0.0.1:7070".into(), true, false)];
        app.log = vec![LogEntry {
            query: "AT LENS x 0".into(),
            response: "VAL i42".into(),
            is_err: false,
        }];
        assert_eq!(app.pane_text(Focus::Connections), "* dev  127.0.0.1:7070");
        assert_eq!(app.pane_text(Focus::Log), "AT LENS x 0 -> VAL i42");
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
