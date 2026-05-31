+++
title = "Testing"
date = 2026-05-28
template = "page.html"
+++

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

The DST is where emergent correctness bugs live: the ones that only appear when:

- A base lens compacts, a derived lens references it, and then the WAL replays
- Hundreds of correction layers accumulate before compaction fires, then a concurrent `RANGE` scan sees the transition
- The same mutation is applied with three different permission levels and the state machine diverges only on the third

### Dataset and oracle

The DST is driven by the **1BRC dataset shape**: ~413 station names, each mapped to one Base lens. Every reading is stored as a degenerate tau `[t, t+1)` with `value = temperature × 10`. After ingest, `REDUCE min/max/avg` per station is cross-checked against a `BTreeMap<start, (end, value)>` oracle. Any divergence is a bug.

The oracle has no layers, no compaction, no WAL — just obviously correct temporal semantics.

### Tiers

Scale is controlled by `--tier`. Nano runs in CI on every push.

| tier | rows | use |
|------|------|-----|
| `nano` | 10 k | CI correctness smoke (<1 s) |
| `micro` | 1 M | PR-time perf sanity |
| `small` | 100 M | nightly / dedicated runner |
| `full` | 1 B | manual / release benchmarking |

### Fault injection

A fault is injected every 5,000 rows: the victim station's lens is dropped and recreated, and the oracle resets to match. This exercises the lens lifecycle under live ingest load.

### Deterministic reproduction

A `u64` seed drives all randomness. The seed is printed on every run.

```bash
cargo run --release --bin dst -- --tier nano --seed 3735928559
```

A seed that found a bug six months ago can be re-run against a patched binary to confirm the fix.

### How to run

```bash
# CI smoke — runs in under 1 second
cargo run --release --bin dst -- --tier nano

# Reproducible run
cargo run --release --bin dst -- --tier nano --seed 3735928559

# 1 M rows, disable fault injection
cargo run --release --bin dst -- --tier micro --no-faults
```

On failure, the DST prints the seed and the violated invariant.

## Summary

| layer | what it catches | when to run |
|-------|----------------|-------------|
| Unit tests | Regressions on known-shape behaviour | Always (CI) |
| Hegel PBT | Invariant violations across random inputs | Always (inline with unit tests) |
| DST nano | 1BRC correctness: oracle cross-check + fault injection | CI on every push |
| DST micro/small | Scale + throughput regression | PR / nightly |
| Criterion benches | Engine microbenchmark regressions (AT/RANGE/REDUCE/APPEND) | Nightly / on demand |
