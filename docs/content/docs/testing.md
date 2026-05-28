+++
title = "Testing"
date = 2024-01-07
template = "page.html"
+++

# Testing

Tau uses three distinct testing layers. Each one finds a different class of bug; together they provide confidence across correctness, input coverage, and emergent system behaviour.

---

## Layer 1: Example-Based Unit Tests

**Location:** `#[cfg(test)] mod tests` block at the bottom of every source file.

**What they test:** Specific, known-correct behaviours with a fixed shape. Wire protocol responses, error message strings, parse failures, WAL checksum mismatches, auth rejection sequences. These are behaviours where the output is fully determined by the input and any change is a regression.

**Coverage:**

- Parser rejects malformed input and accepts valid input
- Executor returns the correct `Output` variant for each statement
- Permission checks fire on the correct conditions
- WAL replay reconstructs the same in-memory state as a direct write
- Connection manager accepts and rejects connections as expected

**How to run:**

```bash
cargo test --release            # all tests
cargo test --release --lib      # libtau unit tests only
cargo test --release --bin tau  # server tests only
cargo nextest run               # parallel runner, nicer output
```

---

## Layer 2: Property-Based Tests (Hegel / Hypothesis)

**Location:** `#[hegel::test]` in the same `mod tests` blocks.

**What they test:** Invariants that must hold for *any* input, not just a chosen example. Hegel draws randomised inputs from typed generators, runs each property hundreds of times, and shrinks failures to the smallest possible reproducer.

**Coverage:**

- `Tau::new(s, e, v).contains(t)` iff `s <= t < e`, for any s, e, t
- `Layer::at(t)` matches a linear scan over the same taus
- `Value::encode` / `Value::decode` roundtrip for every variant
- `compact_layers` preserves all query results
- Auth `Perm` display / parse roundtrip
- `handle_query` never panics on arbitrary input strings
- Parse failure responses always start with `ERR parse:`

**How to run:**

```bash
cargo test --release    # Hegel runs inline alongside example tests
```

Hegel auto-installs a Python shim (`~/.cache/hegel`) on first run. Each property runs 100+ randomised cases by default. Use `HEGEL_MAX_EXAMPLES=500` to increase the draw count.

---

## Layer 3: Deterministic Simulation Tester (DST)

The DST is where emergent correctness bugs live — the ones that only appear when:

- A base lens compacts, a derived lens references it, and then the WAL replays
- Hundreds of correction layers accumulate before compaction fires, then a concurrent `RANGE` scan sees the transition
- The same mutation is applied with three different permission levels and the state machine diverges only on the third

### Two modes

**Embedded (`--quick`):** uses the library executor directly, no server process, no I/O. Simulates centuries of temporal data in seconds. Suitable for CI.

**Full (default):** spawns a real `tau` server for each config cell in the matrix (Transport × Auth × WAL), drives traffic over TCP, cross-checks every response against a simple oracle, injects faults (connection drops, WAL truncation), and scrapes Prometheus metrics to verify statement counts. Outputs a table of results.

### Oracle

Both modes cross-check against a reference implementation: a `BTreeMap<start, (end, value)>` per lens with O(log n) lookups. It has no layers, no compaction, no WAL — just obviously correct temporal semantics. Any divergence between the oracle and the executor is a bug.

### Deterministic reproduction

A `u64` seed drives the entire operation sequence. Given the same seed, the exact same operations execute in the same order — no flaky tests, no Heisenbugs.

```bash
cargo run --release --bin dst -- --quick --seed 0xdeadbeef
```

A seed that found a bug six months ago can be re-run against a patched binary to confirm the fix.

### Invariants checked

**Storage:**

- Every base lens has a non-empty layer stack only if data was appended to it
- `layer.taus` is sorted and non-overlapping within a single layer
- `layer.min_start` and `layer.max_end` match the actual first/last tau
- After compaction: for every timestamp the oracle covers, `AT(lens, t) == oracle.AT(lens, t)`

**Query semantics:**

- `AT(lens, t)` agrees with the oracle for any `t` in the covered range
- `AT(lens, t)` returns `None` for any `t` outside all covered intervals
- `RANGE` segments are non-overlapping and strictly sorted by start
- No segment has `start >= end`
- No segment extends outside the queried range

**Concurrent correctness:**

- All concurrent readers querying the same timestamp return the same value
- The background stress reader never panics regardless of concurrent write load

### How to run

```bash
# Fast: embedded mode, 30 seconds, CI-suitable
cargo run --release --bin dst -- --quick

# Embedded with a specific seed (reproducible)
cargo run --release --bin dst -- --quick --seed 0xdeadbeef

# Full simulation: all 8 config cells
cargo run --release --bin dst

# Full simulation with real-disk WAL and CSV output
cargo run --release --bin dst -- --scratch /var/tmp/tau --out results.csv

# Longer embedded run
cargo run --release --bin dst -- --quick --duration 120
```

On failure, the DST prints the seed, the violated invariant, the expected and actual values, and the exact command to reproduce.

---

## Summary

| layer | what it catches | when to run |
|-------|----------------|-------------|
| Unit tests | Regressions on known-shape behaviour | Always (CI, before every commit) |
| Hegel PBT | Invariant violations across random inputs | Always (inline with unit tests) |
| DST embedded | Emergent correctness across simulated centuries | CI (30s), before release |
| DST full | Fault injection, all transport/auth/WAL combinations | Before release, regression investigation |
