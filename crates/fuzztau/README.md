# fuzztau

LibFuzzer fuzz targets for `libtau`. Requires a nightly Rust toolchain.

## Targets

| Target | Entry point | Attack surface |
|--------|------------|----------------|
| `parse` | `libtau::parse` | `nom` parser — TauQL grammar |
| `wire` | `libtau::Response::parse` | Wire decoder — response line grammar |

## Quick start

```bash
rustup toolchain install nightly   # one-time

# Run indefinitely (ctrl-c to stop)
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire  crates/fuzztau/seeds/wire
cargo +nightly fuzz run --fuzz-dir crates/fuzztau parse crates/fuzztau/seeds/parse

# Cap to 60 seconds
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire crates/fuzztau/seeds/wire \
    -- -max_total_time=60 -print_final_stats=1
```

Run from the workspace root (`/path/to/tau`), not from inside this directory — `cargo fuzz` looks for the workspace `Cargo.toml` by default but `--fuzz-dir` overrides the target location.

## Seed corpus

`seeds/{wire,parse}/` contains hand-crafted inputs covering every response variant and every TauQL statement. These are committed to the repo so every fresh run starts from a good base.

The generated corpus accumulates in `corpus/` (gitignored). After a long run, minimise it to remove redundant inputs while keeping full coverage:

```bash
cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau wire  crates/fuzztau/corpus/wire
cargo +nightly fuzz cmin --fuzz-dir crates/fuzztau parse crates/fuzztau/corpus/parse
```

## Crash reproduction

Crashes are saved to `artifacts/{wire,parse}/`. Reproduce a specific crash:

```bash
cargo +nightly fuzz run --fuzz-dir crates/fuzztau wire artifacts/wire/<filename>
```

## What the targets check

Both targets verify a single invariant: **the function under test must never panic or crash on any input**. They do not assert correctness of the output — that is covered by the property-based tests in `libtau`'s own `mod tests` blocks (`#[hegel::test]`).
