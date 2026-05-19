+++
title = "Testing"
date = 2026-05-28
template = "page.html"
+++

Tau uses two distinct testing layers. Each one finds a different class of bug; together they provide confidence across correctness, input coverage, and emergent system behaviour.

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
cargo nextest run --release                        # all tests (preferred)
cargo nextest run --release --lib                  # libtau unit tests only
cargo nextest run --release -E 'binary(tau)'       # server tests only
cargo nextest run --release -E 'binary(ctl)'       # tauctl tests only
cargo test --release                               # fallback if nextest is not installed
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
cargo nextest run --release   # Hegel runs inline alongside example tests
```

Hegel auto-installs a Python shim (`~/.cache/hegel`) on first run. Each property runs 100+ randomised cases by default. Use `HEGEL_MAX_EXAMPLES=500` to increase the draw count.

## Summary

| layer | what it catches | when to run |
|-------|----------------|-------------|
| Unit tests | Regressions on known-shape behaviour | Always (CI) |
| Hegel PBT | Invariant violations across random inputs | Always (inline with unit tests) |
