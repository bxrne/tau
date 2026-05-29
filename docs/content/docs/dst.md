+++
title = "DST"
date = 2026-05-29
template = "page.html"
+++

Tau's correctness is not a claim. It is a property checked on every build by a deterministic simulation tester that drives the engine against a reference oracle, injects faults and reproduces any failure from a single seed.

This page describes what the DST does, why it works and where the idea comes from.

---

## Inspiration

The deterministic simulation approach is not new. It comes from a lineage of systems that earned their reputations for correctness the same way.

[**FoundationDB**](https://apple.github.io/foundationdb/testing.html) is the canonical example. The team built a database that starts in an arbitrary state and runs for thousands of simulated days, with concurrent clients and aggressive fault injection, against a single threaded executable so every bug is reproducible from a seed. The simulation tester ran continuously for years before the first public release.

[**TigerBeetle**](https://tigerbeetle.com/) carried the idea forward with `vopr`, the Variable Operating Reproducer, and built a brand around it. The discipline is the same. Single threaded executable, real protocol traffic, every fault injectable, every run reproducible.

Tau's DST is shaped by both. The mechanism is general. Any system whose correctness depends on the interaction of stateful components benefits from a tester of this shape.

---

## Two modes

**Embedded (`--quick`).** Uses the `libtau` executor directly. No server process, no I/O, no network. Simulates centuries of temporal data in seconds. Runs as part of CI on every push.

**Full (default).** Spawns a real `tau` server process for each cell in the configuration matrix, drives traffic over TCP, scrapes Prometheus metrics, injects faults and cross checks every response against a reference oracle.

The configuration matrix is eight cells.

```
Transport: plain | TLS
Auth:      none  | password
WAL:       off   | on
```

All eight are driven from the same seed.

---

## The oracle

The oracle is a `BTreeMap<start, (end, value)>` per lens. No layers. No compaction. No WAL. Just obviously correct temporal semantics with `O(log n)` lookups.

Every statement processed by the engine is also applied to the oracle. Every read is checked against both. If the engine and the oracle disagree, the DST stops, prints the seed and the violated invariant and tells you how to reproduce.

The oracle has no clever optimisations. That is the point. It is a specification, not an implementation. Any divergence between the engine and the oracle is a bug in the engine.

---

## Reproducibility

A `u64` seed drives the entire operation sequence. Which database. Which lens. Which intervals. Which timestamps. When to inject faults. Which faults to inject. Given the same seed, the same operations execute in the same order, on the same files, in the same threads.

```sh
cargo run --release --bin dst -- --quick --seed 0xdeadbeef
```

A seed that found a bug six months ago can be replayed against a patched binary to confirm the fix.

This is the property that makes the DST useful. Most concurrency bugs are not reproducible. A DST seed is.

---

## Fault injection

Two fault classes are injected in full mode.

**Connection drop.** The client TCP connection is closed and reconnected. Verifies that the server accepts reconnections cleanly and that previously written data is still readable afterwards.

**WAL truncation.** The WAL file is truncated by 16 bytes to simulate a partial write. On the next server restart the WAL must replay cleanly without panic or silent data loss.

A `--fault-interval N` flag controls density. The default injects a fault every 500 operations.

---

## What it catches

The DST is where emergent bugs live. Not the ones a unit test catches, but the ones that only appear when several mechanisms interact at once.

- A base lens compacts. A derived lens references it. The WAL replays. A `RANGE` query straddles the boundary.
- Hundreds of correction layers accumulate before compaction fires. A concurrent reader sees the transition.
- The same mutation is applied at three permission levels. The state machine diverges only on the third.

These are the bugs narrow testing approaches miss. The DST catches them or fails to terminate, and either result is information.

---

## Invariants checked

**Storage**

- A base lens has data only if data was appended.
- Each layer is sorted with no internal overlap.
- `min_start` and `max_end` match the actual extent.
- After compaction, for every timestamp the oracle covers, `AT(lens, t) == oracle.AT(lens, t)`.

**Query semantics**

- `AT(lens, t)` agrees with the oracle for any `t` in the covered range.
- `AT(lens, t)` returns `None` for any `t` outside all covered intervals.
- `RANGE` segments are non overlapping and strictly sorted by `start`.
- No segment has `start >= end`. No segment extends outside the queried range.

**Concurrent correctness**

- All concurrent readers querying the same timestamp return the same value.
- A background stress reader never panics regardless of the concurrent write load.

---

## Running it

```sh
# Embedded mode. 30 seconds. CI suitable.
cargo run --release --bin dst -- --quick

# Embedded with a specific seed.
cargo run --release --bin dst -- --quick --seed 0xdeadbeef

# Full simulation across all eight cells.
cargo run --release --bin dst

# Full simulation with real disk WAL and CSV output.
cargo run --release --bin dst -- --scratch /var/tmp/tau --out results.csv

# Longer embedded run.
cargo run --release --bin dst -- --quick --duration 120
```

On failure the DST prints the seed, the violated invariant, the expected and actual values and the exact command to reproduce.

---

*Tau's DST builds on prior art from [FoundationDB](https://apple.github.io/foundationdb/testing.html) and [TigerBeetle](https://tigerbeetle.com/). Both teams have written extensively about the practice and both are recommended reading for anyone building a system where correctness matters more than features.*
