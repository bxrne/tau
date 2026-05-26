//! Read–eval–print loop.
//!
//! Each iteration: write the prompt, read one line, dispatch it (first as a
//! built-in command via the `Registry`, otherwise as a tauql statement to the
//! active TCP connection), then print a one-line status footer with elapsed
//! time and exit code.  Ctrl-D / EOF and `exit` / `quit` terminate cleanly.

use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use crate::commands::{CommandResult, Registry};
use crate::style;
use crate::tcpmgr::TcpManager;

pub struct Repl {
    pub history: Vec<String>,
    pub manager: TcpManager,
    prompt: String,
}

impl Repl {
    pub fn new(prompt: String) -> Self {
        Self {
            history: Vec::new(),
            manager: TcpManager::new(),
            prompt,
        }
    }

    pub fn run(&mut self, registry: &Registry) {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let mut input = String::new();

        loop {
            print!("{}", style::bold(&style::cyan(&self.prompt)));
            if stdout.flush().is_err() {
                break;
            }

            input.clear();
            match stdin.lock().read_line(&mut input) {
                Ok(0) => {
                    println!();
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("{}", style::red(&format!("read error: {}", e)));
                    break;
                }
            }

            let line = input.trim();
            if line.is_empty() {
                continue;
            }
            if line == "exit" || line == "quit" {
                println!("{}", style::dim("bye."));
                break;
            }

            self.history.push(line.to_string());

            let started = Instant::now();
            let result = dispatch(registry, self, line);
            print_status(&result, started.elapsed());
        }
    }
}

/// Dispatch policy:
/// 1. First whitespace token matches a registered command → run it.
/// 2. Otherwise, if a TCP connection is active → send `line` as a tauql
///    statement, print the response, surface `ERR ...` as an error.
/// 3. Otherwise → "unknown command".
fn dispatch(registry: &Registry, repl: &mut Repl, line: &str) -> CommandResult {
    let name = line.split_whitespace().next().unwrap_or("");
    if let Some(cmd) = registry.find(name) {
        return (cmd.action)(registry, repl, line);
    }
    if let Some(conn) = repl.manager.active_mut() {
        let resp = conn.send(line).map_err(|e| e.to_string())?;
        println!("{}", resp);
        if let Some(msg) = resp.strip_prefix("ERR ") {
            return Err(msg.to_string());
        }
        return Ok(());
    }
    Err(format!(
        "unknown command: {} (no active connection - try `connect`)",
        name
    ))
}

fn print_status(result: &Result<(), String>, elapsed: Duration) {
    println!("{}", format_status(result, elapsed));
}

/// Pure formatter: produces the styled footer line for a dispatch result.
/// Exposed for tests; `print_status` is the wrapper that actually prints.
fn format_status(result: &Result<(), String>, elapsed: Duration) -> String {
    let pretty = format_elapsed(elapsed);
    match result {
        Ok(()) => style::dim(&format!("[ok in {}]", pretty)),
        Err(msg) => style::red(&format!("[err 1 in {}: {}]", pretty, msg)),
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs >= 1.0 {
        format!("{:.3}s", secs)
    } else if secs >= 1e-3 {
        format!("{:.3}ms", secs * 1e3)
    } else {
        format!("{:.3}µs", secs * 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Command, Registry};
    use std::cell::Cell;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::rc::Rc;
    use std::thread::JoinHandle;

    fn echo_listener() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let read = stream.try_clone().unwrap();
                let mut writer = stream;
                let mut reader = BufReader::new(read);
                let mut buf = String::new();
                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                    writeln!(writer, "OK").unwrap();
                    buf.clear();
                }
            }
        });
        (addr, handle)
    }

    fn err_listener() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let read = stream.try_clone().unwrap();
                let mut writer = stream;
                let mut reader = BufReader::new(read);
                let mut buf = String::new();
                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                    writeln!(writer, "ERR something went wrong").unwrap();
                    buf.clear();
                }
            }
        });
        (addr, handle)
    }

    #[test]
    fn format_elapsed_picks_unit_by_magnitude() {
        assert!(format_elapsed(Duration::from_micros(123)).ends_with("µs"));
        assert!(format_elapsed(Duration::from_millis(45)).ends_with("ms"));
        assert!(format_elapsed(Duration::from_secs(2)).ends_with('s'));
    }

    #[test]
    fn format_status_ok_contains_ok_marker_and_elapsed() {
        let line = format_status(&Ok(()), Duration::from_micros(5));
        assert!(line.contains("[ok in "));
        assert!(line.contains("5.000µs]"));
    }

    #[test]
    fn format_status_err_contains_err_marker_and_message() {
        let line = format_status(&Err("boom".to_string()), Duration::from_millis(2));
        assert!(line.contains("[err 1 in "));
        assert!(line.contains("2.000ms"));
        assert!(line.contains("boom"));
    }

    #[test]
    fn dispatch_runs_matching_registered_command() {
        let mut registry = Registry::new();
        let calls: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        let probe = calls.clone();
        registry.register(Command::new("ping", "test", move |_, _, _| {
            probe.set(probe.get() + 1);
            Ok(())
        }));
        let mut repl = Repl::new("τ: ".into());
        assert!(dispatch(&registry, &mut repl, "ping").is_ok());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn dispatch_propagates_command_error() {
        let mut registry = Registry::new();
        registry.register(Command::new("bad", "test", |_, _, _| {
            Err("nope".to_string())
        }));
        let mut repl = Repl::new("τ: ".into());
        assert_eq!(
            dispatch(&registry, &mut repl, "bad").unwrap_err(),
            "nope".to_string()
        );
    }

    #[test]
    fn dispatch_unknown_with_no_connection_errors() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let err = dispatch(&registry, &mut repl, "SELECT 1").unwrap_err();
        assert!(err.starts_with("unknown command: SELECT"));
    }

    #[test]
    fn dispatch_forwards_to_active_connection_on_miss() {
        let (addr, _h) = echo_listener();
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        repl.manager.connect("dev", &addr.to_string()).unwrap();
        // "SHOW LENSES" is not a registered command - should pass through.
        assert!(dispatch(&registry, &mut repl, "SHOW LENSES").is_ok());
    }

    #[test]
    fn dispatch_surfaces_server_err_response_as_error() {
        let (addr, _h) = err_listener();
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        repl.manager.connect("dev", &addr.to_string()).unwrap();
        let err = dispatch(&registry, &mut repl, "anything").unwrap_err();
        assert_eq!(err, "something went wrong");
    }

    #[test]
    fn dispatch_prefers_registered_command_over_connection_fallthrough() {
        let (addr, _h) = echo_listener();
        let mut registry = Registry::new();
        let hits: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        let probe = hits.clone();
        registry.register(Command::new("HELLO", "test", move |_, _, _| {
            probe.set(probe.get() + 1);
            Ok(())
        }));
        let mut repl = Repl::new("τ: ".into());
        repl.manager.connect("dev", &addr.to_string()).unwrap();
        dispatch(&registry, &mut repl, "HELLO world").unwrap();
        // The command should have fired locally - no traffic to the server.
        assert_eq!(hits.get(), 1);
    }
}
