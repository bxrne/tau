use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use libtau::{Executor, Metrics, Response, format_parse_error, needs_registry_lock, parse};
use tracing::{debug, info, trace, warn};

// Parse an AUTH line, returning the username and password if successful.  The caller should enforce
// that this is only called once as the very first message from the client, and that the line starts
// with "AUTH " (case-sensitive).  Returns `None` if the line is malformed (e.g. missing password).
fn parse_auth_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim().strip_prefix("AUTH ")?;
    let (user, pass) = rest.split_once(' ')?;
    Some((user.to_string(), pass.to_string()))
}

/// Attempt to authenticate the client with the line already read.
/// Returns `Ok(Some(username))` on success, `Ok(None)` on bad credentials
/// (response already sent, caller should break), `Err` on I/O error.
fn handle_auth_attempt<S: Read + Write>(
    reader: &mut BufReader<S>,
    peer: SocketAddr,
    exec: &Arc<RwLock<Executor>>,
    metrics: &Arc<Metrics>,
    trimmed: &str,
) -> io::Result<Option<String>> {
    match parse_auth_line(trimmed) {
        Some((u, p)) => {
            metrics.record_auth_attempt();
            if exec
                .read()
                .expect("executor lock poisoned")
                .users()
                .verify(&u, &p)
                .is_some()
            {
                reader.get_mut().write_all(b"OK\n")?;
                reader.get_mut().flush()?;
                info!(%peer, user = %u, "authenticated");
                Ok(Some(u))
            } else {
                metrics.record_auth_failure();
                metrics.record_error();
                warn!(%peer, user = %u, "authentication failed");
                reader.get_mut().write_all(b"ERR authentication failed\n")?;
                reader.get_mut().flush()?;
                Ok(None)
            }
        }
        None => {
            metrics.record_auth_failure();
            metrics.record_error();
            warn!(%peer, "first message was not AUTH");
            reader
                .get_mut()
                .write_all(b"ERR authentication required\n")?;
            reader.get_mut().flush()?;
            Ok(None)
        }
    }
}

fn is_quit_cmd(s: &str) -> bool {
    s.eq_ignore_ascii_case("QUIT") || s.eq_ignore_ascii_case("EXIT")
}

/// Maximum size, in bytes, of a single client request line (including the
/// trailing `\n`). TauQL is line-oriented, so this bounds the largest single
/// statement (e.g. a `BATCH APPEND`) the server will accept. Chosen generous
/// enough that no realistic statement hits it, while preventing a client from
/// driving unbounded heap growth by streaming a line with no `\n`.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Outcome of [`read_line_bounded`].
#[derive(Debug)]
enum LineRead {
    /// Connection closed with no more data.
    Eof,
    /// A complete line (including the trailing `\n`, if present), decoded
    /// lossily as UTF-8.
    Line(String),
    /// The line exceeded `max_len` bytes before a `\n` was found (or before
    /// EOF). Any remaining bytes up to and including the next `\n` (or EOF)
    /// have been consumed and discarded, so the stream remains framed on line
    /// boundaries for the caller's next read.
    TooLong,
}

/// Read one line from `reader`, bounding total allocation to `max_len` bytes.
///
/// Unlike `BufRead::read_line`, this never grows its internal buffer past
/// `max_len`: once that many bytes have been seen without a `\n`, the rest of
/// the oversized line is drained and discarded (up to and including its
/// terminating `\n`, or EOF) and `LineRead::TooLong` is returned.
fn read_line_bounded<R: BufRead>(reader: &mut R, max_len: usize) -> io::Result<LineRead> {
    let mut buf: Vec<u8> = Vec::new();
    let mut overflow = false;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if overflow {
                LineRead::TooLong
            } else if buf.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line(String::from_utf8_lossy(&buf).into_owned())
            });
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            let was_overflow = overflow || buf.len() + take > max_len;
            if !was_overflow {
                buf.extend_from_slice(&available[..take]);
            }
            reader.consume(take);
            return Ok(if was_overflow {
                LineRead::TooLong
            } else {
                LineRead::Line(String::from_utf8_lossy(&buf).into_owned())
            });
        }

        let len = available.len();
        if !overflow {
            if buf.len() + len > max_len {
                overflow = true;
            } else {
                buf.extend_from_slice(available);
            }
        }
        reader.consume(len);
    }
}

