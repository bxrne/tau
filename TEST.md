# Tau Testing Philosophy

Tau uses three distinct testing layers. Each one finds a different class of bug; together they provide confidence across correctness, input coverage, and emergent system behavior.

---

## Layer 1: Example-Based Unit Tests (`cargo test`)

**Location:** `#[cfg(test)] mod tests` block at the bottom of every source file.

**What they test:** Specific, known-correct behaviors with a fixed shape. Wire protocol responses, error message strings, parse failures, WAL checksum mismatches, auth rejection sequences. These are behaviors where the output is fully determined by the input and any change is a regression.

**Coverage:**
- Parser rejects malformed input and accepts valid input
- Executor returns the correct `Output` variant for each statement
- Permission checks fire on the correct conditions
- WAL replay reconstructs the same in-memory state as a direct write
- Connection manager accepts and rejects connections as expected

**How to run:**
```bash
cargo test --release          # all example-based and Hegel tests
cargo test --release --lib    # libtau unit tests only
cargo test --release --bin tau
cargo nextest run             # parallel runner, nicer output
```

These tests are fast regression anchors. Where a behavior can be stated as an invariant over any input, the Hegel layer below covers it instead.

---

## Layer 2: Property-Based Tests (Hegel / Hypothesis)

**Location:** `#[hegel::test]` in the same `mod tests` blocks as example-based tests. Currently 103 properties across 12 modules.

**What they test:** Invariants that must hold for any input, not just a chosen example. Hegel draws randomized inputs from typed generators, runs the property hundreds of times, and shrinks failures to the smallest possible reproducer.

**Coverage:**
- `Tau::new(s, e, v).contains(t)` iff `s <= t < e`, for any s, e, t
- `Layer::at(t)` matches a linear scan over the same taus
- `Value::encode` / `Value::decode` roundtrip for every value variant
- `compact_layers` preserves all query results (pending; see DST below)
- Auth `Perm` display / parse roundtrip
- `handle_query` never panics on arbitrary input strings
- Parse failure responses always start with `ERR parse:`

**How to run:**
```bash
cargo test --release          # Hegel runs inline alongside example tests
```

Hegel auto-installs a Python shim (`~/.cache/hegel`) on first run. Each property runs 100+ randomized cases by default. Use `HEGEL_MAX_EXAMPLES=500` to increase the draw count.

Property tests complement unit tests: unit tests pin specific behaviors, property tests verify that invariants hold across all inputs Hegel can generate.

---

## Layer 3: Deterministic Simulation Tester (`cargo run --release --bin dst`)

### Why this exists

The first two layers are local. They test individual components with known inputs or random inputs from a single draw. They cannot find bugs that only emerge when:

- A base lens is compacted, a derived lens references it, and then the WAL is replayed
- Hundreds of correction layers accumulate before compaction fires, then a concurrent RANGE scan sees the transition
- The same mutation is applied with three different permission levels and the state machine diverges only on the third

The simulation tester is inspired by three sources.

**Antithesis** (deterministic simulation testing): run the entire system under a single controlled PRNG. Given the same seed, the exact same sequence of operations executes, so any discovered bug is a reproducible repro. No flaky tests, no Heisenbugs. A seed that found a bug six months ago can be re-run against a patched binary to confirm the fix.

**TigerStyle** (TigerBeetle engineering philosophy): assert both the positive and the negative. Not just "after APPEND the value is present" but also "after APPEND the previous value at the same timestamp is inaccessible through the base type" and "no layer ever contains an interval with `start >= end`". The DST inserts explicit invariant checks after every mutating operation. In production these paths are never reached; in the DST they run millions of times.

**Hypothesis / Hegel**: the operations themselves are drawn from the same machinery that powers Hegel. If the simulation finds an invariant violation, the operation sequence can be minimized to the smallest prefix that still reproduces the failure.

### Two modes

The DST runs in two modes. In embedded mode (`--quick`), it uses the library executor directly with no server process, no I/O, and a tightly controlled simulation loop. This is the fast path: suitable for CI, covers centuries of simulated time in under a minute. In full mode (default), it spawns a real tau server for each config cell (transport x auth x WAL), drives traffic over TCP, cross-checks every response against the oracle, injects faults, and scrapes Prometheus metrics -- the same infrastructure used to measure throughput. Both modes share the same oracle and invariant checks.

