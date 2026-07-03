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

A static `LazyLock<Tree<SimCtx, Op>>` built from 26 closure-based [`libdst::btree::Leaf`] entries covering:
- `APPEND` (int, float, bool, str; default DB and aux DB)
- `AT` / `RANGE` / `REDUCE` (base, derived, materialised, and mixed-type lenses)
- `CREATE` / `DROP` / `DERIVE` / `XDERIVE` lens operations (the materialised `XDERIVE` form optionally bounded by an `OVER` range)
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

Sequential profiles checkpoint every 200 ops. The injected fault depends on storage/transport and the checkpoint parity; damage kinds (truncate vs corrupt) are drawn from the seeded RNG so both are exercised across the matrix. The two file faults reuse `libdst`'s `truncate_file` (a short write) and `corrupt_file` (a contiguous bit-flip run, length preserved).

| Storage / transport | Checkpoint | Fault |
|---------------------|-----------|-------|
| Memory | every | Rebuild target + oracle; dual-replay op log |
| WAL (odd) | dual-replay | Dual-replay op log after deleting the WAL file |
| WAL (even) | **disk media fault** | Truncate **or corrupt** the WAL; reopen-probe it (must not panic); rebuild fresh; reset op log |
| Disk (odd) | restart | Wipe `.manifest`/`.run.*` files; rebuild target + oracle; dual-replay op log |
| Disk (even) | **disk media fault** | Truncate **or corrupt** a random manifest/run file, probe that tau reopens without panicking, then wipe + dual-replay |
| Wire (odd) | server crash | Rebuild the whole wire stack (new server, fresh executor) + dual-replay |
| Wire (even) | **network fault** | Drop the live TCP connection and reconnect to the same server; state survives, op log untouched |

The damage probes assert tau **recovers or returns a clean error — never panics or hangs**. This is what caught a real bug: reading a corrupted on-disk file could decode an inverted `[start, end)` interval and panic in `Tau::new`; the loader now validates intervals (and bounds untrusted length prefixes) and returns `InvalidData` instead. Because a damaged file is always followed by a rebuild from the authoritative op log, the fault never perturbs the oracle comparison.

### Concurrent phase (`src/harness.rs`)

After sequential profiles, `--concurrency N` spawns N reader threads checking RANGE shape invariants (non-overlapping, sorted). The writer uses `apply_dual` against the oracle; a reconciliation pass checks AT against the oracle after all writes complete.

## Tests

```bash
cargo nextest run --release -p dst
```

Tests in `src/sim.rs` run every profile variant across multiple seeds. Tests in `src/apply.rs`, `src/btree.rs`, and `src/oracle.rs` include `#[hegel::test]` property-based invariants.
