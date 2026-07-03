use std::fs::File;
use std::io::{self, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use libtau::Kernel;
use rcgen::generate_simple_self_signed;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tracing::{debug, error, info, warn};

use crate::handler::run_query_loop;

pub fn build_tls_config(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
) -> io::Result<ServerConfig> {
    let (certs, private_key) = match (cert_path, key_path) {
        (Some(cp), Some(kp)) => {
            let cert_file = File::open(cp)?;
            let certs: Vec<CertificateDer<'static>> =
                rustls_pemfile::certs(&mut BufReader::new(cert_file))
                    .collect::<Result<_, _>>()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            let key_file = File::open(kp)?;
            let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "no private key found in file")
                })?;
            (certs, key)
        }
        (None, None) => {
            warn!(
                "no TLS cert/key provided - generating ephemeral self-signed certificate (not for production)"
            );
            let certified = generate_simple_self_signed(vec!["localhost".to_string()])
                .map_err(io::Error::other)?;
            let cert_der = CertificateDer::from(certified.cert.der().to_vec());
            let key_der =
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
            (vec![cert_der], key_der)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tls.cert and tls.key must both be set (or both omitted for ephemeral)",
            ));
        }
    };

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, private_key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn accept_loop(
    listener: TcpListener,
    kernel: Arc<Kernel>,
    tls_config: Option<Arc<ServerConfig>>,
    auth_enabled: bool,
    connection_limit: usize,
    idle_timeout: Option<Duration>,
) -> io::Result<()> {
    let active_connections = Arc::new(AtomicUsize::new(0));
    let shared_metrics = kernel.metrics();
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let peer = stream
                    .peer_addr()
                    .unwrap_or_else(|_| "0.0.0.0:0".parse().expect("fallback addr"));
                let in_flight = active_connections.fetch_add(1, Ordering::AcqRel) + 1;
                shared_metrics.set_active_connections(in_flight as u64);
                if in_flight > connection_limit {
                    shared_metrics.record_rejected_connection();
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    shared_metrics.set_active_connections((in_flight - 1) as u64);
                    warn!(%peer, connection_limit, "rejecting connection: server at capacity");
                    let mut s = stream;
                    let _ = s.write_all(b"ERR server at connection limit\n");
                    let _ = s.flush();
                    continue;
                }
                shared_metrics.connections.inc();
                debug!(%peer, in_flight, "accepted connection");
                let exec = kernel.clone();
                let tls = tls_config.clone();
                let active_clone = active_connections.clone();
                let metrics_clone = shared_metrics.clone();
                thread::Builder::new()
                    .name("tau-conn".into())
                    .spawn(move || {
                        if let Err(e) =
                            handle_connection(stream, peer, exec, tls, auth_enabled, idle_timeout)
                        {
                            warn!(error = %e, "connection ended with error");
                        }
                        let remaining = active_clone.fetch_sub(1, Ordering::AcqRel) - 1;
                        metrics_clone.set_active_connections(remaining as u64);
                    })
                    .expect("failed to spawn connection thread");
            }
            Err(e) => error!(error = %e, "accept failed"),
        }
    }
    Ok(())
}

pub fn handle_connection(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    exec: Arc<Kernel>,
    tls_config: Option<Arc<ServerConfig>>,
    auth_enabled: bool,
    idle_timeout: Option<Duration>,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    if let Some(timeout) = idle_timeout {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
    }
    info!(%peer, "client connected");

    if let Some(cfg) = tls_config {
        let conn = rustls::ServerConnection::new(cfg).map_err(io::Error::other)?;
        let tls_stream = rustls::StreamOwned::new(conn, stream);
        let mut reader = BufReader::new(tls_stream);
        run_query_loop(&mut reader, peer, &exec, auth_enabled)
    } else {
        let mut reader = BufReader::new(stream);
        run_query_loop(&mut reader, peer, &exec, auth_enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tls_config_fails_when_only_cert_provided() {
        let r = build_tls_config(Some(Path::new("/nonexistent/cert.pem")), None);
        assert!(r.is_err());
    }

    #[test]
    fn build_tls_config_fails_when_only_key_provided() {
        let r = build_tls_config(None, Some(Path::new("/nonexistent/key.pem")));
        assert!(r.is_err());
    }
}
