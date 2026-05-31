//! Structured simulation results.

use libharness::Tier;

pub struct Result {
    pub tier: Tier,
    pub seed: u64,
    pub rows: u64,
    pub faults: u64,
    pub ingest_ms: u64,
    pub query_ms: u64,
}

pub struct Report {
    pub ok: bool,
    pub result: Option<Result>,
    pub error: Option<String>,
}

impl Report {
    pub fn success(r: Result) -> Self {
        Self {
            ok: true,
            result: Some(r),
            error: None,
        }
    }

    pub fn failure(msg: String) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(msg),
        }
    }

    pub fn print(&self) {
        if self.ok {
            let r = self
                .result
                .as_ref()
                .expect("ok=true implies result is Some");
            println!(
                "PASS  tier={} seed={:#x} rows={} faults={} ingest={}ms query={}ms rows/s={:.0}",
                r.tier.name(),
                r.seed,
                r.rows,
                r.faults,
                r.ingest_ms,
                r.query_ms,
                r.rows as f64 / (r.ingest_ms as f64 / 1_000.0).max(0.001),
            );
        } else {
            eprintln!("FAIL  {}", self.error.as_deref().unwrap_or("unknown error"));
        }
    }
}
