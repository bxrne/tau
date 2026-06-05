//! Generic dual-model simulation trait.

use rand::rngs::StdRng;

use crate::divergence::Divergence;

/// Outcome of a periodic checkpoint (fault injection / restart).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointAction {
    /// Keep the op log; target and model state reflect whatever the hook established.
    Continue {
        /// Divergences detected during checkpoint replay.
        divergences: Vec<Divergence>,
    },
    /// Clear the op log after the hook; report any replay failures via `divergences`.
    ResetLog { divergences: Vec<Divergence> },
}

/// A reproducible workload driving a target and an isolated reference model in lock-step.
pub trait DualSimulation {
    type Op: Clone;

    /// Draw the next operation (e.g. from a behavior tree).
    fn pick(&mut self, rng: &mut StdRng) -> Self::Op;

    /// Apply one op to the target and reference; return any newly detected divergences.
    fn apply(&mut self, step: usize, op: &Self::Op) -> Vec<Divergence>;

    /// Inject a fault / restart at a checkpoint. `checkpoint` is 1-based.
    fn checkpoint(
        &mut self,
        step: usize,
        checkpoint: usize,
        log: &mut Vec<Self::Op>,
        rng: &mut StdRng,
    ) -> CheckpointAction;
}
