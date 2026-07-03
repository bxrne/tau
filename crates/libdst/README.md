# libdst

## What it is

A generic deterministic-simulation-testing framework — not tied to any engine. It compares a **target** (system under test) against an isolated **reference model** under a reproducible seeded workload; any mismatch is a structured `Divergence` (step, description, expected, got), and the seed replays the exact failing sequence.

## How it works

You implement `DualSimulation` for your op type: `pick` draws the next op (typically from `btree::Tree`, a closure-based weighted selector with runtime tag exclusion), `apply` runs it against target and model and returns divergences, and `checkpoint` injects restarts or file damage, returning `Continue` (keep the op log) or `ResetLog` (history destroyed). `run_sequential` drives the pick → apply → checkpoint loop and accumulates a `RunResult` with the first divergence.

Supporting pieces: `Clock` (atomic virtual time), `Scheduler` (deterministic cooperative tasks — a seeded RNG picks who runs next, no OS threads), `shrink`/`shrink_with_granularity` (delta-debug trace minimisation), and `faults` (`truncate_file` — a short write — and `corrupt_file` — a length-preserving bit-flip run).

## Using it

```rust
use libdst::{CheckpointAction, Divergence, DualSimulation, SequentialOpts, run_sequential};

impl DualSimulation for MySim {
    type Op = MyOp;
    fn pick(&mut self, rng: &mut StdRng) -> MyOp { /* behavior tree */ }
    fn apply(&mut self, step: usize, op: &MyOp) -> Vec<Divergence> {
        // run op on target and model; return mismatches
    }
    fn checkpoint(&mut self, step: usize, n: usize, log: &mut Vec<MyOp>, rng: &mut StdRng)
        -> CheckpointAction {
        CheckpointAction::Continue { divergences: vec![] }
    }
}

let result = run_sequential(SequentialOpts { n_ops: 1000, checkpoint_every: Some(200) }, &mut rng, &mut sim);
assert!(result.passed(), "first mismatch: {:?}", result.first_divergence);
```

When a long trace fails, `shrink(failing_ops, |trace| replay(trace).has_errors())` reduces it to a minimal reproducer. See `crates/dst` for a full production driver.
