# Contributing

## Prerequisites

- **Rust 1.94.1** -- the toolchain is pinned in `rust-toolchain.toml`. `rustup` picks it up automatically on first use.
- **Python 3.8+** -- required by the Hegel property-based test runner. It auto-installs a `uv`-managed shim to `~/.cache/hegel` on first test run; no manual setup needed.

---

## Machine Setup

### Linux (Arch / Debian / Ubuntu)

```bash
# Install Rust via rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# The toolchain version is pinned; rustup resolves it automatically
cargo build --release
```

For cross-compilation to `aarch64-unknown-linux-gnu` (used by CI releases):

```bash
# Install Zig for cargo-zigbuild (avoids glibc version mismatches)
wget https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz
tar -xf zig-linux-x86_64-0.13.0.tar.xz && mv zig-linux-x86_64-0.13.0 ~/zig
export PATH="$HOME/zig:$PATH"

cargo install cargo-zigbuild
rustup target add aarch64-unknown-linux-gnu

cargo zigbuild --release --target aarch64-unknown-linux-gnu
```

### macOS (Apple Silicon or Intel)

```bash
# Install Rust via rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Python 3 ships with Xcode Command Line Tools; verify
python3 --version   # must be >= 3.8

cargo build --release
```

The Hegel test runner installs `uv` to `~/.cache/hegel` on first run. No additional steps needed.

### Docker (any platform)

To build and run locally using the container stack:

```bash
cd container
docker compose up --build
```

This starts Tau on `127.0.0.1:7070`, Prometheus on `:9090`, and Grafana on `:3000` (default credentials `admin` / `admin`).

To run a one-off server without Compose:

```bash
docker build -f container/Dockerfile -t tau .
docker run --rm -p 7070:7070 tau
```

---

## Development Workflow

```bash
# Check formatting
cargo fmt --check

# Build
cargo build --release

# Lint (warnings are errors in CI)
cargo clippy --all-targets --all-features -- -D warnings

# Run all tests (unit + Hegel PBT; ~3 minutes on first run, faster after cache warms)
cargo test --release

# Parallel test runner with nicer output
cargo nextest run

# Run only library tests
cargo test --release --lib

# Run a single test by name substring
cargo test --release <test_name_substring>
```

See [TEST.md](TEST.md) for a full explanation of the three test layers and when to use each.

---

## Running the Server

```bash
# Plain server on 127.0.0.1:7070
cargo run --release

# With TLS (ephemeral self-signed cert for dev)
cargo run --release -- --tls

# With auth (bootstraps an admin user on first run)
cargo run --release -- --auth --users-file /tmp/tau-users.json \
  --username admin --password hunter2

# With encryption at rest
export TAU_ENCRYPTION_KEY=000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
cargo run --release -- --data /tmp/tau-data
```

The interactive REPL (`tauctl`) connects to a running server:

```bash
cargo run --release --bin ctl

# Inside the REPL:
connect localhost:7070
auth admin hunter2
CREATE DATABASE sensors
CREATE LENS temperature int
APPEND LENS temperature 0 100 22
AT LENS temperature 50
```

---

## Benchmarking

```bash
# Quick run on tmpfs (measures compute cost; fsync is a no-op on tmpfs)
cargo run --release --bin bench -- --quick

# Real-disk run (measures fsync cost)
cargo run --release --bin bench -- --scratch /path/to/real/disk --out results.csv
```

Note: `/tmp` is tmpfs on most Linux systems. Use a path on a real disk to measure fsync latency accurately. See [ARCHITECTURE.md](ARCHITECTURE.md) for the bench vs. dst distinction.

---

## Adding a Statement to TauQL

Every new statement requires changes to four files:

1. `src/libtau/ql/ast.rs` -- add the `Stmt` variant and its `Display` impl
2. `src/libtau/ql/parser.rs` -- add the `nom` production and register it in the top-level `alt`
3. `src/libtau/executor.rs` -- add the handler branch, the `check_permission` arm, and update `is_read_only` if needed
4. `src/bin/tau/main.rs` -- add the output formatter in `format_output` / `format_error`

---

## Code Style

- **Edition 2024** -- `let ... && let` syntax and other edition features are in use.
- **No comments on obvious code** -- comment only the non-obvious: a hidden constraint, a workaround for a specific bug, an invariant that would surprise a reader.
- **Tests in the same file** -- every module has a `#[cfg(test)] mod tests` block. There is no `tests/` directory.
- **Hegel for invariants, `#[test]` for regression anchors** -- use property-based tests for anything that can be stated as "this must hold for any input"; use example-based tests for known-shape inputs where drift is a bug.

---

## CI

CI runs on every push and pull request:

1. `cargo fmt --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo nextest run`

All three must pass before a PR is merged.
