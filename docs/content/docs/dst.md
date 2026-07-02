+++
title = "Deterministic Simulation Testing"
date = 2026-06-05
template = "page.html"
+++

Tau DST is split into two crates:

- **[`libdst`](https://github.com/bxrne/tau/tree/master/crates/libdst)** — generic framework: [`DualSimulation`](https://github.com/bxrne/tau/tree/master/crates/libdst/src/sim.rs), behavior tree, deterministic scheduler, shrink, fault helpers. Usable for any target + isolated reference model.
- **[`dst`](https://github.com/bxrne/tau/tree/master/crates/dst)** — Tau driver binary. Implements the framework for every [`libtau::Executor`](https://github.com/bxrne/tau/tree/master/crates/libtau/src/executor.rs) storage configuration.

## CLI

User-facing flags (plus `--help`):

| Flag | Meaning |
|------|---------|
| `--seed` | RNG seed (logged at start; random if omitted) |
| `--ops` | Sequential ops **per profile** (default `2000`) |
| `--concurrency` | Reader threads in the concurrent phase (`0` = skip) |
| `--ci` | CI presets: profile-specific op counts; concurrency defaults to `4` when unset |
| `--tier` | Profile matrix tier: `smoke`, `standard`, `nightly` (default: `smoke` with `--ci`, else `standard`) |

Output is **tracing only** (`RUST_LOG=warn` recommended).

```bash
# Local run (all profiles + optional concurrent)
cargo run --release --bin dst -- --seed 42 --ops 2000 --concurrency 4

# CI (single command: all backends + concurrent)
RUST_LOG=warn cargo run --release --bin dst -- --ci --seed 1
```

## Architecture

### DualSimulation

Every run is a `DualSimulation`: pick an op, apply it to both target and model, compare outputs. The framework lives in `libdst`:

```
pick(rng) -> Op
apply(step, op) -> Vec<Divergence>   // structured mismatches
checkpoint(step, n, log, rng) -> CheckpointAction
```

`Divergence` records the step index, a description string, and `Debug`-formatted expected vs got values. `CheckpointAction` is either `Continue { divergences }` (keep log) or `ResetLog { divergences }` (discard log after WAL truncation).

### Independent oracle

The reference oracle (`crates/dst/src/oracle.rs`) shares **no code with `libtau`**. It stores `Vec<TauInterval>` per layer, stamps each layer's `written_at` from the same virtual clock as the SUT, and runs its own per-transaction-time-generation sweep-line compaction at the same threshold. It models `AT … AS OF` (newest-wins filtered by `written_at`) and the `HISTORY` generation set independently, so a bug in libtau's sweep-line, compaction, or as-of paths diverges and is caught.

### Transaction-time virtualization

The write clock is virtualized: each applied op advances a replay-safe tick that pins `written_at`, and ops are clustered into shared generations. So `AT … AS OF` and `HISTORY` are exercised across real generation boundaries — before the first write, mid-history, and at `now` — and the tick is reset and re-advanced identically on checkpoint replay, reproducing every `written_at` bit-for-bit.

### Behavior tree

A static `LazyLock<Tree<SimCtx, Op>>` of 34 closure-based leaves, including `AT … AS OF`, `HISTORY`, and the N-dimensional ops (create/append/drop/point/range over a 2-axis lens, modelled independently in the oracle as orthotope scans). Guards and builders are `Arc<dyn Fn>` — no fn-pointer constraints. Tag bits suppress WAL-excluded ops at runtime (`excluded_tags` parameter to `Tree::pick`).

### Deterministic scheduler

`libdst::Scheduler` implements cooperative concurrency without OS threads. A seeded RNG picks which task runs next. Every interleaving is reproducible from the seed. Use it to simulate multi-client concurrent workloads in integration tests.

### Shrink

`libdst::shrink` and `shrink_with_granularity` reduce a failing op trace to the smallest sub-sequence that still fails, using the delta-debugging algorithm. Useful when a divergence is found after hundreds of ops and you need to understand the minimal reproducer.

## Profile matrix

Profiles are a Cartesian product in [`profile/spec.rs`](https://github.com/bxrne/tau/tree/master/crates/dst/src/profile/spec.rs): **storage** × **compaction** × **encryption** × **transport** × **auth**. Names look like `wal_stress_enc_single_direct_noauth`.

Every sequential run uses a **fresh isolated oracle** (never seeded from the executor). The **target** is either a direct [`Executor`](https://github.com/bxrne/tau/tree/master/crates/libtau/src/executor.rs) or a TCP/TLS [`WireClient`](https://github.com/bxrne/tau/tree/master/crates/dst/src/target/wire.rs) talking to an ephemeral [`tau` harness](https://github.com/bxrne/tau/tree/master/crates/tau/src/harness.rs).

| Tier | When | Cells |
|------|------|-------|
| **smoke** | `--ci` | Five representative direct cells (memory ×2, wal ×2, disk) |
| **standard** | default local run | All direct engine cells (10), including AES-256 WAL/disk |
| **nightly** | `--tier nightly` | Standard + wire plain/TLS/auth over in-memory server |

```bash
RUST_LOG=warn cargo run --release --bin dst -- --seed 42 --tier nightly
```

Each profile in a run is driven with the same `--seed` (re-seeded per profile for reproducibility).

### Faults (checkpoint every 200 ops)

Damage kinds — a short write (truncate) vs a length-preserving bit-flip run (corrupt) — are drawn from the seeded RNG so both fire across the matrix. File faults reopen-probe the damaged store and assert tau **recovers or returns a clean error, never panics**; the store is then rebuilt from the authoritative op log, so the damage never perturbs the oracle comparison.

| Storage / transport | Checkpoint behavior |
|---------|---------------------|
| Memory | Rebuild target + oracle, dual-replay op log |
| WAL (odd) | Delete WAL + oracle replay; dual-replay op log |
| WAL (even) | Truncate **or corrupt** the WAL; reopen-probe; fresh target; reset op log |
| Disk (odd) | Wipe target `.dat`/`.wal` files, dual-replay op log (replay equivalence). A separate `pbt_disk_persists_*_across_reopen` test exercises faithful restart over the real persisted files (no wipe). |
| Disk (even) | Truncate **or corrupt** a random `.dat`, reopen-probe, then wipe + dual-replay |
| Wire (odd) | Server crash: rebuild the whole wire stack (new server, fresh executor) + dual-replay |
| Wire (even) | Network drop: sever the TCP connection and reconnect to the same live server; state survives, op log untouched |

The disk corruption probe caught a real bug: a corrupted `.dat` could decode an inverted `[start, end)` interval and panic in `Tau::new`. The loader now validates intervals and bounds untrusted length prefixes, returning `InvalidData` instead.

**Transactions** are enabled for memory/disk/wire profiles. WAL profiles use `WAL_EXCLUDED` tags in the behavior tree to skip transaction and multi-DB ops until single-DB WAL replay semantics are fully validated.

For TTL, DST pins the wall clock via [`wall_clock::set_fixed_now_secs`](https://github.com/bxrne/tau/tree/master/crates/libtau/src/wall_clock.rs) (`1_700_000_000`).

The `disk` backend (when selected) pairs each `<db>.dat` with a `<db>.wal`: appends and schema DDL (`CREATE`/`DERIVE`/`SET TTL`/`DROP`) go to the WAL first (fsynced by default), and `<db>.dat` is rewritten atomically only on checkpoint (compaction or `[wal].max_size_mb`). This makes acknowledged DML and lens definitions durable across clean restarts; `CREATE DATABASE <name>` on a disk executor re-opens `<db>.dat`, replays `<db>.wal` on top, and replays the schema section to restore base/derived lenses and TTL policies. The `pbt_disk_persists_data_and_schema_across_reopen` test in `sim.rs` covers this path under the DST harness.

## Concurrent phase

When `--concurrency` > 0 (or `--ci` with default `4` readers), an in-memory writer/readers phase runs after all sequential profiles. Writes use the same dual-apply path; readers check for invalid RANGE shape (non-overlapping, sorted), then reconcile all AT values against the oracle after writes complete.

## CI

After `cargo nextest run --release`, the workflow runs:

```bash
cargo test -p libdst -p dst --release
RUST_LOG=warn cargo run --release --bin dst -- --ci --seed 1
```

Then the Docker image build.

## Tests

```bash
cargo nextest run --release -p libdst   # framework: btree, divergence, scheduler, shrink, ...
cargo nextest run --release -p dst      # driver: oracle, apply, btree, sim profiles
```

All `#[hegel::test]` property-based tests across crates are named with a `pbt_` prefix (e.g. `pbt_...`) so they are easy to filter in logs and CI output.

See [Testing](/docs/testing/) for the full strategy.
