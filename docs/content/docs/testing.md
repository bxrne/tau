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
- The kernel routes each statement to the right service and returns the correct `Output` variant
- Permission checks fire on the correct conditions
- WAL replay reconstructs the same in-memory state as a direct write
- WAL rotation archives the pre-rotation file for point-in-time recovery
- Connection manager accepts and rejects connections as expected

**How to run:**

```bash
cargo nextest run --release                        # all tests (preferred)
cargo nextest run --release --lib                  # libtau unit tests only
cargo nextest run --release --lib                  # libtau unit tests only
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
- `libtau` compaction, TTL, and query handling properties

**How to run:**

```bash
cargo nextest run --release   # Hegel runs inline alongside example tests
```

All such tests are named `pbt_*` for easy log filtering. Hegel auto-installs a Python shim (`~/.cache/hegel`) on first run. Each property runs 100+ randomised cases by default. Use `HEGEL_MAX_EXAMPLES=500` to increase the draw count.

---

## Layer 3: Deterministic Chaos Testing (dstest)

**Location:** `dst/` directory with Lua scripts; requires [dstest](https://crates.io/crates/dstest) CLI.

**What it tests:** Container resilience under fault injection. Spins up Tau Docker containers, injects chaos (pause, kill, resource deprivation), and verifies HTTP health endpoints recover correctly.

**How to run:**

```bash
cargo install dstest
dstest < dst/alive.lua
```

See [dst/README.md](https://github.com/bxrne/tau/tree/master/dst/) for scripts and configuration.

---

## Layer 4: Fuzz Testing (cargo-fuzz / LibFuzzer)

**Location:** `crates/fuzztau/fuzz_targets/`

**What they test:** Crash-freedom and panic-freedom under arbitrary byte inputs — the class of bug that neither deterministic unit tests nor property tests reliably find, because the fault depends on specific byte sequences the author never imagined.

Fuzz targets exercise the untrusted-input surfaces:

| Target | Entry point | Input | What it finds |
|--------|------------|-------|---------------|
| `parse` | `libtau::parse` | UTF-8 | Panics / OOMs / loops in the TauQL `nom` parser |
| `wire` | `libtau::Response::parse` | UTF-8 | Wire response decoder (different grammar, splitting, integer parsing) |
| `value_decode` | `libtau::Value::decode` | UTF-8 | Value codec used inside `VAL` / `RANGE` wire segments (escapes, tags) |
| `perm_parse` | `libtau::Perm::parse` | UTF-8 | Permission bitmap parser (`CRUDA`, `*`, `-`) used by wire `GRANTS` and users file |
| `parse_literal` | `libtau::parse_literal` | UTF-8 | Single-literal parser used by bulk/COPY paths |

All current targets gate on `&str`. There is no binary-format target today — the earlier
`disk_decode` target, which fuzzed the now-removed `Disk` backend's `.dat` decoder, was deleted
when `Disk` was replaced by the `Sstable` backend; an equivalent target against `Sstable`'s
run/manifest decode path would be worthwhile follow-up work.

**Dictionary:** `crates/fuzztau/tauql.dict` lists TauQL/wire tokens; pass it to the text targets with `-dict=crates/fuzztau/tauql.dict` for faster structural coverage.

**Seed corpus:** `crates/fuzztau/seeds/{parse,wire,value_decode,perm_parse,parse_literal}/` — small committed sets of valid + boundary cases. The working corpus grows in the gitignored `crates/fuzztau/corpus/` and can be minimised with `cargo fuzz cmin`.

**CI:** the `fuzz` job builds every target then runs `parse` and `wire` time-boxed (45 s each) with the dictionary and seeds; a crash/panic/OOM fails the job and uploads the reproducer.

**Prerequisites:** a nightly Rust toolchain (`rustup toolchain install nightly`).

**How to run:**

```bash
# Bootstrap working corpus from committed seeds (recommended for long runs)
mkdir -p crates/fuzztau/corpus/wire && cp crates/fuzztau/seeds/wire/* crates/fuzztau/corpus/wire/ 2>/dev/null || true
mkdir -p crates/fuzztau/corpus/parse && cp crates/fuzztau/seeds/parse/* crates/fuzztau/corpus/parse/ 2>/dev/null || true

# Run a target (ctrl-c to stop; add -- -max_total_time=N to bound the session)
# Text targets take the dictionary; the binary target takes raw bytes.
cargo +nightly fuzz run --fuzz-dir crates/fuzztau parse crates/fuzztau/corpus/parse -- -dict=crates/fuzztau/tauql.dict
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire crates/fuzztau/corpus/wire -- -dict=crates/fuzztau/tauql.dict
cargo +nightly fuzz run --fuzz-dir crates/fuzztau value_decode crates/fuzztau/corpus/value_decode
cargo +nightly fuzz run --fuzz-dir crates/fuzztau perm_parse crates/fuzztau/corpus/perm_parse
cargo +nightly fuzz run --fuzz-dir crates/fuzztau parse_literal crates/fuzztau/corpus/parse_literal

# Minimise a corpus after a long run (keeps high coverage, drops redundant inputs)
cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau wire  crates/fuzztau/corpus/wire
cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau parse crates/fuzztau/corpus/parse
```

Crash inputs are saved to `crates/fuzztau/artifacts/<target>/`. Reproduce:

```bash
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire crates/fuzztau/artifacts/wire/<filename>
```

---

## Summary

| Layer | What it catches | When to run |
|-------|----------------|-------------|
| Unit tests | Regressions on known-shape behaviour | Always (CI) |
| Hegel PBT | Invariant violations across random inputs | Always (inline with unit tests) |
| dstest | Container crash/restart resilience under chaos | CI (after nextest, before Docker) |
| cargo-fuzz | Panics, crashes, and OOMs from adversarial bytes on the text parsers | Time-boxed every CI run (build-check all targets; run `parse`/`wire`), plus on demand |