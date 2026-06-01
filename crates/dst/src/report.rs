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

#[cfg(test)]
mod tests {
    use super::*;
    use libharness::Tier;
    use pretty_assertions::assert_eq;

    #[test]
    fn success_stores_all_result_fields() {
        let r = Report::success(Result {
            tier: Tier::Nano,
            seed: 42,
            rows: 100,
            faults: 2,
            ingest_ms: 50,
            query_ms: 10,
        });
        assert!(r.ok);
        assert!(r.error.is_none());
        let res = r.result.unwrap();
        assert_eq!(res.rows, 100);
        assert_eq!(res.faults, 2);
        assert_eq!(res.seed, 42);
        assert_eq!(res.ingest_ms, 50);
        assert_eq!(res.query_ms, 10);
    }

    #[test]
    fn failure_stores_error_message() {
        let r = Report::failure("something broke".to_string());
        assert!(!r.ok);
        assert!(r.result.is_none());
        assert_eq!(r.error.as_deref(), Some("something broke"));
    }

    #[test]
    fn print_success_does_not_panic() {
        Report::success(Result {
            tier: Tier::Nano,
            seed: 0,
            rows: 1_000,
            faults: 0,
            ingest_ms: 10,
            query_ms: 5,
        })
        .print();
    }

    #[test]
    fn print_failure_does_not_panic() {
        Report::failure("bad".to_string()).print();
    }

    #[test]
    fn print_zero_ingest_ms_does_not_divide_by_zero() {
        Report::success(Result {
            tier: Tier::Nano,
            seed: 0,
            rows: 0,
            faults: 0,
            ingest_ms: 0,
            query_ms: 0,
        })
        .print();
    }
}