### Architecture

```
+-----------------------------------------------------+
|                      Simulator                       |
|                                                      |
|   seed -> RNG -> Op stream -> Executor (embedded)    |
|                    |                                 |
|                    v                                 |
|             Oracle (simple model)                    |
|                    |                                 |
|                    v                                 |
|         Invariant checker (after every op)           |
+-----------------------------------------------------+
```

**Seed:** A `u64` seed (from `--seed`, or the system clock). Printed on every run so any failure is a one-liner repro.

**Op stream:** A randomized sequence of `Op` variants drawn by a seeded RNG:
- `Append(lens, taus)`: one or more `(start, end, value)` triples, non-overlapping within the batch
- `At(lens, t)`: point lookup
- `Range(lens, start, end)`: range scan
- `Drop(lens)`: drop a lens
- `Recreate(lens)`: recreate a dropped lens

**Oracle:** A minimal reference implementation that maintains a `Vec<(u64 revision, i64 start, i64 end, Value)>` per lens. Newest revision wins. It answers AT and RANGE by linear scan. Its job is to be obviously correct: no layers, no compaction, no WAL, just a list. Every query is cross-checked; if the executor and the oracle disagree, the test fails.

**Concurrent reader pool:** A background thread continuously reads random timestamps across all lenses for the entire simulation duration, using a shared `Arc<RwLock<Executor>>`. Any panic in that thread is a concurrency bug. After each write, a burst of N reader threads all query the same timestamp and must all agree with the oracle.

**Simulated time:** The time cursor advances monotonically with each append. Over a 30-second run at 500 ops/second, the simulator accumulates years of simulated sensor time, stressing correction-layer accumulation and compaction far beyond what example tests can reach.

### Invariants checked

After every operation the DST asserts:

**Storage invariants:**
- Every base lens has a non-empty layer stack only if data was appended to it.
- `layer.taus` is sorted and non-overlapping within a single layer.
- `layer.min_start == layer.taus.first().start` and `layer.max_end == layer.taus.last().end`.
- After compaction: for every timestamp `t` that the oracle covers, `executor.AT(lens, t) == oracle.AT(lens, t)`.

**Query semantics:**
- `AT(lens, t)` agrees with the oracle for any `t` in the covered range.
- `AT(lens, t)` returns `None` for any `t` outside all covered intervals.
- `RANGE(lens, s, e)` segments are non-overlapping and strictly sorted by start.
- No segment in a RANGE result has `start >= end`.
- No segment extends outside the queried range.
- RANGE midpoints agree with the oracle.

**Concurrent correctness:**
- All concurrent readers querying the same timestamp return the same value.
- Concurrent readers never observe a corrupt or partial write state.
- The background stress reader never panics regardless of concurrent write load.

**Crash safety (negative invariants):**
- No two segments in a RANGE result ever overlap.
- No RANGE segment ever has `start >= end`.
- The executor never panics on any generated operation sequence.

### How to run

```bash
# Default: random seed, 30s, 500 ops/s, 8 concurrent readers
cargo run --release --bin dst

# Reproducible run from a known seed
cargo run --release --bin dst -- --seed 0xdeadbeef

# Longer run (more simulated time, more compaction cycles)
cargo run --release --bin dst -- --duration 300

# More concurrent reader pressure
cargo run --release --bin dst -- --readers 32

# Print the op sequence as it executes (useful for debugging a failure)
cargo run --release --bin dst -- --seed 42 --log-level trace
```

On failure the DST prints:
1. The seed that reproduces the failure
2. The invariant that was violated
3. The expected value (oracle) and the actual value (executor)
4. The exact command to reproduce

The seed alone is sufficient to reproduce the failure on the same binary.

---

## Summary

```
cargo test --release           fast, always run in CI, catches regressions
cargo nextest run              same, parallel, better output
cargo run --bin dst -- --quick correctness depth test, run before release
cargo run --bin dst            full simulation: all configs, fault injection, throughput
```

Unit tests are the floor: any regression that breaks a named case fails immediately. Hegel PBT tests hammer invariants with random inputs and shrink on failure. The DST stress-tests emergent system behavior across operation sequences and simulated time periods that would take years to accumulate organically.
