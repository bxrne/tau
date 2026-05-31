//! Read–eval–print loop, backed by `rustyline` for arrow keys, history,
//! bracketed paste, and the standard emacs-style line-edit shortcuts.
//!
//! Each iteration: read one line through the rustyline editor, dispatch it
//! (first as a built-in command via the `Registry`, otherwise as a tauql
//! statement to the active TCP connection), then print a one-line status
//! footer with elapsed time and exit code.  `Ctrl-D` / EOF and `exit` /
//! `quit` terminate cleanly; `Ctrl-C` cancels the current line and continues.

use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustyline::{ColorMode, Config, EditMode, Editor, error::ReadlineError, history::FileHistory};

use crate::commands::{CommandResult, Registry};
use crate::style;
use crate::tcpmgr::TcpManager;

/// Filename used when persisting REPL history beneath the user's home dir.
const HISTORY_FILENAME: &str = ".tau_history";

pub struct Repl {
    pub manager: TcpManager,
    prompt: String,
}

impl Repl {
    pub fn new(prompt: String) -> Self {
        Self {
            manager: TcpManager::new(),
            prompt,
        }
    }

    pub fn run(&mut self, registry: &Registry) {
        let config = Config::builder()
            .auto_add_history(true)
            .color_mode(ColorMode::Enabled)
            .edit_mode(EditMode::Emacs)
            .completion_type(rustyline::CompletionType::List)
            .bracketed_paste(true)
            .build();

        let mut editor: Editor<(), FileHistory> = match Editor::with_config(config) {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "{}",
                    style::red(&format!("could not initialise readline: {}", e))
                );
                return;
            }
        };

        let history_path = resolve_history_path();
        if let Some(path) = history_path.as_deref()
            && path.exists()
            && let Err(e) = editor.load_history(path)
        {
            eprintln!(
                "{}",
                style::dim(&format!(
                    "warning: could not load history from {}: {}",
                    path.display(),
                    e
                ))
            );
        }

        // Prompt is rendered with ANSI styling; rustyline wraps the escape
        // sequences in invisible markers when ColorMode::Enabled is set.
        let prompt = style::bold(&style::cyan(&self.prompt));

        loop {
            match editor.readline(&prompt) {
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if trimmed == "exit" || trimmed == "quit" {
                        println!("{}", style::dim("bye."));
                        break;
                    }
                    let started = Instant::now();
                    let result = dispatch(registry, self, trimmed);
                    print_status(&result, started.elapsed());
                }
                Err(ReadlineError::Interrupted) => {
                    // Ctrl-C: cancel current line, do not exit.
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    println!("{}", style::dim("bye."));
                    break;
                }
                Err(e) => {
                    eprintln!("{}", style::red(&format!("read error: {}", e)));
                    break;
                }
            }
        }

        if let Some(path) = history_path.as_deref()
            && let Err(e) = editor.save_history(path)
        {
            eprintln!(
                "{}",
                style::dim(&format!(
                    "warning: could not save history to {}: {}",
                    path.display(),
                    e
                ))
            );
        }
    }
}

/// Resolve the file used to persist line-edit history across sessions.
///
/// Priority: `TAU_HISTORY_FILE` env var > `$HOME/.tau_history`.  Returns
/// `None` when neither is resolvable (history is then in-memory only).
fn resolve_history_path() -> Option<PathBuf> {
    if let Ok(explicit) = env::var("TAU_HISTORY_FILE")
        && !explicit.is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    let home = env::var("HOME").ok().filter(|s| !s.is_empty())?;
    Some(Path::new(&home).join(HISTORY_FILENAME))
}

/// Dispatch policy:
/// - If the first whitespace token matches a registered command, run it.
/// - Otherwise, if a TCP connection is active, send `line` as a tauql
///   statement, print the response, and surface `ERR ...` as an error.
/// - Otherwise, return "unknown command".
fn dispatch(registry: &Registry, repl: &mut Repl, line: &str) -> CommandResult {
    let name = line.split_whitespace().next().unwrap_or("");
    if let Some(cmd) = registry.find(name) {
        return (cmd.action)(registry, repl, line);
    }
    if let Some(conn) = repl.manager.active_mut() {
        let resp = conn.send(line).map_err(|e| e.to_string())?;
        println!("{}", resp);
        if let libtau::Response::Err(msg) = &resp {
            return Err(msg.clone());
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
    use pretty_assertions::assert_eq;
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

    #[test]
    fn resolve_history_path_prefers_env_var() {
        let saved = env::var("TAU_HISTORY_FILE").ok();
        // SAFETY: tests in this crate are run with --test-threads logic that
        // does not assume process-wide env var stability; this scoped set/restore
        // is sufficient for sequential coverage.
        unsafe { env::set_var("TAU_HISTORY_FILE", "/tmp/tau-history-test") };
        let p = resolve_history_path().expect("should resolve");
        assert_eq!(p, PathBuf::from("/tmp/tau-history-test"));
        match saved {
            Some(v) => unsafe { env::set_var("TAU_HISTORY_FILE", v) },
            None => unsafe { env::remove_var("TAU_HISTORY_FILE") },
        }
    }
}
