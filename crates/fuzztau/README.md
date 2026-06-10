# fuzztau

LibFuzzer fuzz targets for `libtau`. Requires a nightly Rust toolchain.

## Targets

| Target | Entry point | Attack surface |
|--------|------------|----------------|
| `parse` | `libtau::parse` | `nom` parser — TauQL grammar |
| `wire` | `libtau::Response::parse` | Wire decoder — response line grammar |
| `value_decode` | `libtau::Value::decode` (via `Codec`) | Value encoding used in wire `VAL` / `RANGE` segments |
| `perm_parse` | `libtau::Perm::parse` | Permission letter parsing (`CRUDA`, `*`, `-`) used in wire and users file |
| `parse_literal` | `libtau::parse_literal` | Public helper for single-literal bulk loads |

All targets assert a single property: **no panic or crash on any input** (including malformed UTF-8 where the API takes `&str`). Correctness is covered by unit + Hegel PBT tests in `libtau`.

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

# Or pass seeds/ directly for a short session (note: libFuzzer will append interesting cases to it):
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire crates/fuzztau/seeds/wire \
    -- -max_total_time=60 -print_final_stats=1
```

Run from the workspace root (`/path/to/tau`).

## Seed corpus

`seeds/{parse,wire,value_decode,perm_parse,parse_literal}/` contain hand-crafted valid and boundary inputs for each surface. These are committed so every run starts from a useful base.

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
