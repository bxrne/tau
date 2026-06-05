# Tau

**A bitemporal time series database. Corrections, restatements and out of order arrivals are first class. Old values are never overwritten and every query returns the right answer at any point in time.**

1. **Corrections are first class.** Every append is an immutable layer. The newest layer wins where layers overlap. Old values stay on disk.
2. **Compaction is a provable normalisation.** A sweep line algorithm collapses N layers into one canonical layer. Query equivalent before and after. Checked by property based tests on every build.
3. **TauQL.** A tiny query language. One statement in, one response line out. Derived lenses compose as lazy closures. Rolling window aggregations are first class expressions.
4. **Library or server.** Embed `libtau` in a Rust process or run the standalone TCP server. Same engine.

Time series data is not static. Sensors drift. Prices get restated. Audit records get amended. Tau models this directly. Values live in intervals `[start, end)` that tile without gaps or overlap. Corrections append as new layers. The newest layer wins at query time. Compaction collapses any stack of layers into a single canonical form with every query result preserved exactly. The invariants that make this correct are not asserted by hand. They are verified by randomised property tests and a deterministic simulation tester driven against a reference oracle.

**Documentation:** [tau.bxrne.com](https://tau.bxrne.com)


## Install

```bash
# From a release binary (Linux x86_64)
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tau-x86_64-linux -o tau
chmod +x tau && sudo mv tau /usr/local/bin/

# The interactive client
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tauctl-x86_64-linux -o tauctl
chmod +x tauctl && sudo mv tauctl /usr/local/bin/

# Via cargo install (builds from source)
cargo install --git https://github.com/bxrne/tau tau
cargo install --git https://github.com/bxrne/tau tauctl

# Docker
docker pull ghcr.io/bxrne/tau:latest
docker run -p 7070:7070 ghcr.io/bxrne/tau:latest
```


## Quick start

```bash
# No config file needed — starts in-memory on 127.0.0.1:7070
tau

# With a config file
cp config.toml my.toml && $EDITOR my.toml
tau --config my.toml
```

Connect with the interactive client (`tauctl`) and try a correction:

```bash
tauctl
τ connect demo 127.0.0.1:7070
τ CREATE DATABASE sensors
τ CREATE LENS temperature float
τ APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
τ AT LENS temperature 1800
VAL f18.5

τ APPEND LENS temperature 0 3600 20.0   # correction: new layer over same range
τ AT LENS temperature 1800
VAL f20                                  # newest layer wins; prior layer still on disk until compaction
τ REDUCE LENS temperature 0 7200 USING avg
VAL f20.5                                # aggregate reflects the correction
```


## Documentation

- [Tutorial](https://tau.bxrne.com/docs/tutorial/). End-to-end sensor drift correction walkthrough.
- [Overview](https://tau.bxrne.com/docs/overview/). The data model.
- [TauQL Reference](https://tau.bxrne.com/docs/tauql/). Every statement and operator.
- [How it works](https://tau.bxrne.com/docs/how-it-works/). Storage, WAL, compaction, concurrency.
- [Configuration](https://tau.bxrne.com/docs/configuration/). TOML config file reference.
- [Testing](https://tau.bxrne.com/docs/testing/). Property based tests and unit anchors.


## Development

```bash
# Run from source (developer workflow)
git clone https://github.com/bxrne/tau && cd tau
cargo run --release --bin tau             # in-memory server on 127.0.0.1:7070
cargo run --release --bin tauctl          # interactive client

cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --release               # preferred runner (used in CI)

# Parser fuzzing (requires nightly + cargo-fuzz)
cargo install cargo-fuzz
cargo +nightly fuzz run parse             # run from repo root
```



## License

[Apache License 2.0](LICENSE)
