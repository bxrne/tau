# libharness

Shared simulation and benchmark harness for the tau workspace. Used by `dst` (deterministic simulation tester) and the Criterion benchmark suite.

## Components

### `OneBrcGen` — 1BRC data generator

Generates synthetic station/temperature readings in the shape of the [One Billion Row Challenge](https://github.com/gunnarmorling/1brc): ~413 station names, each with a temperature reading sampled from a per-station Gaussian distribution.

```rust
use libharness::{SeedTree, datagen::OneBrcGen};

let tree = SeedTree::new(0xdeadbeef);
let mut gen = OneBrcGen::new(&tree, "my-stream");
let reading = gen.draw(); // { station: "Hamburg", temp_x10: 175 }
let batch = gen.batch(1000);
```

Temperatures are returned as `temp_x10: i64` (temperature × 10) to keep all arithmetic in i64 — no floating-point in the oracle path.

### `Oracle` — BTreeMap reference implementation

A `BTreeMap`-backed reference implementation for cross-checking query results against `libtau`. Supports `append`, `at`, `reduce_min`, `reduce_max`, `reduce_avg`, `sample_midpoints`, and `reset` (for fault injection).

```rust
use libharness::Oracle;

let mut oracle = Oracle::new();
oracle.append("Hamburg", &[(0, 1, 175), (1, 2, 182)]);
assert_eq!(oracle.at("Hamburg", 0), Some(175));
assert_eq!(oracle.reduce_min("Hamburg", 0, 2), Some(175));
```

### `SeedTree` — hierarchical seed derivation

Deterministic seed tree: a root seed is hashed with a tag string to produce independent sub-seeds. Ensures that multiple concurrent generators don't interfere with each other even when seeded from the same root.

```rust
use libharness::SeedTree;

let tree = SeedTree::new(42);
let child_a = tree.child("stream-a"); // deterministic sub-seed
let child_b = tree.child("stream-b"); // independent from child_a
```

### `Tier` — workload scale enum

```rust
use libharness::Tier;

assert_eq!(Tier::Nano.row_count(), 10_000);
assert_eq!(Tier::Micro.row_count(), 1_000_000);
```

| Tier | Rows | Intended use |
|------|------|-------------|
| `Nano` | 10 k | CI smoke (< 1 s) |
| `Micro` | 1 M | PR-time sanity |
| `Small` | 100 M | Nightly / dedicated runner |
| `Full` | 1 B | Manual / release benchmarking |

## Benchmarks

The Criterion micro-benchmark suite lives in `benches/engine.rs`:

```bash
cargo bench -p libharness               # all suites
cargo bench -p libharness -- --bench at # just AT
```

Suites: `at` (point lookup at varying layer counts), `range` (narrow + full), `reduce` (all aggregation functions), `append` (batch sizes 1–1000), `onebrc` (nano-tier 1BRC ingest throughput).
