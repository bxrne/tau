# Tau

A bitemporal time series database. Corrections, restatements and out of order arrivals are first class: every append is an immutable layer, the newest layer wins at query time, and old values are never overwritten. Compaction collapses layers into a canonical form with every query result preserved exactly — an invariant checked by property based tests and deterministic simulation testing on every build.

Embed `libtau` in a Rust process or run the standalone TCP server with the `tauctl` interactive client. Same engine either way. Queries use TauQL, a small line-oriented query language.

Full documentation, tutorial, and TauQL reference: [tau.bxrne.com](https://tau.bxrne.com)

## Install

```bash
# Release binary (Linux x86_64)
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tau-x86_64-linux -o tau
chmod +x tau && sudo mv tau /usr/local/bin/

# Client
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tauctl-x86_64-linux -o tauctl
chmod +x tauctl && sudo mv tauctl /usr/local/bin/

# From source
cargo install --git https://github.com/bxrne/tau tau
cargo install --git https://github.com/bxrne/tau tauctl

# Docker
docker run -p 7070:7070 ghcr.io/bxrne/tau:latest
```

## Quick start

```bash
tau        # in-memory server on 127.0.0.1:7070, no config needed
tauctl     # interactive client
```

```
τ connect demo 127.0.0.1:7070
τ CREATE DATABASE sensors
τ CREATE LENS temperature float
τ APPEND LENS temperature 0 3600 18.5
τ AT LENS temperature 1800
VAL f18.5

τ APPEND LENS temperature 0 3600 20.0   # correction: new layer over same range
τ AT LENS temperature 1800
VAL f20                                  # newest layer wins; prior value preserved
```

See the [tutorial](https://tau.bxrne.com/docs/tutorial/) for an end-to-end walkthrough and the [TauQL reference](https://tau.bxrne.com/docs/tauql/) for every statement. Storage, configuration, and testing are covered in [how it works](https://tau.bxrne.com/docs/how-it-works/), [configuration](https://tau.bxrne.com/docs/configuration/), and [testing](https://tau.bxrne.com/docs/testing/).

## Configuration

The server takes a TOML config file (`tau --config my.toml`). A production-ready example used by the Docker stack lives at [container/tau-config.toml](container/tau-config.toml):

```toml
bind = "0.0.0.0:7070"
log_level = "info"
compact_threshold = 8

[wal]
enabled = true
path = "/data/tau.wal"

[tls]
enabled = false

[auth]
enabled = true
username = "admin"
password = "changeme_use_a_strong_password"
users_file = "/data/users.db"

[metrics]
port = 9100

[limits]
max_connections = 1024
idle_timeout_secs = 300
```

Secrets and infrastructure overrides come from the environment — copy [container/.env.example](container/.env.example) to `.env` for the Docker stack. The one secret the server itself reads is:

```bash
# Optional AES-256-GCM encryption-at-rest for WAL + disk store.
# 64 hex chars (32 bytes). Generate with: openssl rand -hex 32
TAU_ENCRYPTION_KEY=
```

The rest of `.env.example` covers Docker-level settings: image tag pinning, host bind addresses, resource limits, and Prometheus/Grafana credentials for the bundled monitoring stack. Full reference: [configuration docs](https://tau.bxrne.com/docs/configuration/).

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --release
cargo run --release --bin dst -- --seed 42
```

## License

[Apache License 2.0](LICENSE)
