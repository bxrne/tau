# dst

## What it is

The Tau deterministic-simulation driver binary — implements `libdst::DualSimulation` for every `libtau::Kernel` storage profile. It drives the system under test and an independent reference oracle in lock-step, comparing outputs at every step; any divergence is a bug, and the seed reproduces any run exactly.

The microkernel makes each simulation self-contained: a run pins **its own kernel's** virtual clock and arms **its own kernel's** fault injector — no process-global state, so simulations can run in parallel — and divergences name the kernel service that owned the op (`[query]` / `[db]`).

## How it works

The **oracle** (`oracle.rs`) shares no code with `libtau`: naive `Vec<TauInterval>` layers, boundary-decomposition queries, and threshold-triggered compaction mirroring the engine's semantics independently, with its own virtual `now_ms` advanced per op. The **workload** is a weighted behavior tree of ~26 op kinds (appends across types and databases; base/derived/materialised reads; lens DDL; `USE DATABASE`; transactions; extreme-timestamp probes), with tags suppressing multi-DB and transaction ops for WAL profiles. **Profiles** are the Cartesian product storage × compaction × encryption × transport × auth (e.g. `wal_stress_enc_single_direct_noauth`), tiered as smoke (5 direct cells), standard (all 10 direct), and nightly (adds wire plain/TLS/auth).

Sequential runs checkpoint every 200 ops with parity-scheduled **faults**, all asserting tau recovers or returns a clean error — never panics: memory profiles rebuild + dual-replay the op log; WAL profiles alternate clean replay with an armed in-flight WAL-write failure (the WAL-first invariant) followed by at-rest truncate/corrupt damage and a reopen probe; disk profiles damage a random manifest/run file before wipe + replay; wire profiles alternate server crashes with connection drops. The at-rest probes caught a real bug: a corrupted file could decode an inverted interval and panic — the loader now returns `InvalidData`.

## Using it

```bash
cargo run --release --bin dst -- --seed 42                       # local, standard tier
RUST_LOG=warn cargo run --release --bin dst -- --ci --seed 1     # CI preset
RUST_LOG=warn cargo run --release --bin dst -- --tier nightly --seed 1
```

Flags: `--seed N` (random if omitted, logged at start), `--ops N` (default 2000), `--concurrency N` (reader threads for the concurrent phase), `--ci` (profile-specific counts + 4 readers), `--tier smoke|standard|nightly`. Every profile in a run re-seeds from the same `--seed` for reproducibility.
