//! TCP connection manager for the tauctl REPL.
//!
//! Holds a named pool of live connections to tau servers. Each connection is
//! line-oriented: send a statement (with trailing newline), read one response
//! line back. Plain TCP and TLS variants share the same `Connection` API; the
//! TLS variant uses rustls with **no certificate verification** so it works
//! against the tau server's default self-signed cert (this is appropriate for
//! dev/internal networks - do not point at an untrusted server).

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

enum Backend {
    Plain(BufReader<TcpStream>),
    // Boxed - rustls' ClientConnection holds a sizeable state machine; without
    // the indirection the whole Backend (and every Plain instance) would be
    // ~10 KiB and clippy's large-size-difference lint trips.
    Tls(Box<BufReader<StreamOwned<ClientConnection, TcpStream>>>),
}

impl Backend {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(r) => r.get_mut().write_all(buf),
            Self::Tls(r) => r.get_mut().write_all(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(r) => r.get_mut().flush(),
            Self::Tls(r) => r.get_mut().flush(),
        }
    }

    fn read_line(&mut self, buf: &mut String) -> io::Result<usize> {
        match self {
            Self::Plain(r) => r.read_line(buf),
            Self::Tls(r) => r.read_line(buf),
        }
    }
}

pub struct Connection {
    addr: String,
    tls: bool,
    backend: Backend,
}

impl Connection {
    /// Open a plain TCP connection.
    pub fn open(addr: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        Ok(Self {
            addr: addr.to_string(),
            tls: false,
            backend: Backend::Plain(BufReader::new(stream)),
        })
    }

    /// Open a TLS connection.  Uses a no-verify certificate verifier so the
    /// tau server's default self-signed cert is accepted.  Pass a hostname for
    /// SNI; `localhost` is a safe default for dev.
    pub fn open_tls(addr: &str, sni_hostname: &str) -> io::Result<Self> {
        let tcp = TcpStream::connect(addr)?;
        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        let server_name = ServerName::try_from(sni_hostname.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let client =
            ClientConnection::new(Arc::new(config), server_name).map_err(io::Error::other)?;
        let stream = StreamOwned::new(client, tcp);
        Ok(Self {
            addr: addr.to_string(),
            tls: true,
            backend: Backend::Tls(Box::new(BufReader::new(stream))),
        })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// Send `line` (a `\n` is appended) and return the first response line.
    /// Trailing newline is stripped.  Returns `Err(UnexpectedEof)` if the
    /// server closed the connection without replying.
    pub fn send(&mut self, line: &str) -> io::Result<String> {
        self.backend.write_all(line.as_bytes())?;
        self.backend.write_all(b"\n")?;
        self.backend.flush()?;

        let mut resp = String::new();
        let n = self.backend.read_line(&mut resp)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed connection",
            ));
        }
        Ok(resp.trim_end_matches(['\r', '\n']).to_string())
    }
}

/// Server certificate verifier that accepts any cert.  Used so tauctl can
/// connect to a tau server presenting an ephemeral self-signed cert.  This is
/// safe for dev/internal traffic only - over an untrusted network it gives
/// no protection against an MITM.
#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

// Suppress unused import warning when only one Backend variant is constructed.
#[allow(dead_code)]
fn _backend_read_witness<R: Read>(_: R) {}

pub struct TcpManager {
    conns: HashMap<String, Connection>,
    active: Option<String>,
}

impl TcpManager {
    pub fn new() -> Self {
        Self {
            conns: HashMap::new(),
            active: None,
        }
    }

    /// Open a new plain-TCP connection and register it under `name`.
    /// Becomes the active connection if none was active yet.
    pub fn connect(&mut self, name: &str, addr: &str) -> io::Result<()> {
        let conn = Connection::open(addr)?;
        self.insert(name, conn);
        Ok(())
    }

    /// Open a new TLS connection and register it under `name`.  Uses a
    /// no-verify cert verifier - see [`Connection::open_tls`] for the
    /// security caveat.
    pub fn connect_tls(&mut self, name: &str, addr: &str, sni: &str) -> io::Result<()> {
        let conn = Connection::open_tls(addr, sni)?;
        self.insert(name, conn);
        Ok(())
    }

    fn insert(&mut self, name: &str, conn: Connection) {
        self.conns.insert(name.to_string(), conn);
        if self.active.is_none() {
            self.active = Some(name.to_string());
        }
    }

    /// Close the named connection.  If it was the active one, the active
    /// pointer slides to whichever connection is enumerated first (if any).
    pub fn disconnect(&mut self, name: &str) -> Result<(), String> {
        if self.conns.remove(name).is_none() {
            return Err(format!("no such connection: {}", name));
        }
        if self.active.as_deref() == Some(name) {
            self.active = self.conns.keys().next().cloned();
        }
        Ok(())
    }

    /// Switch the active connection by name.
    pub fn set_active(&mut self, name: &str) -> Result<(), String> {
        if !self.conns.contains_key(name) {
            return Err(format!("no such connection: {}", name));
        }
        self.active = Some(name.to_string());
        Ok(())
    }

    /// Mutable handle to the active connection, if any.
    pub fn active_mut(&mut self) -> Option<&mut Connection> {
        let name = self.active.as_ref()?.clone();
        self.conns.get_mut(&name)
    }

