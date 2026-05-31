# dst — 1BRC Deterministic Simulation Tester

Correctness verification, fault injection, and throughput measurement driven by
the One Billion Row Challenge dataset shape.

## Dataset model

~413 station names, each mapped to one Base lens in tau. Every reading is a
degenerate tau `[t, t+1)` with `value = temperature × 10` (i64 fixed-point).
After ingest, `REDUCE min/max/avg` per station is cross-checked against a
BTreeMap oracle. A fault is injected every 5,000 rows: one station's lens is
dropped and recreated (simulating a connection reset), and the oracle is reset
to match.

## Tiers

| Tier | Rows | Use |
|------|------|-----|
| `nano` | 10 k | CI correctness smoke (<1 s) |
| `micro` | 1 M | PR-time perf sanity |
| `small` | 100 M | nightly / dedicated runner |
| `full` | 1 B | manual / release benchmarking |

## Usage

```bash
# CI smoke (runs in ~1 s)
cargo run --release --bin dst -- --tier nano

# Reproducible run from a known seed
cargo run --release --bin dst -- --tier nano --seed 3735928559

# Disable fault injection
cargo run --release --bin dst -- --tier micro --no-faults

# Quiet output
cargo run --release --bin dst -- --tier nano --log-level error
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--tier` | `nano` | Workload tier: nano, micro, small, full |
| `--seed N` | time-based | RNG seed (printed on every run for reproducibility) |
| `--no-faults` | off | Disable fault injection |
| `--log-level` | `info` | Tracing log level |

## On failure

The seed printed at startup is sufficient to reproduce the failure:

```bash
cargo run --release --bin dst -- --tier nano --seed <printed-seed>
```

## Design

The DST runs in embedded mode (library executor directly, no server process).
It uses `libharness` for:
- `OneBrcGen` — deterministic reading generator (station + temperature)
- `Oracle` — BTreeMap reference implementation for cross-checking
- `SeedTree` — hierarchical seed derivation so sub-streams are independent

See `crates/libharness/src/` for the shared harness components.
