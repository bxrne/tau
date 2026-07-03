# Tau

## What it is

A bitemporal time series database. Corrections, restatements and out-of-order arrivals are first class: every append is an immutable layer, the newest layer wins at query time, and old values are never overwritten. Compaction collapses layers into a canonical form with every query result preserved exactly — an invariant checked by property-based tests and deterministic simulation testing on every build.

The engine is a syscall-routing microkernel (`libtau`): embed it in a Rust process, or run the standalone TCP server with the `tauctl` interactive client — same engine either way. Queries use TauQL, a small line-oriented query language.

Full documentation, tutorial, and TauQL reference: [tau.bxrne.com](https://tau.bxrne.com)

## Using it

```bash
# Release binaries (Linux x86_64)
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tau-x86_64-linux -o tau
chmod +x tau && sudo mv tau /usr/local/bin/
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tauctl-x86_64-linux -o tauctl
chmod +x tauctl && sudo mv tauctl /usr/local/bin/

# From source / Docker
cargo install --git https://github.com/bxrne/tau tau tauctl
docker run -p 7070:7070 ghcr.io/bxrne/tau:latest

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

τ DERIVE LENS fahrenheit AS temperature * 1.8 + 32        # lazy, recomputed per query
τ XDERIVE LENS fahrenheit_mat AS temperature * 1.8 + 32   # materialised, auto-refreshes
```

Casing carries meaning: **TauQL keywords are UPPERCASE** (`CREATE`, `APPEND`, `AT`, …); **`tauctl` meta-commands are lowercase** (`connect`, `use`, `load`). So `use` switches the client connection, `USE` switches the server database.

The server takes a TOML config (`tau --config my.toml`) covering the storage backend, WAL, TLS, auth, metrics, and limits; a production example lives at [container/tau-config.toml](container/tau-config.toml) and the Docker stack reads secrets (like the optional `TAU_ENCRYPTION_KEY` for AES-256-GCM at-rest encryption) from `.env`. See the [tutorial](https://tau.bxrne.com/docs/tutorial/), [TauQL reference](https://tau.bxrne.com/docs/tauql/), and [configuration](https://tau.bxrne.com/docs/configuration/) docs.

## Developing

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --release
cargo run --release --bin dst -- --seed 42
```

The workspace layout and per-crate details are in [crates/README.md](crates/README.md); architecture rationale is in [how it works](https://tau.bxrne.com/docs/how-it-works/). Licensed under [Apache 2.0](LICENSE).
