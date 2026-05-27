//! Commands for the tauctl REPL.
//!
//! A `Command` is a name + description + action.  Actions receive the
//! `Registry` (so commands like `help` can introspect it) and a mutable
//! reference to the `Repl` (so commands can read or grow its history),
//! plus the raw input line.

use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};

use crate::repl::Repl;

pub type CommandResult = Result<(), String>;
pub type CommandAction = Box<dyn Fn(&Registry, &mut Repl, &str) -> CommandResult>;

pub struct Command {
    pub name: String,
    pub description: String,
    pub action: CommandAction,
}

impl Command {
    pub fn new<F>(name: &str, description: &str, action: F) -> Self
    where
        F: Fn(&Registry, &mut Repl, &str) -> CommandResult + 'static,
    {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            action: Box::new(action),
        }
    }
}

pub struct Registry {
    commands: Vec<Command>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn register(&mut self, command: Command) {
        self.commands.push(command);
    }

    /// Look up a command by its exact name.
    pub fn find(&self, name: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.name == name)
    }
}

pub fn help_command() -> Command {
    Command::new("help", "Show this help message", |registry, _, _| {
        println!("Available commands:");
        for cmd in &registry.commands {
            println!("  {:<10} {}", cmd.name, cmd.description);
        }
        Ok(())
    })
}

pub fn clear_command() -> Command {
    Command::new("clear", "Clear the screen", |_, _, _| {
        let mut out = io::stdout();
        if out.is_terminal() {
            // ESC[2J = clear screen, ESC[H = cursor to home.  Matches what
            // the standard `clear(1)` emits for most terminals.
            out.write_all(b"\x1b[2J\x1b[H").map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        }
        Ok(())
    })
}

pub fn connect_command() -> Command {
    Command::new(
        "connect",
        "connect <name> <host:port> [tls] [<user> <pass>] - open a tau connection",
        |_, repl, line| {
            // Tokens after "connect": name, host:port, then any of [tls] [user pass].
            let args: Vec<&str> = line.split_whitespace().skip(1).collect();
            if args.len() < 2 {
                return Err("usage: connect <name> <host:port> [tls] [<user> <pass>]".to_string());
            }
            let (name, addr) = (args[0], args[1]);
            let mut rest = &args[2..];

            // Optional "tls" keyword (any position after addr).
            let mut tls = false;
            if let Some(pos) = rest.iter().position(|t| t.eq_ignore_ascii_case("tls")) {
                tls = true;
                // Remove the tls token from the slice by collecting around it.
                // Simpler: rebuild rest without it.
                let mut filtered: Vec<&str> = Vec::with_capacity(rest.len() - 1);
                for (i, t) in rest.iter().enumerate() {
                    if i != pos {
                        filtered.push(*t);
                    }
                }
                // Stash into a heap vec so the slice borrow lasts.
                let filtered = Box::leak(filtered.into_boxed_slice());
                rest = filtered;
            }

            // Optional auth: two trailing tokens = user pass.
            let auth_pair: Option<(&str, &str)> = match rest.len() {
                0 => None,
                2 => Some((rest[0], rest[1])),
                _ => {
                    return Err(
                        "usage: connect <name> <host:port> [tls] [<user> <pass>]".to_string()
                    );
                }
            };

            if tls {
                // SNI: use the host part of host:port.  Defaults to "localhost"
                // when parsing fails - matches the server's dev cert.
                let sni = addr.split(':').next().unwrap_or("localhost").to_string();
                repl.manager
                    .connect_tls(name, addr, &sni)
                    .map_err(|e| format!("connect: {}", e))?;
            } else {
                repl.manager
                    .connect(name, addr)
                    .map_err(|e| format!("connect: {}", e))?;
            }

            println!(
                "connected to {} as {} ({})",
                addr,
                name,
                if tls { "TLS" } else { "plain" }
            );

            // If auth args were provided, send AUTH right away on the new
            // connection (not the active one - connect doesn't steal active).
            if let Some((user, pass)) = auth_pair {
                let conn = repl
                    .manager
                    .get_mut(name)
                    .ok_or_else(|| format!("connection '{}' vanished after connect", name))?;
                let resp = conn
                    .send(&format!("AUTH {} {}", user, pass))
                    .map_err(|e| e.to_string())?;
                println!("{}", resp);
                if let Some(msg) = resp.strip_prefix("ERR ") {
                    return Err(msg.to_string());
                }
            }
            Ok(())
        },
    )
}

