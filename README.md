# Tau

[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=bxrne_tau&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=bxrne_tau)

A time-series database built on immutable, layered temporal intervals.

**Documentation:** [tau.bxrne.com](https://tau.bxrne.com)

---

## Quick start

```bash
# Build from source
git clone https://github.com/bxrne/tau && cd tau
cargo run --release                  # in-memory server on 127.0.0.1:7070

# Docker
docker pull ghcr.io/bxrne/tau:latest
docker run --rm -p 7070:7070 ghcr.io/bxrne/tau:latest
```

Connect with the interactive REPL:

```bash
cargo run --release --bin ctl
τ: connect demo 127.0.0.1:7070
τ: CREATE DATABASE demo
τ: CREATE LENS temperature float
τ: APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
τ: AT LENS temperature 1800
VAL f18.5
τ: REDUCE LENS temperature 0 7200 USING avg
VAL f19.75
```

---

## Documentation

- [Overview](https://tau.bxrne.com/docs/overview/) — data model and design philosophy
- [TauQL Reference](https://tau.bxrne.com/docs/tauql/) — complete language reference
- [Configuration](https://tau.bxrne.com/docs/configuration/) — all server flags
- [Containers](https://tau.bxrne.com/docs/containers/) — Docker stack with Prometheus and Grafana
- [Examples](https://tau.bxrne.com/docs/examples/) — worked queries against real datasets
- [Tutorials](https://tau.bxrne.com/docs/tutorials/local/) — local, Docker, and embedded
- [How it works](https://tau.bxrne.com/docs/how-it-works/) — storage model, WAL, compaction
- [Testing](https://tau.bxrne.com/docs/testing/) — unit, property-based, and simulation testing

---

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release
cargo run --release --bin dst -- --quick   # deterministic simulation tester
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and workflow details.

---

## License

[PolyForm Noncommercial License 1.0.0](LICENSE) — free for personal use, research, and education.