/// Drive a single client connection over any `Read + Write` stream.
///
/// Enforces authentication (when `auth_enabled`) as the very first exchange,
/// then routes every subsequent statement through `exec_as` so the matched
/// user's per-database CRUDA grants are enforced.
pub fn run_query_loop<S: Read + Write>(
    reader: &mut BufReader<S>,
    peer: SocketAddr,
    exec: &Arc<RwLock<Executor>>,
    auth_enabled: bool,
) -> io::Result<()> {
    let metrics = exec.read().expect("executor lock poisoned").metrics();
    let mut authenticated_user: Option<String> = None;

    loop {
        let line_buf = match read_line_bounded(reader, MAX_LINE_BYTES)? {
            LineRead::Eof => break,
            LineRead::TooLong => {
                warn!(%peer, "client sent oversized line ({MAX_LINE_BYTES} byte limit), closing connection");
                reader.get_mut().write_all(b"ERR line too long\n")?;
                reader.get_mut().flush()?;
                break;
            }
            LineRead::Line(line) => line,
        };
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        if auth_enabled && authenticated_user.is_none() {
            authenticated_user = handle_auth_attempt(reader, peer, exec, &metrics, trimmed)?;
            if authenticated_user.is_none() {
                break;
            }
            continue;
        }

        if is_quit_cmd(trimmed) {
            info!(%peer, user = ?authenticated_user, "client quit");
            reader.get_mut().write_all(b"OK BYE\n")?;
            reader.get_mut().flush()?;
            break;
        }

        trace!(%peer, user = ?authenticated_user, query = %trimmed, "dispatching");
        let started = Instant::now();
        let response = handle_query(trimmed, exec, authenticated_user.as_deref());
        let elapsed = started.elapsed();
        let is_err = response.is_err();
        if is_err {
            metrics.record_error();
        }
        debug!(
            %peer,
            user = ?authenticated_user,
            elapsed_us = elapsed.as_micros() as u64,
            status = if is_err { "err" } else { "ok" },
            "handled query"
        );
        let response_line = format!("{response}\n");
        reader.get_mut().write_all(response_line.as_bytes())?;
        reader.get_mut().flush()?;
    }

    info!(%peer, user = ?authenticated_user, "client disconnected");
    Ok(())
}

