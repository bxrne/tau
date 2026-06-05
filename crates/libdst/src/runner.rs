//! Generic sequential DST loop with structured divergence tracking.

use rand::rngs::StdRng;

use crate::report::RunResult;
use crate::sim::{CheckpointAction, DualSimulation};

/// Options for [`run_sequential`].
#[derive(Debug, Clone)]
pub struct SequentialOpts {
    pub n_ops: usize,
    /// When set, [`DualSimulation::checkpoint`] is invoked every N operations (after op 0).
    pub checkpoint_every: Option<usize>,
}

/// Run a seeded sequential dual simulation.
pub fn run_sequential<Sim>(opts: SequentialOpts, rng: &mut StdRng, sim: &mut Sim) -> RunResult
where
    Sim: DualSimulation,
{
    let mut result = RunResult::default();
    let mut checkpoint_count = 0usize;
    let mut op_log: Vec<Sim::Op> = Vec::new();

    for step in 0..opts.n_ops {
        if let Some(every) = opts.checkpoint_every
            && step > 0
            && step.is_multiple_of(every)
        {
            checkpoint_count += 1;
            match sim.checkpoint(step, checkpoint_count, &mut op_log, rng) {
                CheckpointAction::Continue { divergences } => {
                    for d in divergences {
                        result.record(d);
                    }
                }
                CheckpointAction::ResetLog { divergences } => {
                    for d in divergences {
                        result.record(d);
                    }
                    op_log.clear();
                }
            }
        }

        let op = sim.pick(rng);
        for d in sim.apply(step, &op) {
            result.record(d);
        }
        op_log.push(op);
        result.ops_run += 1;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::divergence::Divergence;
    use crate::sim::CheckpointAction;
    use rand::Rng;
    use rand::SeedableRng;

    struct CountingSim;

    impl DualSimulation for CountingSim {
        type Op = u8;

        fn pick(&mut self, rng: &mut StdRng) -> u8 {
            rng.gen_range(0..3)
        }

        fn apply(&mut self, step: usize, op: &u8) -> Vec<Divergence> {
            if *op > 0 {
                vec![Divergence::new(step, "nonzero", 0u8, *op)]
            } else {
                vec![]
            }
        }

        fn checkpoint(
            &mut self,
            _step: usize,
            _n: usize,
            _log: &mut Vec<u8>,
            _rng: &mut StdRng,
        ) -> CheckpointAction {
            CheckpointAction::Continue {
                divergences: vec![],
            }
        }
    }

    #[test]
    fn run_sequential_accumulates_checkpoint_continue_errors() {
        struct CheckpointErrSim;

        impl DualSimulation for CheckpointErrSim {
            type Op = u8;

            fn pick(&mut self, rng: &mut StdRng) -> u8 {
                rng.gen_range(0..2)
            }

            fn apply(&mut self, _step: usize, _op: &u8) -> Vec<Divergence> {
                vec![]
            }

            fn checkpoint(
                &mut self,
                step: usize,
                _n: usize,
                _log: &mut Vec<u8>,
                _rng: &mut StdRng,
            ) -> CheckpointAction {
                CheckpointAction::Continue {
                    divergences: vec![
                        Divergence::new(step, "a", 1, 2),
                        Divergence::new(step, "b", 3, 4),
                        Divergence::new(step, "c", 5, 6),
                    ],
                }
            }
        }

        let mut sim = CheckpointErrSim;
        let mut rng = StdRng::seed_from_u64(1);
        let r = run_sequential(
            SequentialOpts {
                n_ops: 401,
                checkpoint_every: Some(200),
            },
            &mut rng,
            &mut sim,
        );
        assert_eq!(r.errors, 6, "checkpoints at steps 200 and 400 → 3+3");
        assert!(r.first_divergence.is_some());
    }

    #[test]
    fn run_sequential_accumulates_apply_errors() {
        let mut sim = CountingSim;
        let mut rng = StdRng::seed_from_u64(9);
        let r = run_sequential(
            SequentialOpts {
                n_ops: 4,
                checkpoint_every: None,
            },
            &mut rng,
            &mut sim,
        );
        assert_eq!(r.ops_run, 4);
        assert!(r.errors > 0);
    }

    #[test]
    fn ops_run_equals_n_ops() {
        let mut sim = CountingSim;
        let mut rng = StdRng::seed_from_u64(0);
        let r = run_sequential(
            SequentialOpts {
                n_ops: 100,
                checkpoint_every: None,
            },
            &mut rng,
            &mut sim,
        );
        assert_eq!(r.ops_run, 100);
    }
}
