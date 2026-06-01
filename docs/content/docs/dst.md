+++
title = "DST"
date = 2026-05-31
template = "page.html"
+++

Tau's correctness is not a claim. It is a property verified on every build by a deterministic simulation tester that drives the engine against a reference oracle, injects faults, and reproduces any failure from a single seed.

This page describes what the DST does, why it works, and where the idea comes from.

## Inspiration

The deterministic simulation approach is not new. It comes from a lineage of systems that earned their reputations for correctness the same way.

[**FoundationDB**](https://apple.github.io/foundationdb/testing.html) is the canonical example. The team built a database that starts in an arbitrary state and runs for thousands of simulated days, with concurrent clients and aggressive fault injection, against a single-threaded executable so every bug is reproducible from a seed. The simulation tester ran continuously for years before the first public release.

[**TigerBeetle**](https://tigerbeetle.com/) carried the idea forward with `vopr`, the Variable Operating Reproducer, and built a brand around it. Single-threaded executable, real protocol traffic, every fault injectable, every run reproducible.

Tau's DST is shaped by both.

## Dataset: the 1BRC

The DST is driven by the **One Billion Row Challenge** dataset shape: ~413 station names, each mapped to one Base lens in tau. Every reading is a degenerate tau `[t, t+1)` with `value = temperature × 10` (i64 fixed-point so all arithmetic stays exact). After ingest, `REDUCE min/max/avg` per station is cross-checked against a BTreeMap oracle.

The 1BRC shape was chosen deliberately: it exercises the ingest path (many small appends to many distinct lenses), the compaction path (layer count grows and is swept), and the aggregation path (full-range REDUCE). Any divergence between the engine and the oracle is a bug.

## Tiers

The DST runs at four scale tiers:

| Tier | Rows | Use |
|------|------|-----|
| `nano` | 10 k | CI correctness smoke (<1 s) |
| `micro` | 1 M | PR-time perf sanity |
| `small` | 100 M | nightly / dedicated runner |
| `full` | 1 B | manual / release benchmarking |

Nano runs on every push. Micro runs on workflow dispatch. Small and full are manual.

## The oracle

The oracle is a `BTreeMap<start, (end, value)>` per station lens. No layers. No compaction. No WAL. Just obviously correct temporal semantics.

Every write applied to the engine is also applied to the oracle. Every REDUCE result is compared against an oracle computation. Any divergence stops the run, prints the seed, and prints the exact command to reproduce.

The oracle has no clever optimisations. That is the point. It is a specification.

## Fault injection

A fault is injected every 5,000 rows: the victim station's lens is dropped and recreated (simulating a connection reset), and the oracle resets to match. This verifies the engine handles lens lifecycle correctly under live ingest load.

## Reproducibility

A `u64` seed drives all randomness: which station each reading goes to, temperature values, and fault victim selection. The same seed always produces the same sequence.

```sh
# Reproduce a failure exactly
cargo run --release --bin dst -- --tier nano --seed <printed-seed>
```

A seed that found a bug six months ago can be replayed against a patched binary to confirm the fix.

## Running it

```sh
# CI smoke — runs in under 1 second
cargo run --release --bin dst -- --tier nano

# Reproducible run
cargo run --release --bin dst -- --tier nano --seed 3735928559

# Disable fault injection
cargo run --release --bin dst -- --tier micro --no-faults

# WAL backend — exercises replay correctness under fault injection
cargo run --release --bin dst -- --tier nano --backend wal

# TCP backend — exercises the full line protocol over loopback
cargo run --release --bin dst -- --tier nano --backend tcp

# Quiet output
cargo run --release --bin dst -- --tier nano --log-level error
```

On failure the DST prints the seed, the violated invariant, and the exact command to reproduce.

## Architecture

The DST uses `libharness` for:

- `OneBrcGen` — deterministic reading generator
- `Oracle` — BTreeMap reference implementation
- `SeedTree` — hierarchical seed derivation so sub-streams are independent

Three backends are available via `--backend`:

| backend | what it exercises |
|---------|-------------------|
| `embedded` (default) | library executor directly — fastest, no OS scheduling noise |
| `wal` | executor backed by a real WAL on disk; fault injection restarts the executor and replays the WAL, verifying replay correctness |
| `tcp` | full in-process TCP server over loopback — exercises the line protocol, lock-routing, and connection handling |

The embedded backend is the default because it is deterministic, fast, and has no external dependencies. The WAL and TCP backends add coverage at the cost of I/O and OS scheduling.

## Invariants checked

**After ingest (all backends):** `AT(lens, t)` agrees with the oracle for every midpoint of every recorded segment.

**After REDUCE (embedded and WAL):** `REDUCE LENS station 0 N USING min` matches `oracle.reduce_min(station, 0, N)` for the first ten stations.

**After REDUCE (TCP backend):** same check issued over the wire; any divergence between the TCP response and the oracle value is a bug in the protocol layer, not just the engine.

**WAL replay (WAL backend):** after each fault, the executor is restarted from the WAL file. The restarted executor must produce the same query results as the oracle, verifying that WAL replay reconstructs state correctly.

**Fault recovery (all backends):** after a lens is dropped and recreated, subsequent appends and queries succeed without error.

*Tau's DST builds on prior art from [FoundationDB](https://apple.github.io/foundationdb/testing.html) and [TigerBeetle](https://tigerbeetle.com/). Both teams have written extensively about deterministic simulation testing and both are recommended reading.*