/// Parse + dispatch one query line.  Returns the response line (without the
/// trailing newline).  Lock-routing: read-only statements take the shared
/// lock; everything else takes the exclusive lock.  When `caller` is `Some`,
/// the statement is executed via `exec_as` so per-user CRUDA permissions are
/// enforced.
fn handle_query(query: &str, exec: &Arc<RwLock<Executor>>, caller: Option<&str>) -> Response {
    let stmt = match parse(query) {
        Ok((rest, s)) if rest.trim().is_empty() => s,
        Ok((rest, _)) => return Response::Err(format!("trailing input: {rest:?}")),
        Err(e) => return Response::Err(format_parse_error(query, e)),
    };
    // Read-only: shared executor lock + per-DB read lock.
    // Registry write (CREATE DATABASE etc.): exclusive executor lock.
    // Data write (APPEND etc.): shared executor lock + per-DB write lock.
    // When a transaction is active data writes still use exec.write() so they are buffered.
    let needs_exclusive = !stmt.is_read_only()
        && (needs_registry_lock(&stmt)
            || exec
                .read()
                .expect("executor lock poisoned")
                .is_in_transaction());

    let result = match (stmt.is_read_only(), needs_exclusive, caller) {
        (true, _, Some(u)) => exec
            .read()
            .expect("executor lock poisoned")
            .exec_read_as(&stmt, u),
        (true, _, None) => exec
            .read()
            .expect("executor lock poisoned")
            .exec_read(&stmt),
        (false, true, Some(u)) => exec
            .write()
            .expect("executor lock poisoned")
            .exec_as(&stmt, u),
        (false, true, None) => exec.write().expect("executor lock poisoned").exec(&stmt),
        (false, false, Some(u)) => exec
            .read()
            .expect("executor lock poisoned")
            .exec_db_write_as(&stmt, u),
        (false, false, None) => exec
            .read()
            .expect("executor lock poisoned")
            .exec_db_write(&stmt),
    };
    Response::from_result(&result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use libtau::{ExecError, Perm, Stmt, User};
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn exec() -> Arc<RwLock<Executor>> {
        Arc::new(RwLock::new(Executor::new()))
    }

    /// Run a query and render the response to its wire line, for assertions
    /// against the protocol's exact string output.
    fn q(query: &str, e: &Arc<RwLock<Executor>>, caller: Option<&str>) -> String {
        handle_query(query, e, caller).to_string()
    }

    #[hegel::test]
    fn pbt_handle_query_never_panics(tc: TestCase) {
        let input = tc.draw(gs::text().max_size(256));
        let e = exec();
        let _ = q(&input, &e, None);
    }

    #[hegel::test]
    fn pbt_at_after_append_returns_encoded_value(tc: TestCase) {
        let v = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        let probe = tc.draw(gs::integers::<i64>().min_value(0).max_value(99));
        let e = exec();
        q("CREATE DATABASE main", &e, None);
        q("CREATE LENS x int", &e, None);
        assert_eq!(q(&format!("APPEND LENS x 0 100 {v}"), &e, None), "OK");
        assert_eq!(
            q(&format!("AT LENS x {probe}"), &e, None),
            format!("VAL i{v}")
        );
    }

    #[hegel::test]
    fn pbt_at_uncovered_timestamp_yields_nil(tc: TestCase) {
        let probe = tc.draw(gs::integers::<i64>().min_value(101).max_value(1_000_000));
        let e = exec();
        q("CREATE DATABASE main", &e, None);
        q("CREATE LENS x int", &e, None);
        q("APPEND LENS x 0 100 42", &e, None);
        assert_eq!(q(&format!("AT LENS x {probe}"), &e, None), "VAL NIL");
    }

    #[hegel::test]
    fn pbt_parse_failure_starts_with_err_parse(tc: TestCase) {
        let junk = tc.draw(gs::from_regex("[A-Z]{4,12}").fullmatch(true).filter(|s| {
            !matches!(
                s.as_str(),
                "CREATE"
                    | "DROP"
                    | "USE"
                    | "APPEND"
                    | "COPY"
                    | "DERIVE"
                    | "XDERIVE"
                    | "SHOW"
                    | "RANGE"
                    | "REDUCE"
                    | "GRANT"
                    | "REVOKE"
            )
        }));
        let line = format!("{junk} something");
        let r = q(&line, &exec(), None);
        assert!(r.starts_with("ERR "), "expected ERR, got {r:?}");
    }

    #[hegel::test]
    fn pbt_trailing_input_is_reported(tc: TestCase) {
        let extra = tc.draw(gs::from_regex("[A-Z][A-Z]+").fullmatch(true));
        let r = q(&format!("CREATE DATABASE a {extra}"), &exec(), None);
        assert!(r.starts_with("ERR trailing input"), "got: {r}");
    }

    #[test]
    fn empty_response_for_ddl() {
        let e = exec();
        assert_eq!(q("CREATE DATABASE main", &e, None), "OK");
        assert_eq!(q("CREATE LENS x int", &e, None), "OK");
    }

    #[test]
    fn range_response_lists_segments() {
        let e = exec();
        q("CREATE DATABASE main", &e, None);
        q("CREATE LENS x int", &e, None);
        q("APPEND LENS x 0 5 1", &e, None);
        q("APPEND LENS x 5 10 2", &e, None);
        assert_eq!(q("RANGE LENS x 0 10", &e, None), "RANGE 2; 0:5:i1; 5:10:i2");
    }

    #[test]
    fn execution_error_is_reported() {
        let r = q("CREATE LENS x int", &exec(), None);
        assert!(r.starts_with("ERR no active database"), "got: {r}");
    }

    #[test]
    fn read_only_router_picks_shared_lock() {
        let e = exec();
        q("CREATE DATABASE main", &e, None);
        q("CREATE LENS x int", &e, None);
        q("APPEND LENS x 0 100 7", &e, None);
        let mut handles = vec![];
        for _ in 0..8 {
            let e = e.clone();
            handles.push(thread::spawn(move || {
                assert_eq!(q("AT LENS x 50", &e, None), "VAL i7");
            }));
        }
        for h in handles {
            h.join().expect("reader thread panicked");
        }
    }

    #[test]
    fn is_read_only_routing_matches_stmt_kind() {
        assert!(
            Stmt::At {
                name: "x".into(),
                t: 0
            }
            .is_read_only()
        );
        assert!(
            Stmt::Range {
                name: "x".into(),
                start: 0,
                end: 1,
                filter: None,
                limit: None,
                offset: None,
            }
            .is_read_only()
        );
        assert!(!Stmt::CreateDatabase { name: "a".into() }.is_read_only());
        assert!(!Stmt::Drop { name: "x".into() }.is_read_only());
    }

    #[test]
    fn exec_read_rejects_mutations() {
        let e = exec();
        let guard = e.write().expect("test executor lock");
        let stmt = Stmt::CreateDatabase { name: "a".into() };
        assert!(matches!(
            guard.exec_read(&stmt),
            Err(ExecError::InvalidExpr(_))
        ));
    }

    #[hegel::test]
    fn pbt_parse_auth_line_roundtrips_for_valid_input(tc: TestCase) {
        let user = tc.draw(gs::from_regex("[a-z][a-z0-9_]{0,10}").fullmatch(true));
        let pass = tc.draw(gs::from_regex("[A-Za-z0-9!@#$%^&*]{1,16}").fullmatch(true));
        let line = format!("AUTH {user} {pass}");
        assert_eq!(parse_auth_line(&line), Some((user, pass)));
    }

    #[test]
    fn is_quit_cmd_recognizes_quit_and_exit() {
        assert!(is_quit_cmd("QUIT"));
        assert!(is_quit_cmd("quit"));
        assert!(is_quit_cmd("Quit"));
        assert!(is_quit_cmd("EXIT"));
        assert!(is_quit_cmd("exit"));
        assert!(is_quit_cmd("Exit"));
    }

    #[test]
    fn is_quit_cmd_rejects_others() {
        assert!(!is_quit_cmd(""));
        assert!(!is_quit_cmd("QUIT NOW"));
        assert!(!is_quit_cmd("AT LENS x 0"));
        assert!(!is_quit_cmd("CREATE DATABASE x"));
    }

    #[test]
    fn read_line_bounded_returns_eof_on_empty_input() {
        let mut cursor = io::Cursor::new(&b""[..]);
        assert!(matches!(
            read_line_bounded(&mut cursor, 16).unwrap(),
            LineRead::Eof
        ));
    }

    #[test]
    fn read_line_bounded_returns_line_within_limit() {
        let mut cursor = io::Cursor::new(&b"AT LENS x 0\n"[..]);
        match read_line_bounded(&mut cursor, 64).unwrap() {
            LineRead::Line(s) => assert_eq!(s, "AT LENS x 0\n"),
            other => panic!("expected Line, got {other:?}"),
        }
    }

    #[test]
    fn read_line_bounded_handles_line_without_trailing_newline_at_eof() {
        let mut cursor = io::Cursor::new(&b"QUIT"[..]);
        match read_line_bounded(&mut cursor, 64).unwrap() {
            LineRead::Line(s) => assert_eq!(s, "QUIT"),
            _ => panic!("expected Line"),
        }
    }

    #[test]
    fn read_line_bounded_flags_oversized_line_without_newline() {
        // 20 'a' bytes, no newline, limit of 8: must report TooLong.
        let mut cursor = io::Cursor::new(&b"aaaaaaaaaaaaaaaaaaaa"[..]);
        assert!(matches!(
            read_line_bounded(&mut cursor, 8).unwrap(),
            LineRead::TooLong
        ));
    }

    #[test]
    fn read_line_bounded_resyncs_on_next_call_after_oversized_line() {
        // First line is oversized (no newline within limit), second line is fine.
        let mut cursor = io::Cursor::new(&b"aaaaaaaaaaaaaaaaaaaa\nOK\n"[..]);
        assert!(matches!(
            read_line_bounded(&mut cursor, 8).unwrap(),
            LineRead::TooLong
        ));
        match read_line_bounded(&mut cursor, 8).unwrap() {
            LineRead::Line(s) => assert_eq!(s, "OK\n"),
            _ => panic!("expected Line for the second call"),
        }
    }

    #[test]
    fn read_line_bounded_flags_line_exactly_over_limit_with_newline() {
        // "aaaaaaaaa\n" is 10 bytes; limit 8 must reject it as TooLong.
        let mut cursor = io::Cursor::new(&b"aaaaaaaaa\nOK\n"[..]);
        assert!(matches!(
            read_line_bounded(&mut cursor, 8).unwrap(),
            LineRead::TooLong
        ));
        match read_line_bounded(&mut cursor, 8).unwrap() {
            LineRead::Line(s) => assert_eq!(s, "OK\n"),
            _ => panic!("expected Line for the second call"),
        }
    }

    fn connected_pair() -> (TcpStream, TcpStream, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test bind on localhost:0");
        let addr = listener.local_addr().expect("test listener has local addr");
        let client = TcpStream::connect(addr).expect("test connect to ephemeral listener");
        let (server, peer) = listener.accept().expect("test accept");
        (client, server, peer)
    }

    /// Send each of `lines` (without trailing `\n`) to the server, flushing
    /// after each, then read and return one response line per input line
    /// (trimmed, without the trailing `\n`).
    fn send_and_collect(client: &mut TcpStream, lines: &[&str]) -> Vec<String> {
        let mut writer = client.try_clone().expect("test clone stream for writer");
        let mut reader = BufReader::new(client.try_clone().expect("test clone stream for reader"));
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            writeln!(writer, "{line}").expect("test write line");
            writer.flush().expect("test flush");
            let mut resp = String::new();
            reader.read_line(&mut resp).expect("test read response");
            out.push(resp.trim().to_string());
        }
        out
    }

    #[test]
    fn run_query_loop_sends_ok_bye_on_quit() {
        let (client, server, peer) = connected_pair();
        let e = exec();
        thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let _ = run_query_loop(&mut reader, peer, &e, false);
        });
        let mut br = BufReader::new(client);
        writeln!(br.get_mut(), "QUIT").expect("test write QUIT");
        br.get_mut().flush().expect("test flush");
        let mut resp = String::new();
        br.read_line(&mut resp).expect("test read response");
        assert_eq!(resp.trim(), "OK BYE");
    }

    #[test]
    fn run_query_loop_closes_connection_on_oversized_line() {
        let (mut client, server, peer) = connected_pair();
        let e = exec();
        let handle = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            run_query_loop(&mut reader, peer, &e, false)
        });

        // One byte over the limit, with a trailing newline.
        let mut oversized = vec![b'a'; MAX_LINE_BYTES + 1];
        oversized.push(b'\n');
        client
            .write_all(&oversized)
            .expect("test write oversized line");

        let mut br = BufReader::new(&mut client);
        let mut resp = String::new();
        br.read_line(&mut resp).expect("test read response");
        assert_eq!(resp.trim(), "ERR line too long");

        // Server should close its side; the join should complete promptly.
        handle
            .join()
            .expect("server thread panicked")
            .expect("run_query_loop result");
    }

    #[test]
    fn run_query_loop_auth_rejects_non_auth_first_message() {
        let (client, server, peer) = connected_pair();
        let e = exec();
        {
            let mut g = e.write().expect("test executor lock");
            let mut grants = HashMap::new();
            grants.insert("*".to_string(), Perm::ALL);
            g.users_mut()
                .add(User::new("admin", "pw", grants))
                .expect("add test admin user");
        }
        thread::spawn(move || {
            let mut reader = BufReader::new(server);
            let _ = run_query_loop(&mut reader, peer, &e, true);
        });
        let mut br = BufReader::new(client);
        writeln!(br.get_mut(), "CREATE DATABASE nope").expect("test write");
        br.get_mut().flush().expect("test flush");
        let mut resp = String::new();
        br.read_line(&mut resp).expect("test read");
        assert!(resp.contains("ERR authentication required"), "got: {resp}");
    }

    #[test]
    fn run_query_loop_handles_authenticated_multi_statement_session() {
        let (mut client, server, peer) = connected_pair();
        let e = exec();
        {
            let mut g = e.write().expect("test executor lock");
            let mut grants = HashMap::new();
            grants.insert("*".to_string(), Perm::ALL);
            g.users_mut()
                .add(User::new("admin", "pw", grants))
                .expect("add test admin user");
        }
        let handle = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            run_query_loop(&mut reader, peer, &e, true)
        });

        let responses = send_and_collect(
            &mut client,
            &[
                "AUTH admin pw",
                "CREATE DATABASE main",
                "CREATE LENS x int",
                "APPEND LENS x 0 100 42",
                "AT LENS x 50",
                "AT LENS x 500",
                "QUIT",
            ],
        );

        assert_eq!(
            responses,
            vec!["OK", "OK", "OK", "OK", "VAL i42", "VAL NIL", "OK BYE",]
        );
        handle
            .join()
            .expect("server thread panicked")
            .expect("run_query_loop result");
    }

    #[test]
    fn run_query_loop_transaction_commit_applies_buffered_writes() {
        let (mut client, server, peer) = connected_pair();
        let e = exec();
        {
            let mut g = e.write().expect("test executor lock");
            let mut grants = HashMap::new();
            grants.insert("*".to_string(), Perm::ALL);
            g.users_mut()
                .add(User::new("admin", "pw", grants))
                .expect("add test admin user");
        }
        let handle = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            run_query_loop(&mut reader, peer, &e, true)
        });

        let responses = send_and_collect(
            &mut client,
            &[
                "AUTH admin pw",
                "CREATE DATABASE main",
                "CREATE LENS x int",
                "START TRANSACTION",
                "APPEND LENS x 0 100 7",
                "AT LENS x 50",
                "COMMIT",
                "AT LENS x 50",
                "QUIT",
            ],
        );

        assert_eq!(responses[0], "OK");
        assert_eq!(responses[1], "OK");
        assert_eq!(responses[2], "OK");
        assert_eq!(responses[3], "OK");
        assert_eq!(responses[4], "OK");
        assert_eq!(responses[5], "VAL NIL");
        assert_eq!(responses[6], "OK");
        assert_eq!(responses[7], "VAL i7");
        assert_eq!(responses[8], "OK BYE");

        handle
            .join()
            .expect("server thread panicked")
            .expect("run_query_loop result");
    }

    #[test]
    fn run_query_loop_transaction_rollback_discards_buffered_writes() {
        let (mut client, server, peer) = connected_pair();
        let e = exec();
        {
            let mut g = e.write().expect("test executor lock");
            let mut grants = HashMap::new();
            grants.insert("*".to_string(), Perm::ALL);
            g.users_mut()
                .add(User::new("admin", "pw", grants))
                .expect("add test admin user");
        }
        let handle = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            run_query_loop(&mut reader, peer, &e, true)
        });

        let responses = send_and_collect(
            &mut client,
            &[
                "AUTH admin pw",
                "CREATE DATABASE main",
                "CREATE LENS x int",
                "START TRANSACTION",
                "APPEND LENS x 0 100 7",
                "ROLLBACK",
                "AT LENS x 50",
                "QUIT",
            ],
        );

        assert_eq!(responses[0], "OK");
        assert_eq!(responses[1], "OK");
        assert_eq!(responses[2], "OK");
        assert_eq!(responses[3], "OK");
        assert_eq!(responses[4], "OK");
        assert_eq!(responses[5], "OK");
        assert_eq!(responses[6], "VAL NIL");
        assert_eq!(responses[7], "OK BYE");

        handle
            .join()
            .expect("server thread panicked")
            .expect("run_query_loop result");
    }

    #[test]
    fn run_query_loop_recovers_from_error_and_keeps_serving() {
        let (mut client, server, peer) = connected_pair();
        let e = exec();
        {
            let mut g = e.write().expect("test executor lock");
            let mut grants = HashMap::new();
            grants.insert("*".to_string(), Perm::ALL);
            g.users_mut()
                .add(User::new("admin", "pw", grants))
                .expect("add test admin user");
        }
        let handle = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            run_query_loop(&mut reader, peer, &e, true)
        });

        let responses = send_and_collect(
            &mut client,
            &[
                "AUTH admin pw",
                "CREATE DATABASE main",
                "AT LENS does_not_exist 0",
                "CREATE LENS x int",
                "AT LENS x 0",
                "QUIT",
            ],
        );

        assert_eq!(responses[0], "OK");
        assert_eq!(responses[1], "OK");
        assert!(responses[2].starts_with("ERR"), "got: {}", responses[2]);
        assert_eq!(responses[3], "OK");
        assert_eq!(responses[4], "VAL NIL");
        assert_eq!(responses[5], "OK BYE");

        handle
            .join()
            .expect("server thread panicked")
            .expect("run_query_loop result");
    }

    #[test]
    fn run_query_loop_unprivileged_user_gets_permission_denied() {
        let (mut client, server, peer) = connected_pair();
        let e = exec();
        {
            let mut g = e.write().expect("test executor lock");

            let mut admin_grants = HashMap::new();
            admin_grants.insert("*".to_string(), Perm::ALL);
            g.users_mut()
                .add(User::new("admin", "adminpw", admin_grants))
                .expect("add test admin user");

            let mut ro_grants = HashMap::new();
            ro_grants.insert("main".to_string(), Perm::R);
            g.users_mut()
                .add(User::new("reader", "readerpw", ro_grants))
                .expect("add test read-only user");
        }
        {
            assert_eq!(
                handle_query("CREATE DATABASE main", &e, Some("admin")).to_string(),
                "OK"
            );
            assert_eq!(
                handle_query("CREATE LENS x int", &e, Some("admin")).to_string(),
                "OK"
            );
        }

        let handle = thread::spawn(move || {
            let mut reader = BufReader::new(server);
            run_query_loop(&mut reader, peer, &e, true)
        });

        let responses = send_and_collect(
            &mut client,
            &[
                "AUTH reader readerpw",
                "AT LENS x 0",
                "APPEND LENS x 0 100 1",
                "AT LENS x 0",
                "QUIT",
            ],
        );

        assert_eq!(responses[0], "OK");
        assert_eq!(responses[1], "VAL NIL");
        assert!(responses[2].starts_with("ERR"), "got: {}", responses[2]);
        assert_eq!(responses[3], "VAL NIL");
        assert_eq!(responses[4], "OK BYE");

        handle
            .join()
            .expect("server thread panicked")
            .expect("run_query_loop result");
    }
}
