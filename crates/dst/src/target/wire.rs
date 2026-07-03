//! TCP/TLS wire client for tau DST.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

use libtau::{ExecError, Output, Response};
use rustls::{
    ClientConfig, ClientConnection, StreamOwned,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::profile::Transport;
use crate::profile::spec::Auth;

/// DST admin credentials when [`Auth::On`].
pub const DST_AUTH_USER: &str = "dst_admin";
pub const DST_AUTH_PASS: &str = "dst_secret";

enum Backend {
    Plain(BufReader<TcpStream>),
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
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}

/// Line-oriented tau wire target (one persistent connection per profile run).
pub struct WireClient {
    backend: Backend,
    auth: Auth,
    authenticated: bool,
    in_transaction: bool,
}

impl WireClient {
    pub fn connect(addr: &str, transport: Transport, auth: Auth) -> io::Result<Self> {
        let backend = match transport {
            Transport::WirePlain => {
                let stream = TcpStream::connect(addr)?;
                stream.set_nodelay(true)?;
                Backend::Plain(BufReader::new(stream))
            }
            Transport::WireTls => {
                let tcp = TcpStream::connect(addr)?;
                tcp.set_nodelay(true)?;
                let config = ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoVerify))
                    .with_no_client_auth();
                let server_name = ServerName::try_from("localhost".to_string())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
                let client = ClientConnection::new(Arc::new(config), server_name)
                    .map_err(io::Error::other)?;
                Backend::Tls(Box::new(BufReader::new(StreamOwned::new(client, tcp))))
            }
            Transport::Direct => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "WireClient requires WirePlain or WireTls",
                ));
            }
        };
        Ok(Self {
            backend,
            auth,
            authenticated: false,
            in_transaction: false,
        })
    }

    /// Carry the logical transaction state onto a freshly reconnected client.
    /// tau buffers transactions in the shared kernel (not per-connection), so
    /// after a connection drop the server's open transaction survives; the new
    /// client must inherit that flag to stay in step with the server and oracle.
    pub fn set_in_transaction(&mut self, in_transaction: bool) {
        self.in_transaction = in_transaction;
    }

    fn send_raw(&mut self, line: &str) -> io::Result<String> {
        if matches!(self.auth, Auth::On) && !self.authenticated {
            let auth_line = format!("AUTH {DST_AUTH_USER} {DST_AUTH_PASS}");
            self.backend.write_all(auth_line.as_bytes())?;
            self.backend.write_all(b"\n")?;
            self.backend.flush()?;
            let mut resp = String::new();
            let n = self.backend.read_line(&mut resp)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "auth: server closed",
                ));
            }
            if resp.trim() != "OK" {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("auth failed: {}", resp.trim()),
                ));
            }
            self.authenticated = true;
        }

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

impl super::Target for WireClient {
    fn exec(&mut self, sql: &str) -> Result<Output, ExecError> {
        let raw = self
            .send_raw(sql)
            .map_err(|e| ExecError::Io(e.to_string()))?;
        let resp = Response::parse(&raw).map_err(|e| ExecError::Io(e.0))?;
        let out = resp.into_output()?;
        match sql.trim().to_ascii_uppercase().as_str() {
            "START TRANSACTION" => self.in_transaction = true,
            "COMMIT" | "ROLLBACK" => self.in_transaction = false,
            _ => {}
        }
        Ok(out)
    }

    fn is_in_transaction(&self) -> bool {
        self.in_transaction
    }
}
