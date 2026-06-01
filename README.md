# Tau

[![Build](https://github.com/bxrne/tau/actions/workflows/sonar.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/sonar.yml)
[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![DST](https://github.com/bxrne/tau/actions/workflows/bench.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/bench.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)

[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=bxrne_tau&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=bxrne_tau)
[![CodeQL](https://github.com/bxrne/tau/actions/workflows/github-code-scanning/codeql/badge.svg)](https://github.com/bxrne/tau/actions/workflows/github-code-scanning/codeql)
[![Dependabot Updates](https://github.com/bxrne/tau/actions/workflows/dependabot/dependabot-updates/badge.svg)](https://github.com/bxrne/tau/actions/workflows/dependabot/dependabot-updates)

**A bitemporal time series database. Corrections, restatements and out of order arrivals are first class. Old values are never overwritten and every query returns the right answer at any point in time.**

1. **Corrections are first class.** Every append is an immutable layer. The newest layer wins where layers overlap. Old values stay on disk.
2. **Compaction is a provable normalisation.** A sweep line algorithm collapses N layers into one canonical layer. Query equivalent before and after. Checked by property based tests on every build.
3. **Deterministic simulation tester.** Inspired by TigerBeetle's DST. Drives the 1BRC dataset against a BTreeMap oracle, injects faults, and is reproducible from a single seed. See [/docs/dst/](https://tau.bxrne.com/docs/dst/).
4. **TauQL.** A tiny query language. One statement in, one response line out. Derived lenses compose as lazy closures. Rolling window aggregations are first class expressions.
5. **Library or server.** Embed `libtau` in a Rust process or run the standalone TCP server. Same engine.

Time series data is not static. Sensors drift. Prices get restated. Audit records get amended. Tau models this directly. Values live in intervals `[start, end)` that tile without gaps or overlap. Corrections append as new layers. The newest layer wins at query time. Compaction collapses any stack of layers into a single canonical form with every query result preserved exactly. The invariants that make this correct are not asserted by hand. They are verified by randomised property tests and a deterministic simulation tester driven against a reference oracle.

**Documentation:** [tau.bxrne.com](https://tau.bxrne.com)


## Quick start

```bash
# From source — no config file needed, starts in-memory on 127.0.0.1:7070
git clone https://github.com/bxrne/tau && cd tau
cargo run --release --bin tau

# With a config file
cp config.toml my.toml && $EDITOR my.toml
cargo run --release --bin tau -- --config my.toml

# Docker
docker pull ghcr.io/bxrne/tau:latest
docker run --rm -p 7070:7070 -v $PWD/config.toml:/data/config.toml:ro \
  ghcr.io/bxrne/tau:latest --config /data/config.toml
```

Connect with the interactive client (`ctl`) and try a correction:

```bash
cargo run --release --bin ctl
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

- [Overview](https://tau.bxrne.com/docs/overview/). The data model.
- [TauQL Reference](https://tau.bxrne.com/docs/tauql/). Every statement and operator.
- [How it works](https://tau.bxrne.com/docs/how-it-works/). Storage, WAL, compaction, concurrency.
- [Configuration](https://tau.bxrne.com/docs/configuration/). TOML config file reference.
- [DST](https://tau.bxrne.com/docs/dst/). The deterministic simulation tester.
- [Testing](https://tau.bxrne.com/docs/testing/). Property based tests and unit anchors.
- [Containers](https://tau.bxrne.com/docs/containers/). Docker stack with Prometheus and Grafana.
- [Examples](https://tau.bxrne.com/docs/examples/). Worked queries against real datasets.


## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --release               # preferred runner (used in CI)
cargo run --release --bin dst -- --tier nano  # deterministic simulation tester (~1 s)
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for full setup and workflow details.


## License

[Apache License 2.0](LICENSE)
