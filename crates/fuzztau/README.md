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

All targets assert a single property: **no panic, crash, or OOM on any input** (the runs cap RSS at 2 GiB). All targets gate on `&str`. Correctness is covered by unit + Hegel PBT tests in `libtau`.

The `parse` corpus includes the N-dimensional TauQL surface — `create_lens_nd` (`AXES (…)`), `append_nd` (bracketed boxes), `at_nd` (multi-coordinate `AT`, with `AS OF`), and `range_nd` (`RANGE … AT (…)`) — and the dictionary carries `AXES` and `[`/`]`.

> **Note:** there is currently no binary-format fuzz target for the `Sstable` on-disk backend's run/manifest decode path (the `disk_decode` target that covered the earlier `Disk` backend's `.dat` format was removed when `Disk` was replaced). Adding an equivalent target against `Sstable`'s decode functions is worthwhile follow-up work.

## Dictionary

`tauql.dict` lists TauQL tokens (UPPERCASE keywords, lowercase type/agg/literal words, operators, punctuation, and wire tokens) so the mutator assembles structurally-valid inputs faster. Pass it to the text targets with `-dict=crates/fuzztau/tauql.dict`.

## CI

The `fuzz` CI job builds every target (`cargo fuzz check`) and then runs `parse` and `wire` time-boxed (`FUZZ_TIME` seconds each, default 45) seeded from `seeds/` and the dictionary. A crash, panic, or OOM fails the job and uploads the reproducer as the `fuzz-artifacts` artifact.

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
```

Run from the workspace root (`/path/to/tau`). Note: passing a `seeds/` dir directly makes libFuzzer append interesting cases to it — prefer a `corpus/` dir (below) for long runs to keep committed seeds minimal.

## Seed corpus

`seeds/{parse,wire,value_decode,perm_parse,parse_literal}/` contain hand-crafted valid and boundary inputs for each surface. These are committed so every run (and CI) starts from a useful base.

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
