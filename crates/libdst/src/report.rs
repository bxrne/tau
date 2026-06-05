//! Simulation run results with structured divergence tracking.

use crate::divergence::Divergence;

/// Outcome of one simulation phase.
#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub errors: usize,
    pub ops_run: usize,
    /// First divergence observed during this phase, if any.
    pub first_divergence: Option<Divergence>,
}

impl RunResult {
    pub const fn passed(&self) -> bool {
        self.errors == 0
    }

    /// Record a divergence: increment `errors` and store the first one seen.
    pub fn record(&mut self, d: Divergence) {
        if self.first_divergence.is_none() {
            self.first_divergence = Some(d);
        }
        self.errors += 1;
    }
}

/// Aggregated outcome across multiple phases.
#[derive(Debug, Clone, Default)]
pub struct SuiteResult {
    pub errors: usize,
    pub ops_run: usize,
    /// First divergence across all phases, if any.
    pub first_divergence: Option<Divergence>,
}

impl SuiteResult {
    pub fn absorb(&mut self, phase: RunResult) {
        self.errors += phase.errors;
        self.ops_run += phase.ops_run;
        if self.first_divergence.is_none() {
            self.first_divergence = phase.first_divergence;
        }
    }

    pub const fn passed(&self) -> bool {
        self.errors == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_absorb_accumulates() {
        let mut suite = SuiteResult::default();
        suite.absorb(RunResult {
            errors: 2,
            ops_run: 10,
            first_divergence: None,
        });
        suite.absorb(RunResult {
            errors: 3,
            ops_run: 20,
            first_divergence: None,
        });
        assert_eq!(suite.errors, 5);
        assert_eq!(suite.ops_run, 30);
    }

    #[test]
    fn first_divergence_from_first_phase_wins() {
        let mut suite = SuiteResult::default();
        let d1 = Divergence::new(1, "a", 1, 2);
        let d2 = Divergence::new(2, "b", 3, 4);
        suite.absorb(RunResult {
            errors: 1,
            ops_run: 1,
            first_divergence: Some(d1.clone()),
        });
        suite.absorb(RunResult {
            errors: 1,
            ops_run: 1,
            first_divergence: Some(d2),
        });
        assert_eq!(suite.first_divergence.unwrap(), d1);
    }

    #[test]
    fn run_result_record_sets_first() {
        let mut r = RunResult::default();
        let d = Divergence::new(0, "x", 1, 2);
        r.record(d.clone());
        r.record(Divergence::new(1, "y", 3, 4));
        assert_eq!(r.errors, 2);
        assert_eq!(r.first_divergence.unwrap(), d);
    }
}
