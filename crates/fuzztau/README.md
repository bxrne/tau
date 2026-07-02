# fuzztau

LibFuzzer fuzz targets for `libtau`. Requires a nightly Rust toolchain.

## Targets

| Target | Entry point | Input | Attack surface |
|--------|------------|-------|----------------|
| `parse` | `libtau::parse` | UTF-8 | `nom` parser — TauQL grammar |
| `wire` | `libtau::Response::parse` | UTF-8 | Wire decoder — response line grammar |
| `value_decode` | `libtau::Value::decode` (via `Codec`) | UTF-8 | Value encoding used in wire `VAL` / `RANGE` segments |
| `perm_parse` | `libtau::Perm::parse` | UTF-8 | Permission letter parsing (`CRUDA`, `*`, `-`) used in wire and users file |
| `parse_literal` | `libtau::parse_literal` | UTF-8 | Public helper for single-literal bulk loads |
| `disk_decode` | `Disk::<Value>::decode_payload_bytes[_versioned]` / `decode_image_bytes` | **raw bytes** | **Binary** on-disk `.dat` deserialization (v1 + v2 tau parsers) — the path a corrupted file flows through |

All targets assert a single property: **no panic, crash, or OOM on any input** (the runs cap RSS at 2 GiB). The text targets gate on `&str`; `disk_decode` takes raw bytes — it is the only target exercising the binary deserialization layer, where corruption can desync length prefixes and interval bounds. Correctness is covered by unit + Hegel PBT tests in `libtau`.

`disk_decode` exposes the payload parser at every supported format version: `decode_payload_bytes` (current, v2 — a per-tau axis count for N-dimensional lenses) and `decode_payload_bytes_versioned(_, 1)` (the retained v1 migration path — bare `start`/`end` pairs), plus `decode_image_bytes` for the full header + decrypt + zstd + payload path. Hitting the payload directly is high signal — a blind fuzzer would otherwise be stuck on the CRC header and zstd frame. Both v1 and v2 tau parsers must be panic-free. Real bugs found on first run — an OOM from an unbounded `Value::Str` length and a panic on overlapping decoded taus — are fixed and pinned by `seeds/disk_decode/regression-*`; `seeds/disk_decode/nd-v2-{payload,image}` are valid v2 N-D layers (regenerate with `cargo test -p libtau emit_v2_nd_fuzz_seeds -- --ignored`).

The `parse` corpus includes the N-dimensional TauQL surface — `create_lens_nd` (`AXES (…)`), `append_nd` (bracketed boxes), `at_nd` (multi-coordinate `AT`, with `AS OF`), and `range_nd` (`RANGE … AT (…)`) — and the dictionary carries `AXES` and `[`/`]`.

## Dictionary

`tauql.dict` lists TauQL tokens (UPPERCASE keywords, lowercase type/agg/literal words, operators, punctuation, and wire tokens) so the mutator assembles structurally-valid inputs faster. Pass it to the text targets with `-dict=crates/fuzztau/tauql.dict`.

## CI

The `fuzz` CI job builds every target (`cargo fuzz check`) and then runs `parse`, `wire`, and `disk_decode` time-boxed (`FUZZ_TIME` seconds each, default 45) seeded from `seeds/` and the dictionary. A crash, panic, or OOM fails the job and uploads the reproducer as the `fuzz-artifacts` artifact.

## Quick start

```bash
rustup toolchain install nightly   # one-time

# Recommended: run against the (gitignored) corpus dir so committed seeds stay small.
# Bootstrap a corpus from seeds on first use (or after cmin):
mkdir -p crates/fuzztau/corpus/wire && cp crates/fuzztau/seeds/wire/* crates/fuzztau/corpus/wire/ 2>/dev/null || true
mkdir -p crates/fuzztau/corpus/parse && cp crates/fuzztau/seeds/parse/* crates/fuzztau/corpus/parse/ 2>/dev/null || true

# Run indefinitely (ctrl-c to stop)
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire  crates/fuzztau/corpus/wire
cargo +nightly fuzz run --fuzz-dir crates/fuzztau parse crates/fuzztau/corpus/parse
cargo +nightly fuzz run --fuzz-dir crates/fuzztau value_decode crates/fuzztau/corpus/value_decode
cargo +nightly fuzz run --fuzz-dir crates/fuzztau perm_parse crates/fuzztau/corpus/perm_parse
cargo +nightly fuzz run --fuzz-dir crates/fuzztau parse_literal crates/fuzztau/corpus/parse_literal

# Text targets benefit from the dictionary:
cargo +nightly fuzz run --fuzz-dir crates/fuzztau parse crates/fuzztau/seeds/parse \
    -- -max_total_time=60 -dict=crates/fuzztau/tauql.dict -print_final_stats=1

# Binary target — raw bytes, no dictionary:
cargo +nightly fuzz run --fuzz-dir crates/fuzztau disk_decode crates/fuzztau/seeds/disk_decode \
    -- -max_total_time=60 -rss_limit_mb=2048
```

Run from the workspace root (`/path/to/tau`). Note: passing a `seeds/` dir directly makes libFuzzer append interesting cases to it — prefer a `corpus/` dir (below) for long runs to keep committed seeds minimal.

## Seed corpus

`seeds/{parse,wire,value_decode,perm_parse,parse_literal,disk_decode}/` contain hand-crafted valid and boundary inputs for each surface, plus pinned regression reproducers under `disk_decode/`. These are committed so every run (and CI) starts from a useful base.

The fuzzer's working corpus grows in `corpus/` (gitignored). Minimise after long runs:

```bash
cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau wire  crates/fuzztau/corpus/wire
cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau parse crates/fuzztau/corpus/parse
# ... similarly for other targets
```

## Crash reproduction

Crashes land in `artifacts/<target>/`. Reproduce:

```bash
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire crates/fuzztau/artifacts/wire/<filename>
```