pub fn auth_command() -> Command {
    Command::new(
        "auth",
        "auth <user> <pass> - send AUTH on the active connection",
        |_, repl, line| {
            let args: Vec<&str> = line.split_whitespace().skip(1).collect();
            if args.len() != 2 {
                return Err("usage: auth <user> <pass>".to_string());
            }
            let conn = repl
                .manager
                .active_mut()
                .ok_or_else(|| "no active connection".to_string())?;
            let resp = conn
                .send(&format!("AUTH {} {}", args[0], args[1]))
                .map_err(|e| e.to_string())?;
            println!("{}", resp);
            if let Some(msg) = resp.strip_prefix("ERR ") {
                return Err(msg.to_string());
            }
            Ok(())
        },
    )
}

pub fn disconnect_command() -> Command {
    Command::new(
        "disconnect",
        "disconnect <name> - close a TCP connection",
        |_, repl, line| {
            let args: Vec<&str> = line.split_whitespace().skip(1).collect();
            if args.len() != 1 {
                return Err("usage: disconnect <name>".to_string());
            }
            repl.manager.disconnect(args[0])?;
            println!("disconnected {}", args[0]);
            Ok(())
        },
    )
}

pub fn use_command() -> Command {
    Command::new(
        "use",
        "use <name> - switch the active connection",
        |_, repl, line| {
            let args: Vec<&str> = line.split_whitespace().skip(1).collect();
            if args.len() != 1 {
                return Err("usage: use <name>".to_string());
            }
            repl.manager.set_active(args[0])?;
            println!("active connection now {}", args[0]);
            Ok(())
        },
    )
}

/// Client-side bulk load.  Reads a CSV (`start,end,value` per row, `#`
/// comments and blank lines skipped) from the **client's** filesystem and
/// ships it to the active connection as a sequence of multi-tau `APPEND`
/// statements.  Use this when the data lives on your machine (e.g. a laptop
/// driving a Docker container).  For files already inside the server's
/// filesystem use the server-side `COPY LENS x FROM "<path>"` statement.
pub fn load_command() -> Command {
    Command::new(
        "load",
        "load <lens> <local-path> [chunk] - send a local CSV as batched APPENDs",
        |_, repl, line| {
            let args: Vec<&str> = line.split_whitespace().skip(1).collect();
            if args.len() < 2 || args.len() > 3 {
                return Err("usage: load <lens> <local-path> [chunk]".to_string());
            }
            let lens = args[0];
            let path = args[1];
            let chunk: usize = if args.len() == 3 {
                args[2]
                    .parse()
                    .map_err(|_| format!("invalid chunk size {:?}", args[2]))?
            } else {
                256
            };
            if chunk == 0 {
                return Err("chunk size must be >= 1".to_string());
            }

            let conn = repl
                .manager
                .active_mut()
                .ok_or_else(|| "no active connection - try `connect`".to_string())?;

            let file = File::open(path).map_err(|e| format!("open {}: {}", path, e))?;
            let reader = BufReader::new(file);
            let mut buffer: Vec<String> = Vec::with_capacity(chunk);
            let mut total: u64 = 0;
            let mut chunks: u64 = 0;

            for (lineno, raw) in reader.lines().enumerate() {
                let line = raw.map_err(|e| format!("read {} line {}: {}", path, lineno + 1, e))?;
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let mut parts = trimmed.splitn(3, ',');
                let start = parts
                    .next()
                    .ok_or_else(|| format!("{}:{}: missing start", path, lineno + 1))?
                    .trim();
                let end = parts
                    .next()
                    .ok_or_else(|| format!("{}:{}: missing end", path, lineno + 1))?
                    .trim();
                let value = parts
                    .next()
                    .ok_or_else(|| format!("{}:{}: missing value", path, lineno + 1))?
                    .trim();
                buffer.push(format!("{} {} {}", start, end, value));
                if buffer.len() >= chunk {
                    let resp = ship(conn, lens, &buffer)?;
                    if !resp.starts_with("OK") {
                        return Err(format!(
                            "server rejected chunk #{} at row {}: {}",
                            chunks + 1,
                            total + buffer.len() as u64,
                            resp.strip_prefix("ERR ").unwrap_or(&resp)
                        ));
                    }
                    total += buffer.len() as u64;
                    chunks += 1;
                    buffer.clear();
                }
            }
            if !buffer.is_empty() {
                let resp = ship(conn, lens, &buffer)?;
                if !resp.starts_with("OK") {
                    return Err(format!(
                        "server rejected final chunk: {}",
                        resp.strip_prefix("ERR ").unwrap_or(&resp)
                    ));
                }
                total += buffer.len() as u64;
                chunks += 1;
            }

            println!(
                "loaded {} rows into {} ({} chunk{})",
                total,
                lens,
                chunks,
                if chunks == 1 { "" } else { "s" }
            );
            Ok(())
        },
    )
}

