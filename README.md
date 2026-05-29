# Tau

[![Build](https://github.com/bxrne/tau/actions/workflows/sonar.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/sonar.yml)
[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![DST](https://github.com/bxrne/tau/actions/workflows/bench.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/bench.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)

[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=bxrne_tau&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=bxrne_tau)
[![CodeQL](https://github.com/bxrne/tau/actions/workflows/github-code-scanning/codeql/badge.svg)](https://github.com/bxrne/tau/actions/workflows/github-code-scanning/codeql)
[![Dependabot Updates](https://github.com/bxrne/tau/actions/workflows/dependabot/dependabot-updates/badge.svg)](https://github.com/bxrne/tau/actions/workflows/dependabot/dependabot-updates)

**A time-series database built on algebraically precise temporal intervals, verified by property-based tests and deterministic simulation.**

Tau models time as a sequence of half-open intervals `[start, end)` that tile without gaps or overlap. Corrections append as new layers; the newest layer wins at query time. Compaction normalises any stack of layers into a single canonical layer — every query result is preserved exactly. The invariants that make this correct are not asserted by hand: they are verified by randomised property tests and a simulation tester that runs every configuration combination against a reference oracle.

**Documentation:** [tau.bxrne.com](https://tau.bxrne.com)


## Why Tau

- **Algebraic interval model.** Half-open intervals `[start, end)` form a monoid under concatenation. Adjacent intervals tile cleanly; there are no boundary ambiguities. The data model has a formal structure, not an ad-hoc one.
- **Layers as a total order.** Every append creates a layer with a monotonically increasing ID. Conflict resolution is not a policy choice — it is a deterministic rule: the layer with the highest ID wins at each point. No locks, no coordination, no ambiguity.
- **Compaction is a normalisation, not a lossy operation.** The sweep-line compaction algorithm produces a canonical single layer that is query-equivalent to any stack of N layers. Every `AT`, `RANGE`, and `REDUCE` result is identical before and after. This is a provable property, not a claim — it is checked by property-based tests on every build.
- **Derived lenses compose.** `DERIVE LENS f AS expr` compiles the expression into a lazy closure at definition time. Closures capture other lens closures, so derivations chain. Cycle detection runs at `DERIVE` time by walking the dependency graph.
- **Verified by PBT and DST.** Algebraic invariants (`Tau::contains`, `Layer::at`, `compact_layers` query-equivalence, WAL roundtrip) are checked by Hegel/Hypothesis against hundreds of randomised inputs per property. The deterministic simulation tester drives every transport × auth × WAL combination against a reference oracle, injecting faults across hundreds of millions of simulated operations.


## Quick start

```bash
# From source
git clone https://github.com/bxrne/tau && cd tau
cargo run --release                  # in-memory server on 127.0.0.1:7070

# Docker
docker pull ghcr.io/bxrne/tau:latest
docker run --rm -p 7070:7070 ghcr.io/bxrne/tau:latest
```

Connect with the interactive REPL and try a correction:

```bash
cargo run --release --bin ctl
τ: connect demo 127.0.0.1:7070
τ: CREATE DATABASE sensors
τ: CREATE LENS temperature float
τ: APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
τ: AT LENS temperature 1800
VAL f18.5

τ: APPEND LENS temperature 0 3600 20.0   # correction: new layer over same range
τ: AT LENS temperature 1800
VAL f20                                   # newest layer wins; prior layer still on disk until compaction
τ: REDUCE LENS temperature 0 7200 USING avg
VAL f20.5                                 # aggregate reflects the correction
```


## Documentation

- [Overview](https://tau.bxrne.com/docs/overview/) — the data model and its algebraic properties
- [TauQL Reference](https://tau.bxrne.com/docs/tauql/) — every statement and operator
- [How it works](https://tau.bxrne.com/docs/how-it-works/) — storage, WAL, compaction, concurrency
- [Testing](https://tau.bxrne.com/docs/testing/) — property-based tests and the deterministic simulation tester
- [Configuration](https://tau.bxrne.com/docs/configuration/) — all server flags and environment variables
- [Containers](https://tau.bxrne.com/docs/containers/) — Docker stack with Prometheus and Grafana
- [Examples](https://tau.bxrne.com/docs/examples/) — worked queries against real datasets
- [Tutorials](https://tau.bxrne.com/docs/tutorials/local/) — local, Docker, and embedded


## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release                          # unit tests + property-based tests
cargo run --release --bin dst -- --quick      # deterministic simulation tester
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and workflow details.


## License

[Apache License 2.0](LICENSE)
