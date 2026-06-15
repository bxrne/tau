# dst

Tau DST driver binary — implements [`libdst::DualSimulation`] for every `libtau::Executor` storage profile.

Drives the system under test (SUT) and an independent reference oracle in lock-step, comparing outputs at every step. Any divergence is a bug. The seed makes any run reproducible.

## Usage

```bash
# Local run — smoke tier, random seed
cargo run --release --bin dst -- --seed 42

# CI preset (smoke profiles + concurrent phase, structured logging)
RUST_LOG=warn cargo run --release --bin dst -- --ci --seed 1

# Nightly — all profiles including wire transport
RUST_LOG=warn cargo run --release --bin dst -- --tier nightly --seed 1
```

| Flag | Meaning |
|------|---------|
| `--seed N` | RNG seed (random if omitted; logged at start) |
| `--ops N` | Ops per profile (default `2000`) |
| `--concurrency N` | Concurrent reader threads (default `0` = skip) |
| `--ci` | CI presets: profile-specific counts, `4` concurrent readers |
| `--tier` | `smoke` / `standard` / `nightly` (default `standard`; `smoke` with `--ci`) |

## Architecture

### Oracle (`src/oracle.rs`)

Completely independent from `libtau`. Uses `Vec<TauInterval>` per layer with threshold-triggered sweep-line compaction that mirrors libtau's `COMPACT_THRESHOLD`. Point lookups scan layers newest-first. Range scans use boundary decomposition with same-value merging. Aggregate queries derive from the same boundary sweep.

No `libtau` storage code is imported. Divergences in libtau's sweep-line, compaction, or query paths are caught because the oracle implements the same semantics independently.

### Behavior tree (`src/btree.rs`)

A static `LazyLock<Tree<SimCtx, Op>>` built from 20 closure-based [`libdst::btree::Leaf`] entries covering:
- `APPEND` (int, float, bool, str; default DB and aux DB)
- `AT` / `RANGE` / `REDUCE` (base, derived, and mixed-type lenses)
- `CREATE` / `DROP` / `DERIVE` lens operations
- `USE DATABASE` switching
- `START TRANSACTION` / `COMMIT` / `ROLLBACK`
- Extreme timestamp probes

`SimCtx` holds a raw pointer to the oracle (valid for the duration of each `pick` call) and the current transaction flag. Tags (`WAL_EXCLUDED`) suppress multi-DB and transaction ops for WAL profiles.

### Profile matrix (`src/profile/spec.rs`)

Cartesian product of storage × compaction × encryption × transport × auth. Example name: `wal_stress_enc_single_direct_noauth`.

| Tier | Profiles |
|------|---------|
| `smoke` | Five representative direct profiles |
| `standard` | All direct profiles (~10) |
| `nightly` | Standard + wire (plain, TLS, auth) |

### Fault injection (`src/sim.rs`)

Sequential profiles checkpoint every 200 ops:

| Storage | Checkpoint |
|---------|-----------|
| Memory | Rebuild target + oracle; dual-replay op log |
| WAL (odd) | Dual-replay op log after deleting WAL file |
| WAL (even) | Truncate WAL at random offset; rebuild fresh target; reset op log |
| Disk | Wipe `.dat` files; rebuild target + oracle; dual-replay op log |
| Wire | Memory-style replay |

### Concurrent phase (`src/harness.rs`)

After sequential profiles, `--concurrency N` spawns N reader threads checking RANGE shape invariants (non-overlapping, sorted). The writer uses `apply_dual` against the oracle; a reconciliation pass checks AT against the oracle after all writes complete.

## Tests

```bash
cargo nextest run --release -p dst
```

## Relationship to `bench`

The `bench` crate's `ConfigCell`/grid (`crates/bench/src/grid.rs`) covers similar ground to the
profile matrix in `src/profile/spec.rs`: both enumerate storage × WAL × TLS × auth ×
encryption combinations, independently. A shared `ConfigMatrix` type that both crates build
their cells/profiles from has been discussed but is **not yet implemented** — this section is a
forward pointer for whoever picks that refactor up, not a description of current behavior.

Tests in `src/sim.rs` run every profile variant across multiple seeds. Tests in `src/apply.rs`, `src/btree.rs`, and `src/oracle.rs` include `#[hegel::test]` property-based invariants.
