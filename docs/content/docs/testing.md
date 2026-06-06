+++
title = "Testing"
date = 2026-05-28
template = "page.html"
+++

Tau uses four distinct testing layers. Each one finds a different class of bug; together they provide confidence across correctness, input coverage, emergent system behaviour, and crash recovery.

---

## Layer 1: Example-Based Unit Tests

**Location:** `#[cfg(test)] mod tests` block at the bottom of every source file.

**What they test:** Specific, known-correct behaviours with a fixed shape. Wire protocol responses, error message strings, parse failures, WAL checksum mismatches, auth rejection sequences. These are behaviours where the output is fully determined by the input and any change is a regression.

**Coverage:**

- Parser rejects malformed input and accepts valid input
- Executor returns the correct `Output` variant for each statement
- Permission checks fire on the correct conditions
- WAL replay reconstructs the same in-memory state as a direct write
- WAL rotation archives the pre-rotation file for point-in-time recovery
- Connection manager accepts and rejects connections as expected

**How to run:**

```bash
cargo nextest run --release                        # all tests (preferred)
cargo nextest run --release --lib                  # libtau unit tests only
cargo nextest run --release -p libdst              # DST framework tests
cargo nextest run --release -p dst                 # Tau DST driver tests
cargo nextest run --release -E 'binary(tau)'       # server tests only
cargo nextest run --release -E 'binary(tauctl)'    # tauctl tests only
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
- `Response::parse` never panics on arbitrary UTF-8 text
- `Response::display → parse` roundtrip for `VAL`, `RANGE`, and `NAMES` variants
- WAL rotation archive is replayable and contains pre-rotation layer state
- `libdst` behavior-tree guards, deterministic scheduler, shrink correctness, and Tau oracle compaction / TTL properties

**How to run:**

```bash
cargo nextest run --release   # Hegel runs inline alongside example tests
```

All such tests are named `pbt_*` for easy log filtering. Hegel auto-installs a Python shim (`~/.cache/hegel`) on first run. Each property runs 100+ randomised cases by default. Use `HEGEL_MAX_EXAMPLES=500` to increase the draw count.

---

## Layer 3: Deterministic Simulation Testing (libdst + dst)

**Location:** `crates/libdst` (framework), `crates/dst` (Tau driver binary).

**What they test:** End-to-end agreement between `libtau::Executor` and an independent reference oracle (no libtau code) under random workloads — including derived lenses, TTL, multi-database switching, WAL replay, and truncated-WAL recovery. Divergences are structured ([`Divergence`](https://github.com/bxrne/tau/tree/master/crates/libdst/src/divergence.rs)) with step index, description, and expected/got values. Failing traces are minimised via delta-debug shrinking.

**How to run:**

```bash
cargo run --release --bin dst -- --seed 42
RUST_LOG=warn cargo run --release --bin dst -- --ci --seed 42
cargo test -p libdst -p dst
```

See [Deterministic Simulation Testing](/docs/dst/) for architecture and operation tables.

---

## Layer 4: Fuzz Testing (cargo-fuzz / LibFuzzer)

**Location:** `crates/fuzztau/fuzz_targets/`

**What they test:** Crash-freedom and panic-freedom under arbitrary byte inputs — the class of bug that neither deterministic unit tests nor property tests reliably find, because the fault depends on specific byte sequences the author never imagined.

Two targets are maintained:

| Target | Entry point | What it finds |
|--------|------------|---------------|
| `parse` | `libtau::parse` | Panics, OOMs, or infinite loops in the `nom` parser on arbitrary input |
| `wire` | `libtau::Response::parse` | Same surface on the wire decoder — a different grammar with different splitting and integer-parsing code |

**Seed corpus:** `crates/fuzztau/seeds/{wire,parse}/` — hand-crafted valid inputs covering every response variant and every TauQL statement type. The fuzzer uses these as starting points to mutate from; seeds are committed to the repo. The generated corpus (gitignored) grows in `crates/fuzztau/corpus/` during a run and can be minimised with `cargo fuzz cmin`.

**Prerequisites:** a nightly Rust toolchain (`rustup toolchain install nightly`).

**How to run:**

```bash
# Run the wire decoder fuzzer (ctrl-c to stop; add -max_total_time=N for a time cap)
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire crates/fuzztau/seeds/wire

# Run the parser fuzzer
cargo +nightly fuzz run --fuzz-dir crates/fuzztau parse crates/fuzztau/seeds/parse

# Minimise the corpus after a long run (keeps coverage, drops redundant inputs)
cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau wire  crates/fuzztau/corpus/wire
cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau parse crates/fuzztau/corpus/parse
```

Crash inputs are saved to `crates/fuzztau/artifacts/{wire,parse}/`. If a crash is found, reproduce it with:

```bash
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire crates/fuzztau/artifacts/wire/<filename>
```

---

## Summary

| Layer | What it catches | When to run |
|-------|----------------|-------------|
| Unit tests | Regressions on known-shape behaviour | Always (CI) |
| Hegel PBT | Invariant violations across random inputs | Always (inline with unit tests) |
| libdst / dst | SUT vs reference divergence under emergent workloads | CI (after nextest, before Docker) |
| cargo-fuzz | Panics and crashes from adversarial byte sequences | On demand / nightly CI |