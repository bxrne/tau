# Contributing

## Prerequisites

- **Rust 1.94.1** — the toolchain is pinned in `rust-toolchain.toml`. `rustup` picks it up automatically on first use.
- **Python 3.8+** — required by the Hegel property-based test runner. It auto-installs a `uv`-managed shim to `~/.cache/hegel` on first test run; no manual setup needed.
- **cargo-nextest** — preferred test runner (nicer output, per-process isolation, used in CI). Install once: `cargo install cargo-nextest --locked`.

---

## Machine Setup

### Linux (Arch / Debian / Ubuntu)

```bash
# Install Rust via rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# The toolchain version is pinned; rustup resolves it automatically
cargo build --release

# Install nextest
cargo install cargo-nextest --locked
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
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
python3 --version   # must be >= 3.8
cargo build --release
cargo install cargo-nextest --locked
```

### Docker (any platform)

```bash
cd container

# Copy the sample config files and fill in any customisation
cp .env.example .env
cp tau-config.toml tau-config.local.toml   # optional: keep a local override

# Start the full stack (tau + Prometheus + Grafana)
docker compose up -d --build

# Connect via tauctl
cargo run --release --bin ctl
# τ connect prod 127.0.0.1:7070
# τ AUTH admin changeme_use_a_strong_password
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

# Run all tests — nextest is the preferred runner
cargo nextest run --release

# Run only library tests
cargo nextest run --release --lib

# Run tests for a specific binary
cargo nextest run --release -E 'binary(tau)'
cargo nextest run --release -E 'binary(ctl)'

# Run a single test by name substring
cargo nextest run --release -E 'test(my_test_name)'

# Fallback: cargo test still works for ad-hoc runs
cargo test --release
```

---

## Running the Server

```bash
# Plain in-memory server (uses defaults — no config file needed)
cargo run --release --bin tau

# With a config file
cargo run --release --bin tau -- --config config.toml

# Sample config lives in the repo root; copy and edit
cp config.toml my-tau.toml
$EDITOR my-tau.toml
cargo run --release --bin tau -- --config my-tau.toml
```

Key config snippets:

```toml
# WAL + persistent auth
[wal]
enabled = true
path = "/tmp/tau.wal"

[auth]
enabled = true
username = "admin"
password = "hunter2"
users_file = "/tmp/tau-users.db"
```

The interactive client connects to a running server:

```bash
cargo run --release --bin ctl
# τ connect localhost:7070
# τ AUTH admin hunter2
# τ CREATE DATABASE sensors
# τ CREATE LENS temperature int
# τ APPEND LENS temperature 0 100 22
# τ AT LENS temperature 50
```

---

## Simulation and Performance Testing

```bash
# Embedded mode: fast correctness check (CI default, ~1 s)
cargo run --release --bin dst -- --tier nano

# Reproducible run from a known seed
cargo run --release --bin dst -- --tier nano --seed 3735928559

# Micro tier (1M rows)
cargo run --release --bin dst -- --tier micro
```

---

## Adding a Statement to TauQL

Every new statement requires changes to four files:

1. `crates/libtau/src/ql/ast.rs` — add the `Stmt` variant and its `Display` impl
2. `crates/libtau/src/ql/parser.rs` — add the `nom` production and register it in the top-level `alt`
3. `crates/libtau/src/executor.rs` — add the handler branch, the `check_permission` arm, and update `is_read_only` if needed
4. `crates/libtau/src/wire.rs` — add the response shape to `Response::from_output` and `Response::parse`

---

## Code Style

- **Edition 2024** — `let ... && let` syntax and other edition features are in use.
- **No comments on obvious code** — comment only the non-obvious: a hidden constraint, a workaround for a specific bug, an invariant that would surprise a reader.
- **Tests in the same file** — every module has a `#[cfg(test)] mod tests` block. There is no `tests/` directory.
- **Hegel for invariants, `#[test]` for regression anchors** — use property-based tests for anything that can be stated as "this must hold for any input"; use example-based tests for known-shape inputs where drift is a bug.

---

## CI

CI runs on every push and pull request:

1. `cargo fmt --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo nextest run`

All three must pass before a PR is merged.
