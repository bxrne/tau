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

The reference oracle (`crates/dst/src/oracle.rs`) shares **no code with `libtau`**. It stores `Vec<TauInterval>` per layer and runs its own sweep-line compaction at the same threshold as the SUT. Divergences in libtau's sweep-line or query paths are caught because the oracle computes the same results independently.

### Behavior tree

A static `LazyLock<Tree<SimCtx, Op>>` of 20 closure-based leaves. Guards and builders are `Arc<dyn Fn>` — no fn-pointer constraints. Tag bits suppress WAL-excluded ops at runtime (`excluded_tags` parameter to `Tree::pick`).

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

### Faults (direct only; checkpoint every 200 ops)

| Storage | Checkpoint behavior |
|---------|---------------------|
| Memory | Rebuild target + oracle, dual-replay op log |
| WAL (odd) | Delete WAL + oracle replay; dual-replay op log |
| WAL (even) | Truncate WAL at random offset; fresh target; reset op log |
| Disk | Wipe target `.dat` files, dual-replay op log (tests replay equivalence). A separate `pbt_disk_persists_*_across_reopen` test exercises faithful restart over the real persisted files (no wipe) after per-append flushes. |
| Wire | Memory-style replay (no WAL/disk files) |

**Transactions** are enabled for memory/disk/wire profiles. WAL profiles use `WAL_EXCLUDED` tags in the behavior tree to skip transaction and multi-DB ops until single-DB WAL replay semantics are fully validated.

For TTL, DST pins the wall clock via [`wall_clock::set_fixed_now_secs`](https://github.com/bxrne/tau/tree/master/crates/libtau/src/wall_clock.rs) (`1_700_000_000`).

The `disk` backend (when selected) now flushes its `<db>.dat` atomically on *every* append and every schema DDL (`CREATE`/`DERIVE`/`SET TTL`/`DROP`). This makes acknowledged DML and lens definitions durable across clean restarts without a WAL; `CREATE DATABASE <name>` on a disk executor re-opens the `.dat` and replays its embedded schema section to restore base/derived lenses and TTL policies. The new `pbt_disk_persists_data_and_schema_across_reopen` test in `sim.rs` covers this path under the DST harness.

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