    /// Mutable handle to the named connection, regardless of active.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Connection> {
        self.conns.get_mut(name)
    }

    /// `(name, addr, is_active, is_tls)` snapshot of every registered
    /// connection.
    pub fn list(&self) -> Vec<(String, String, bool, bool)> {
        let mut out: Vec<_> = self
            .conns
            .iter()
            .map(|(n, c)| {
                (
                    n.clone(),
                    c.addr().to_string(),
                    self.active.as_deref() == Some(n.as_str()),
                    c.is_tls(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::thread::JoinHandle;

    /// One-shot echo listener: accepts one connection, replies to each line
    /// with the line wrapped in `RESP: …`.  Exits when the client closes.
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
                    writeln!(writer, "RESP: {}", buf.trim_end_matches(['\r', '\n'])).unwrap();
                    buf.clear();
                }
            }
        });
        (addr, handle)
    }

    /// Listener that accepts then immediately closes - used to test the
    /// "server hung up" path on the very first send.
    fn closing_listener() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        (addr, handle)
    }

    #[test]
    fn manager_new_is_empty() {
        let m = TcpManager::new();
        assert!(m.list().is_empty());
    }

    #[test]
    fn connect_registers_and_activates_first_connection() {
        let (addr, _h) = echo_listener();
        let mut m = TcpManager::new();
        m.connect("dev", &addr.to_string()).unwrap();
        assert_eq!(
            m.list(),
            vec![("dev".to_string(), addr.to_string(), true, false)]
        );
    }

    #[test]
    fn second_connect_does_not_steal_active() {
        let (a, _ha) = echo_listener();
        let (b, _hb) = echo_listener();
        let mut m = TcpManager::new();
        m.connect("a", &a.to_string()).unwrap();
        m.connect("b", &b.to_string()).unwrap();
        let active: Vec<String> = m
            .list()
            .into_iter()
            .filter(|(_, _, on, _)| *on)
            .map(|(n, _, _, _)| n)
            .collect();
        assert_eq!(active, vec!["a".to_string()]);
    }

    #[test]
    fn set_active_switches_when_known() {
        let (a, _ha) = echo_listener();
        let (b, _hb) = echo_listener();
        let mut m = TcpManager::new();
        m.connect("a", &a.to_string()).unwrap();
        m.connect("b", &b.to_string()).unwrap();
        m.set_active("b").unwrap();
        assert!(m.list().iter().any(|(n, _, on, _)| n == "b" && *on));
    }

    #[test]
    fn set_active_rejects_unknown_name() {
        let mut m = TcpManager::new();
        assert!(m.set_active("ghost").is_err());
    }

    #[test]
    fn disconnect_rejects_unknown_name() {
        let mut m = TcpManager::new();
        assert!(m.disconnect("ghost").is_err());
    }

    #[test]
    fn disconnect_active_slides_to_next() {
        let (a, _ha) = echo_listener();
        let (b, _hb) = echo_listener();
        let mut m = TcpManager::new();
        m.connect("a", &a.to_string()).unwrap();
        m.connect("b", &b.to_string()).unwrap();
        // a is active; drop it and check that some other connection becomes active.
        m.disconnect("a").unwrap();
        let actives: Vec<_> = m.list().into_iter().filter(|(_, _, on, _)| *on).collect();
        assert_eq!(actives.len(), 1);
        assert_eq!(actives[0].0, "b");
    }

    #[test]
    fn disconnect_last_leaves_no_active() {
        let (a, _ha) = echo_listener();
        let mut m = TcpManager::new();
        m.connect("a", &a.to_string()).unwrap();
        m.disconnect("a").unwrap();
        assert!(m.active_mut().is_none());
        assert!(m.list().is_empty());
    }

    #[test]
    fn active_mut_returns_none_when_empty() {
        let mut m = TcpManager::new();
        assert!(m.active_mut().is_none());
    }

    #[test]
    fn list_is_sorted_by_name() {
        let (a, _ha) = echo_listener();
        let (b, _hb) = echo_listener();
        let (c, _hc) = echo_listener();
        let mut m = TcpManager::new();
        m.connect("c", &c.to_string()).unwrap();
        m.connect("a", &a.to_string()).unwrap();
        m.connect("b", &b.to_string()).unwrap();
        let names: Vec<_> = m.list().into_iter().map(|(n, _, _, _)| n).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn connection_send_round_trips_against_echo_server() {
        let (addr, _h) = echo_listener();
        let mut c = Connection::open(&addr.to_string()).unwrap();
        let resp = c.send("hello").unwrap();
        assert_eq!(resp, "RESP: hello");
    }

    #[test]
    fn connection_send_returns_error_when_server_closes() {
        // The exact ErrorKind is OS-dependent - kernel may surface the
        // server-side close as UnexpectedEof on read, or ConnectionReset /
        // BrokenPipe on the write that races it.  Contract is just "Err".
        let (addr, _h) = closing_listener();
        let mut c = Connection::open(&addr.to_string()).unwrap();
        assert!(c.send("hi").is_err());
    }

    #[test]
    fn connection_open_fails_on_unreachable_addr() {
        // Port 1 is reserved and unbindable on every modern OS; connect must fail
        // synchronously without hanging.
        let result = Connection::open("127.0.0.1:1");
        assert!(result.is_err());
    }
}
