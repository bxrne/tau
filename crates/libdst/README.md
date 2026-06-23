# libdst

Generic deterministic simulation testing (DST) framework for Rust — not tied to any particular engine.

Compare a **target** (system under test) with an isolated **reference model** under a reproducible seeded workload. Any mismatch is a structured [`Divergence`]; the RNG seed replays the exact failing sequence.

## Concepts

| Piece | Provided by |
|-------|-------------|
| `Op` | Your domain — any `Clone` type |
| [`DualSimulation`] | Your impl: `pick`, `apply`, `checkpoint` |
| [`btree::Tree`] | Closure-based weighted op selector |
| [`Divergence`] | Structured mismatch record (step, description, expected, got) |
| [`Scheduler`] | Deterministic cooperative task scheduler |
| [`shrink`] | Delta-debug op-trace minimiser |
| [`faults`] | File truncation + corruption helpers for fault injection |

## Quick start

```rust
use libdst::{
    DualSimulation, CheckpointAction, Divergence,
    SequentialOpts, run_sequential,
};
use rand::rngs::StdRng;

struct MySim { /* target + model state */ }

impl DualSimulation for MySim {
    type Op = MyOp;

    fn pick(&mut self, rng: &mut StdRng) -> MyOp {
        // draw next op from behavior tree or generator
    }

    fn apply(&mut self, step: usize, op: &MyOp) -> Vec<Divergence> {
        // apply op to both target and model; return mismatches
        let got = target.exec(op);
        let expected = model.exec(op);
        if got != expected {
            vec![Divergence::new(step, "exec", expected, got)]
        } else {
            vec![]
        }
    }

    fn checkpoint(
        &mut self,
        step: usize,
        n: usize,
        log: &mut Vec<MyOp>,
        rng: &mut StdRng,
    ) -> CheckpointAction {
        // inject restart, truncate files, replay op log, etc.
        CheckpointAction::Continue { divergences: vec![] }
    }
}

let result = run_sequential(
    SequentialOpts { n_ops: 1000, checkpoint_every: Some(200) },
    &mut rng,
    &mut sim,
);
assert!(result.passed(), "first mismatch: {:?}", result.first_divergence);
```

## Modules

### `sim` — [`DualSimulation`], [`CheckpointAction`]

The core trait. `apply` returns `Vec<Divergence>` — zero means clean. `checkpoint` returns one of:

- `Continue { divergences }` — keep the op log, report replay mismatches
- `ResetLog { divergences }` — clear the op log (after a WAL truncation that destroys history)

### `runner` — [`run_sequential`], [`SequentialOpts`]

Single-threaded loop: pick → apply → checkpoint every N ops. Accumulates errors into [`RunResult`] with `first_divergence`.

### `btree` — [`Tree`], [`Leaf`]

Closure-based weighted behavior tree. Build once with `Tree::new().leaf(Leaf::new(weight, tags, guard, build))`. Guards and builders are `Arc<dyn Fn>` — no fn-pointer constraints, no domain-specific fields. Pass `excluded_tags` to `Tree::pick` to disable tagged leaves at runtime (e.g., WAL-excluded ops).

```rust
let tree: Tree<MyCtx, MyOp> = Tree::new()
    .leaf(Leaf::new(
        10, 0,                                          // weight, tags
        |ctx: &MyCtx| ctx.is_ready(),                  // guard
        |rng, ctx| MyOp::Write(rng.gen_range(0..100)), // builder
    ))
    .leaf(Leaf::new(5, 0, |_| true, |rng, _| MyOp::Read(rng.gen())));
```

### `divergence` — [`Divergence`]

Structured mismatch: `step`, `description`, `expected` (Debug string), `got` (Debug string). Implements `Display`, `Clone`, `PartialEq`.

### `clock` — [`Clock`]

Virtual wall clock backed by an `AtomicI64`. Use `Clock::new(secs)`, `advance(delta)`, `set(t)`, and `now_secs()` to control time deterministically in tests.

### `scheduler` — [`Scheduler`], [`Task`]

Deterministic cooperative concurrency: a seeded RNG picks which task runs next. Each `Task` has an `ops_remaining` counter; `Scheduler::next(&mut rng)` returns the next task ID and decrements it. Use to simulate multi-client workloads reproducibly without OS threads.

### `shrink` — [`shrink`], [`shrink_with_granularity`]

Delta-debug minimiser. Given a failing op trace and a predicate, reduces it to the smallest subsequence that still fails. `shrink_with_granularity` is faster on long traces (halves/quarters then single-element removal).

```rust
let minimal = shrink(failing_ops, |trace| replay(trace).has_errors());
```

### `faults` — [`truncate_file`], [`corrupt_file`], [`Fault`]

Primitive fault injection over persisted bytes, the two ways a file goes bad:

- `truncate_file` — shorten a file at a random valid offset (a short write); returns the bytes removed.
- `corrupt_file` — flip a contiguous run of bytes at a random offset, length preserved (bit-rot / a torn write); returns the bytes corrupted.

Both are seeded by the caller's RNG, so corruption is reproducible. `TruncateFile` / `CorruptFile` wrap them behind the [`Fault`] trait for table-driven injection.

### `report` — [`RunResult`], [`SuiteResult`]

`RunResult` accumulates errors and stores `first_divergence: Option<Divergence>`. `SuiteResult::absorb` aggregates multiple phases.

## Tests

```bash
cargo nextest run --release -p libdst
```
