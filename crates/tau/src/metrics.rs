use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use libtau::Metrics;
use tracing::{debug, error, info, trace, warn};

/// Minimal HTTP server that serves `GET /metrics` in Prometheus text format.
///
/// One thread per request; fine for scrape intervals >= 1 s. The listener
/// runs until the process exits. Every accepted request is traced at
/// `trace` (method, path, peer) and at `debug` (status, bytes, duration).
pub(crate) fn serve_metrics_http(port: u16, metrics: Arc<Metrics>) {
    let listener = match TcpListener::bind(format!("0.0.0.0:{port}")) {
        Ok(l) => l,
        Err(e) => {
            error!(port, error = %e, "could not bind metrics listener");
            return;
        }
    };
    info!(port, "metrics HTTP listener ready");
    for stream in listener.incoming() {
        let mut s = match stream {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "metrics accept failed");
                continue;
            }
        };
        let m = metrics.clone();
        thread::Builder::new()
            .name("tau-metrics-req".into())
            .spawn(move || {
                handle_metrics_request(&mut s, &m);
            })
            .expect("failed to spawn metrics request thread");
    }
}

/// Parse the request line, render Prometheus text, write the HTTP response.
/// Unknown paths return 404 so misconfigured scrapers fail loudly.
fn handle_metrics_request(s: &mut TcpStream, metrics: &Arc<Metrics>) {
    let started = Instant::now();
    let peer = s
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());

    // Read the request line. Bounded by 4 KiB so a malicious client cannot
    // grow the buffer arbitrarily.
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    let n = match s.read(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            warn!(%peer, error = %e, "metrics read failed");
            return;
        }
    };
    if n == 0 {
        return;
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let request_line = head.lines().next().unwrap_or("").to_string();
    trace!(%peer, request = %request_line, "metrics request");

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let (status, body, ctype) = match (method, path) {
        ("GET", "/metrics") | ("HEAD", "/metrics") => (
            "200 OK",
            if method == "HEAD" {
                String::new()
            } else {
                metrics.prometheus_text()
            },
            "text/plain; version=0.0.4; charset=utf-8",
        ),
        ("GET", "/healthz") | ("GET", "/") => (
            "200 OK",
            "tau metrics endpoint ready\n".to_string(),
            "text/plain; charset=utf-8",
        ),
        ("GET", _) | ("HEAD", _) => (
            "404 Not Found",
            "not found\n".to_string(),
            "text/plain; charset=utf-8",
        ),
        _ => (
            "405 Method Not Allowed",
            "method not allowed\n".to_string(),
            "text/plain; charset=utf-8",
        ),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Err(e) = s.write_all(response.as_bytes()) {
        warn!(%peer, error = %e, "metrics write failed");
        return;
    }
    let _ = s.flush();
    debug!(
        %peer,
        method,
        path,
        status,
        bytes = body.len(),
        elapsed_us = started.elapsed().as_micros() as u64,
        "metrics request handled"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use libtau::Kernel;

    fn metrics_response(request: &[u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test metrics bind");
        let addr = listener.local_addr().expect("test metrics addr");
        let metrics = Kernel::new().metrics();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test metrics accept");
            handle_metrics_request(&mut stream, &metrics);
        });
        let mut client = TcpStream::connect(addr).expect("test metrics connect");
        client
            .write_all(request)
            .expect("test metrics request write");
        client.flush().expect("test metrics flush");
        let mut resp = String::new();
        client
            .read_to_string(&mut resp)
            .expect("test metrics response read");
        resp
    }

    #[test]
    fn metrics_get_metrics_returns_200_with_content_type() {
        let resp = metrics_response(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        assert!(resp.contains("text/plain; version=0.0.4"), "got: {resp}");
    }

    #[test]
    fn metrics_get_healthz_returns_200() {
        let resp = metrics_response(b"GET /healthz HTTP/1.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        assert!(resp.contains("tau metrics endpoint ready"), "got: {resp}");
    }

    #[test]
    fn metrics_get_root_returns_200() {
        let resp = metrics_response(b"GET / HTTP/1.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
    }

    #[test]
    fn metrics_unknown_path_returns_404() {
        let resp = metrics_response(b"GET /nope HTTP/1.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 404 Not Found"), "got: {resp}");
    }

    #[test]
    fn metrics_post_returns_405() {
        let resp = metrics_response(b"POST /metrics HTTP/1.1\r\n\r\n");
        assert!(
            resp.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "got: {resp}"
        );
    }

    #[test]
    fn metrics_head_metrics_returns_200_empty_body() {
        let resp = metrics_response(b"HEAD /metrics HTTP/1.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        let body_start = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(resp.len());
        assert_eq!(&resp[body_start..], "", "HEAD must have empty body");
    }
}
