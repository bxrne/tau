# fuzztau

## What it is

LibFuzzer targets for `libtau`'s text parsers (nightly toolchain required). Five targets cover the attack surfaces: `parse` (TauQL grammar), `wire` (response-line decoder), `value_decode` (tagged value codec), `perm_parse` (CRUDA permission letters), and `parse_literal` (single-literal bulk loads). Each asserts one property: **no panic, crash, or OOM on any input** (RSS capped at 2 GiB); correctness is covered by unit and property tests in `libtau`.

## How it works

`tauql.dict` lists TauQL tokens (UPPERCASE keywords, lowercase type/agg/literal words, operators, wire tokens) so the mutator assembles structurally-valid inputs faster. Committed `seeds/<target>/` hold hand-crafted valid and boundary inputs — including the N-dimensional surface (`AXES (…)`, bracketed boxes, multi-coordinate `AT … AS OF`, `RANGE … AT (…)`). The working corpus grows in gitignored `corpus/`; crashes land in `artifacts/<target>/`. CI builds every target, then runs `parse` and `wire` time-boxed from the seeds + dictionary, failing on any crash and uploading the reproducer. (A binary-format target for `Sstable` run/manifest decoding is worthwhile follow-up.)

## Using it

```bash
rustup toolchain install nightly   # one-time

# Bootstrap a working corpus from seeds, then run (ctrl-c to stop):
mkdir -p crates/fuzztau/corpus/parse && cp crates/fuzztau/seeds/parse/* crates/fuzztau/corpus/parse/
cargo +nightly fuzz run --fuzz-dir crates/fuzztau parse crates/fuzztau/corpus/parse \
    -- -dict=crates/fuzztau/tauql.dict

cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau parse crates/fuzztau/corpus/parse   # minimise
cargo +nightly fuzz run  --fuzz-dir crates/fuzztau wire crates/fuzztau/artifacts/wire/<file>  # reproduce
```

Run from the workspace root, and prefer `corpus/` over `seeds/` for long runs so committed seeds stay small.
