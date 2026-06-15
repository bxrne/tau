//! Runs a [`Workload`](crate::workload::Workload) against either the
//! in-process engine or a live wire connection, timing each `measure`
//! statement individually.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libtau::Executor;
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use serde::Serialize;

use crate::workload::Workload;

#[derive(Debug, Clone, Serialize)]
pub struct WorkloadResult {
    pub workload: String,
    pub cell: String,
    pub layer: String,
    pub ops: usize,
    pub total_secs: f64,
    pub throughput_ops_sec: f64,
    pub p50_us: f64,
    pub p99_us: f64,
}

impl WorkloadResult {
    fn from_durations(
        workload: &str,
        cell: &str,
        layer: &str,
        mut durations: Vec<Duration>,
    ) -> Self {
        let ops = durations.len();
        let total: Duration = durations.iter().sum();
        durations.sort_unstable();
        let pct = |p: f64| -> f64 {
            if durations.is_empty() {
                return 0.0;
            }
            let idx = ((durations.len() as f64 - 1.0) * p).round() as usize;
            durations[idx].as_secs_f64() * 1e6
        };
        let throughput = if total.as_secs_f64() > 0.0 {
            ops as f64 / total.as_secs_f64()
        } else {
            0.0
        };
        Self {
            workload: workload.to_string(),
            cell: cell.to_string(),
            layer: layer.to_string(),
            ops,
            total_secs: total.as_secs_f64(),
            throughput_ops_sec: throughput,
            p50_us: pct(0.50),
            p99_us: pct(0.99),
        }
    }
}

/// Apply a workload's `setup` statements (untimed) to an `Executor`.
pub fn apply_setup(executor: &mut Executor, workload: &Workload) {
    for line in &workload.setup {
        exec_line(executor, line);
    }
}

/// Run a workload's `setup` (untimed) then `measure` (timed, one duration per
/// statement) directly against an `Executor`.
pub fn run_engine(executor: &mut Executor, workload: &Workload, cell: &str) -> WorkloadResult {
    apply_setup(executor, workload);
    measure_engine(executor, workload, cell)
}

/// Time a workload's `measure` statements against an already-set-up
/// `Executor`. Use [`apply_setup`] first if the executor hasn't been
/// populated yet.
pub fn measure_engine(executor: &mut Executor, workload: &Workload, cell: &str) -> WorkloadResult {
    let mut durations = Vec::with_capacity(workload.measure.len());
    for line in &workload.measure {
        let start = Instant::now();
        exec_line(executor, line);
        durations.push(start.elapsed());
    }
    WorkloadResult::from_durations(workload.name, cell, "engine", durations)
}

fn exec_line(executor: &mut Executor, line: &str) {
    let (_, stmt) =
        libtau::parse(line).unwrap_or_else(|e| panic!("parse failed for {line:?}: {e:?}"));
    executor
        .exec(&stmt)
        .unwrap_or_else(|e| panic!("exec failed for {line:?}: {e:?}"));
}

/// Run a workload's `setup` (untimed) then `measure` (timed, one duration per
/// round trip) over a TCP connection to `addr`.
pub fn run_wire(
    addr: std::net::SocketAddr,
    tls: bool,
    auth: bool,
    workload: &Workload,
    cell: &str,
) -> io::Result<WorkloadResult> {
    let mut conn = WireConn::connect(addr, tls)?;

    if auth {
        let resp = conn.send_line("AUTH admin admin")?;
        if !resp.starts_with("OK") {
            return Err(io::Error::other(format!("AUTH failed: {resp}")));
        }
    }

    for line in &workload.setup {
        let resp = conn.send_line(line)?;
        if resp.starts_with("ERR") {
            return Err(io::Error::other(format!("setup {line:?} failed: {resp}")));
        }
    }

    let mut durations = Vec::with_capacity(workload.measure.len());
    for line in &workload.measure {
        let start = Instant::now();
        let resp = conn.send_line(line)?;
        durations.push(start.elapsed());
        if resp.starts_with("ERR") {
            return Err(io::Error::other(format!("measure {line:?} failed: {resp}")));
        }
    }

    Ok(WorkloadResult::from_durations(
        workload.name,
        cell,
        "wire",
        durations,
    ))
}

enum Stream {
    Plain(BufReader<TcpStream>),
    Tls(Box<BufReader<StreamOwned<ClientConnection, TcpStream>>>),
}

struct WireConn {
    stream: Stream,
}

impl WireConn {
    fn connect(addr: std::net::SocketAddr, tls: bool) -> io::Result<Self> {
        let tcp = TcpStream::connect(addr)?;
        tcp.set_nodelay(true)?;
        let stream = if tls {
            let config = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth();
            let server_name = ServerName::try_from("localhost".to_string())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            let client =
                ClientConnection::new(Arc::new(config), server_name).map_err(io::Error::other)?;
            Stream::Tls(Box::new(BufReader::new(StreamOwned::new(client, tcp))))
        } else {
            Stream::Plain(BufReader::new(tcp))
        };
        Ok(Self { stream })
    }

    fn send_line(&mut self, line: &str) -> io::Result<String> {
        let msg = format!("{line}\n");
        match &mut self.stream {
            Stream::Plain(r) => {
                r.get_mut().write_all(msg.as_bytes())?;
                r.get_mut().flush()?;
            }
            Stream::Tls(r) => {
                r.get_mut().write_all(msg.as_bytes())?;
                r.get_mut().flush()?;
            }
        }
        let mut resp = String::new();
        match &mut self.stream {
            Stream::Plain(r) => {
                r.read_line(&mut resp)?;
            }
            Stream::Tls(r) => {
                r.read_line(&mut resp)?;
            }
        }
        Ok(resp.trim_end().to_string())
    }
}

/// Accepts any server certificate. The bench wire layer connects to an
/// `EphemeralServer` presenting a self-signed cert generated for the
/// lifetime of the benchmark; there is nothing to verify it against.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::ConfigCell;
    use crate::workload::{ValueType, WorkloadKind};

    #[test]
    fn run_engine_completes_for_every_workload() {
        for kind in WorkloadKind::all() {
            let dir = tempfile::tempdir().expect("tempdir");
            let cell = ConfigCell::default();
            let mut executor = cell.build_executor(dir.path()).expect("executor");
            let workload = kind.build(5, ValueType::Int, crate::workload::DEFAULT_SEED);
            let result = run_engine(&mut executor, &workload, cell.name);
            assert_eq!(result.ops, workload.measure.len());
        }
    }

    #[test]
    fn run_wire_completes_against_ephemeral_server() {
        use std::sync::RwLock;
        let dir = tempfile::tempdir().expect("tempdir");
        let cell = ConfigCell::default();
        let executor = cell.build_executor(dir.path()).expect("executor");
        let executor = Arc::new(RwLock::new(executor));
        let server = tau::harness::EphemeralServer::spawn(executor, cell.harness_opts())
            .expect("spawn ephemeral server");
        let workload =
            WorkloadKind::AppendHeavy.build(5, ValueType::Int, crate::workload::DEFAULT_SEED);
        let result =
            run_wire(server.addr, cell.tls, cell.auth, &workload, cell.name).expect("run_wire");
        assert_eq!(result.ops, workload.measure.len());
    }
}
