# dst - Deterministic Simulation Tester

Correctness verification and throughput measurement in one binary. Replaces the former standalone bench binary.

## Two modes

**Embedded (`--quick`):** uses the `libtau` executor directly, no server process, no I/O. Simulates centuries of temporal data in seconds. Suitable for CI - runs as part of every push.

**Full (default):** spawns a real `tau` server for each config cell in the matrix (Transport x Auth x WAL), drives traffic over TCP, cross-checks every response against a simple oracle, injects faults (connection drops, WAL truncation), and scrapes Prometheus metrics to verify statement counts. Outputs a table of results and optionally writes CSV.

## Config matrix

| Transport | Auth | WAL |
|-----------|------|-----|
| plain | none | off |
| plain | none | on |
| plain | password | off |
| plain | password | on |
| TLS | none | off |
| TLS | none | on |
| TLS | password | off |
| TLS | password | on |

## Usage

```bash
# Embedded mode: fast, no server, CI-suitable (30s default)
cargo run --release --bin dst -- --quick

# Full mode: all 8 config cells, server processes, fault injection
cargo run --release --bin dst

# Reproducible run from a known seed
cargo run --release --bin dst -- --quick --seed 575573495

# Full mode with CSV output and real-disk WAL scratch
cargo run --release --bin dst -- --scratch /var/tmp/tau --out results.csv
```

## Options

| flag | default | description |
|------|---------|-------------|
| `--quick` | off | Embedded executor mode; no server processes |
| `--seed N` | time-based | RNG seed; printed on every run for reproducibility |
| `--duration N` | 30 | Seconds to run in embedded mode |
| `--ops N` | 2000 | Operations per config cell in full mode |
| `--readers N` | 8 | Concurrent reader threads in embedded mode |
| `--fault-interval N` | 500 | Inject a fault every N ops in full mode |
| `--scratch DIR` | $TMPDIR | WAL scratch directory (use a real disk path for accurate fsync timing) |
| `--out PATH` | none | Write CSV results to path |
| `--label NAME` | run | Tag attached to every CSV row |
| `--verbose` | off | Print every operation |

## Oracle

The embedded simulation and each full-mode cell are cross-checked against a simple reference implementation: a `BTreeMap<start, (end, value)>` per lens with O(log n) lookups. It has no layers, no compaction, no WAL - just obviously correct temporal semantics. Any divergence between the oracle and the executor is a bug.

## Fault injection

Two fault types are injected in full mode:

- **Connection drop:** the client TCP connection is dropped and reconnected. Verifies that the server accepts reconnections and that previously written data is still readable.
- **WAL truncation:** the WAL file is truncated by 16 bytes to simulate a partial write. On the next server restart, the WAL must replay cleanly without panic or silent data loss.

## On failure

When any invariant is violated, the DST prints:

1. The seed that reproduces the failure
2. The invariant that was violated
3. The expected value (oracle) and the actual value (executor)
4. The exact command to reproduce

The seed alone is sufficient to reproduce the failure on the same binary:

```bash
cargo run --release --bin dst -- --quick --seed <printed-seed>
```

See [`TEST.md`](../../../TEST.md) for how the DST fits into the three-layer testing strategy.