/// Build and send one `APPEND LENS <lens> s e v, s e v, ...` statement
/// over `conn`, returning the trimmed response line.
fn ship(
    conn: &mut crate::tcpmgr::Connection,
    lens: &str,
    taus: &[String],
) -> Result<String, String> {
    let mut stmt = String::with_capacity(32 + taus.len() * 24);
    stmt.push_str("APPEND LENS ");
    stmt.push_str(lens);
    stmt.push(' ');
    for (i, t) in taus.iter().enumerate() {
        if i > 0 {
            stmt.push_str(", ");
        }
        stmt.push_str(t);
    }
    conn.send(&stmt).map_err(|e| e.to_string())
}

pub fn connections_command() -> Command {
    Command::new(
        "connections",
        "List registered connections (active marked with *, TLS marked too)",
        |_, repl, _| {
            let entries = repl.manager.list();
            if entries.is_empty() {
                println!("(no connections)");
                return Ok(());
            }
            for (name, addr, active, tls) in entries {
                let marker = if active { "*" } else { " " };
                let scheme = if tls { "tls" } else { "tcp" };
                println!("{} {:<10} {:<4} {}", marker, name, scheme, addr);
            }
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn echo_listener() -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let read = stream.try_clone().unwrap();
                let mut writer = stream;
                let mut reader = BufReader::new(read);
                let mut buf = String::new();
                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                    writer.write_all(b"OK\n").unwrap();
                    buf.clear();
                }
            }
        });
        (addr, handle)
    }

    #[test]
    fn registry_new_is_empty() {
        let r = Registry::new();
        assert!(r.find("anything").is_none());
    }

    #[test]
    fn registry_find_returns_registered_command() {
        let mut r = Registry::new();
        r.register(help_command());
        assert!(r.find("help").is_some());
        assert!(r.find("nope").is_none());
    }

    #[test]
    fn registry_keeps_first_registered_when_names_collide() {
        // find() returns the first match - registering twice keeps the older one.
        let mut r = Registry::new();
        r.register(Command::new("dup", "first", |_, _, _| Ok(())));
        r.register(Command::new("dup", "second", |_, _, _| Ok(())));
        assert_eq!(r.find("dup").unwrap().description, "first");
    }

    #[test]
    fn command_new_records_name_and_description() {
        let c = Command::new("ping", "say pong", |_, _, _| Ok(()));
        assert_eq!(c.name, "ping");
        assert_eq!(c.description, "say pong");
    }

    #[test]
    fn help_command_metadata() {
        let c = help_command();
        assert_eq!(c.name, "help");
        assert!(!c.description.is_empty());
    }

    #[test]
    fn help_command_action_succeeds() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = help_command();
        assert!((c.action)(&registry, &mut repl, "help").is_ok());
    }

    #[test]
    fn clear_command_action_succeeds() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = clear_command();
        // No TTY in `cargo test` stdout, so this is the no-op branch - still Ok.
        assert!((c.action)(&registry, &mut repl, "clear").is_ok());
    }

    #[test]
    fn connect_command_rejects_wrong_arity() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = connect_command();
        assert!((c.action)(&registry, &mut repl, "connect").is_err());
        assert!((c.action)(&registry, &mut repl, "connect onlyname").is_err());
        assert!((c.action)(&registry, &mut repl, "connect a b c").is_err());
    }

    #[test]
    fn connect_command_opens_and_registers() {
        let (addr, _h) = echo_listener();
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = connect_command();
        let line = format!("connect dev {}", addr);
        assert!((c.action)(&registry, &mut repl, &line).is_ok());
        assert_eq!(
            repl.manager.list(),
            vec![("dev".to_string(), addr.to_string(), true, false)]
        );
    }

    #[test]
    fn disconnect_command_errors_on_unknown() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = disconnect_command();
        let result = (c.action)(&registry, &mut repl, "disconnect ghost");
        assert!(result.is_err());
    }

    #[test]
    fn disconnect_command_rejects_wrong_arity() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = disconnect_command();
        assert!((c.action)(&registry, &mut repl, "disconnect").is_err());
        assert!((c.action)(&registry, &mut repl, "disconnect a b").is_err());
    }

    #[test]
    fn use_command_errors_on_unknown() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = use_command();
        assert!((c.action)(&registry, &mut repl, "use ghost").is_err());
    }

    #[test]
    fn use_command_switches_active() {
        let (addr_a, _ha) = echo_listener();
        let (addr_b, _hb) = echo_listener();
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        repl.manager.connect("a", &addr_a.to_string()).unwrap();
        repl.manager.connect("b", &addr_b.to_string()).unwrap();
        let c = use_command();
        assert!((c.action)(&registry, &mut repl, "use b").is_ok());
        let active = repl
            .manager
            .list()
            .into_iter()
            .find(|(_, _, on, _)| *on)
            .unwrap();
        assert_eq!(active.0, "b");
    }

    #[test]
    fn auth_command_rejects_wrong_arity() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = auth_command();
        assert!((c.action)(&registry, &mut repl, "auth").is_err());
        assert!((c.action)(&registry, &mut repl, "auth onlyuser").is_err());
        assert!((c.action)(&registry, &mut repl, "auth a b c").is_err());
    }

    #[test]
    fn auth_command_errors_with_no_active_connection() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = auth_command();
        assert!((c.action)(&registry, &mut repl, "auth admin pass").is_err());
    }

    #[test]
    fn connect_command_accepts_inline_auth_args() {
        // Echo server replies "OK\n" to every line including AUTH.
        let (addr, _h) = echo_listener();
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = connect_command();
        let line = format!("connect dev {} admin s3cret", addr);
        assert!((c.action)(&registry, &mut repl, &line).is_ok());
    }

    #[test]
    fn connect_command_rejects_odd_trailing_tokens() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = connect_command();
        // 3 trailing tokens (after addr) is neither "tls" alone nor a 2-tuple.
        let result = (c.action)(&registry, &mut repl, "connect dev 127.0.0.1:9 a b c");
        assert!(result.is_err());
    }

    #[test]
    fn connections_command_succeeds_when_empty_and_when_populated() {
        let (addr, _h) = echo_listener();
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = connections_command();
        assert!((c.action)(&registry, &mut repl, "connections").is_ok());
        repl.manager.connect("dev", &addr.to_string()).unwrap();
        assert!((c.action)(&registry, &mut repl, "connections").is_ok());
    }

    /// Listener that captures every received line so a test can assert on
    /// the exact wire traffic produced by `load`.
    fn capture_listener() -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::{Arc, Mutex};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let shared = lines.clone();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let read = stream.try_clone().unwrap();
                let mut writer = stream;
                let mut reader = BufReader::new(read);
                let mut buf = String::new();
                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                    shared
                        .lock()
                        .unwrap()
                        .push(buf.trim_end_matches(['\r', '\n']).to_string());
                    writer.write_all(b"OK\n").unwrap();
                    buf.clear();
                }
            }
        });
        (addr, lines)
    }

    #[test]
    fn load_command_rejects_wrong_arity_and_chunk() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = load_command();
        assert!((c.action)(&registry, &mut repl, "load").is_err());
        assert!((c.action)(&registry, &mut repl, "load only").is_err());
        assert!((c.action)(&registry, &mut repl, "load a b c d").is_err());
        assert!((c.action)(&registry, &mut repl, "load lens /tmp/x notanumber").is_err());
    }

    #[test]
    fn load_command_errors_with_no_active_connection() {
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        let c = load_command();
        let err = (c.action)(&registry, &mut repl, "load x /tmp/nope.csv").unwrap_err();
        assert!(err.starts_with("no active connection"));
    }

    #[test]
    fn load_command_errors_when_file_missing() {
        let (addr, _) = capture_listener();
        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        repl.manager.connect("dev", &addr.to_string()).unwrap();
        let c = load_command();
        let err = (c.action)(&registry, &mut repl, "load x /tmp/no-such-file.csv").unwrap_err();
        assert!(err.contains("open"));
    }

    #[test]
    fn load_command_chunks_rows_and_skips_comments() {
        let (addr, lines) = capture_listener();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tauctl-load-{}.csv", std::process::id()));
        std::fs::write(
            &path,
            b"# header\n\n0,10,1\n10,20,2\n20,30,3\n30,40,4\n40,50,5\n",
        )
        .unwrap();

        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        repl.manager.connect("dev", &addr.to_string()).unwrap();
        let c = load_command();
        let cmd = format!("load temp {} 2", path.display());
        (c.action)(&registry, &mut repl, &cmd).expect("load should succeed");

        let captured = lines.lock().unwrap().clone();
        // 5 rows, chunk size 2 -> chunks of 2, 2, 1.
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0], "APPEND LENS temp 0 10 1, 10 20 2");
        assert_eq!(captured[1], "APPEND LENS temp 20 30 3, 30 40 4");
        assert_eq!(captured[2], "APPEND LENS temp 40 50 5");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_command_propagates_server_err_mid_stream() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let read = stream.try_clone().unwrap();
                let mut writer = stream;
                let mut reader = BufReader::new(read);
                let mut buf = String::new();
                let mut n = 0;
                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                    n += 1;
                    let resp = if n == 2 {
                        b"ERR type mismatch\n".to_vec()
                    } else {
                        b"OK\n".to_vec()
                    };
                    writer.write_all(&resp).unwrap();
                    buf.clear();
                }
            }
        });

        let dir = std::env::temp_dir();
        let path = dir.join(format!("tauctl-load-err-{}.csv", std::process::id()));
        std::fs::write(&path, b"0,1,1\n1,2,2\n2,3,3\n3,4,4\n").unwrap();

        let registry = Registry::new();
        let mut repl = Repl::new("τ: ".into());
        repl.manager.connect("dev", &addr.to_string()).unwrap();
        let c = load_command();
        let cmd = format!("load lens {} 1", path.display());
        let err = (c.action)(&registry, &mut repl, &cmd).unwrap_err();
        assert!(err.contains("type mismatch"), "got: {err}");

        std::fs::remove_file(&path).ok();
    }
}
