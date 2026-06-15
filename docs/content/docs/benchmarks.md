+++
title = "Benchmarks"
date = 2026-06-15
template = "page.html"
+++

The `bench` crate (`crates/bench`) runs deterministic TauQL workloads against tau and reports
throughput and latency. This page covers why it exists, what it measures, and a set of
limited-scale results with the exact command, seed, scale, and commit needed to reproduce
them.

## Why a custom harness

TauQL's data model does not map cleanly onto generic time-series or KV benchmark suites:
TSBS-style workloads assume immutable point writes, and YCSB has no notion of a correction
creating a new layer over an existing range. `bench` generates TauQL statement sequences that
exercise tau's actual operations instead: appends, corrections (re-appending over the same
range), point lookups, range scans, aggregates, derived lenses, and compaction.

## Workloads

| Workload | Exercises |
|----------|-----------|
| `append-heavy` | Sequential `APPEND LENS` over disjoint ranges |
| `correction-heavy` | Repeated `APPEND LENS` over the same ranges (new layer each time) |
| `point-query` | `AT LENS` lookups across the appended history |
| `range-scan` | `RANGE LENS` over windows spanning many layers |
| `reduce-agg` | `REDUCE LENS` aggregates (avg, sum, min, max, count) |
| `derived-lens` | `DERIVE LENS ... AS (...)` then queries against it |
| `compaction-stress` | Enough appends to repeatedly cross `COMPACT_THRESHOLD` |

## Two layers

- **Engine** runs the workload directly against `libtau::Executor`, with no network involved.
  This isolates the domain logic and storage backend.
- **Wire** runs the same workload against a `tau` server spawned in-process
  (`tau::harness::EphemeralServer`), over TCP, optionally with TLS and `AUTH`. This adds the
  wire codec, connection handling, and (optionally) TLS/auth.

## Config grid

Each run picks a `ConfigCell`: storage backend (memory or disk), WAL on/off (and fsync mode),
TLS, auth, AES-256-GCM encryption at rest, zstd compression level, and compaction threshold.
Presets bundle related cells:

| Preset | Cells |
|--------|-------|
| `quick` | memory, memory+wal, disk, tls |
| `security` | plain, tls, auth, tls+auth, wal+encryption, disk+encryption, disk+tls+auth+encryption |
| `storage` | memory, wal fsync-each, wal grouped, disk zstd1, disk zstd19, low/high compaction threshold |
| `full` | full cartesian product of backend x wal x tls x auth x encryption (32 cells, opt-in) |

See [`crates/bench/README.md`](https://github.com/bxrne/tau/tree/master/crates/bench) for the
exact cell definitions.

## Methodology

All numbers below:

- commit `c437d88`
- `--seed 42 --scale 2000`
- engine layer (in-process `Executor`, no network)
- single host, single run (not averaged across repeats)
- produced with:

```bash
cargo run --release -p bench --bin benchtau -- --preset security --scale 2000 --format json
cargo run --release -p bench --bin benchtau -- --preset storage --scale 2000 --format json
```

These are limited-scale, single-host numbers meant to compare configurations and catch
regressions over time. They are **not** competitive benchmarks against other databases, and
they will vary across machines. Always quote the seed, scale, cell, and commit alongside any
number you cite.

For a reproducible, resource-capped run (fixed CPU/memory caps so numbers are comparable
across machines), use the Docker stack:

```bash
docker compose -f container/docker-compose.bench.yml up
```

See [Containers](/docs/containers/) for the caps and environment variables.

## Results: security grid (engine layer)

`append-heavy` and `point-query`, across the `security` preset cells. Full results for all
seven workloads and all cells are in the JSON output above.

| Cell | append-heavy ops/s | append-heavy p99 (us) | point-query ops/s | point-query p99 (us) |
|------|--------------------:|----------------------:|--------------------:|-----------------------:|
| plain | 82,974 | 173.6 | 2,017,583 | 0.52 |
| tls | 83,493 | 172.2 | 2,069,200 | 0.52 |
| auth | 93,947 | 145.7 | 2,117,063 | 0.54 |
| tls+auth | 99,489 | 147.2 | 2,150,276 | 0.53 |
| wal+encryption | 97,669 | 148.1 | 2,190,238 | 0.49 |
| disk+encryption | 31,093 | 393.9 | 2,116,543 | 0.52 |
| disk+tls+auth+encryption | 30,768 | 403.7 | 1,978,735 | 0.57 |

Point queries are effectively free of TLS/auth/encryption overhead at the engine layer (all
cells land around 2M ops/s, since TLS and auth apply to the wire layer, not the engine). The
disk backend's cost is dominated by `Disk::flush()` running on every append (compress, encrypt
if enabled, atomic rename) - encryption itself adds little on top of that.

## Results: storage grid (engine layer)

`append-heavy` and `compaction-stress`, across the `storage` preset cells.

| Cell | append-heavy ops/s | compaction-stress ops/s |
|------|--------------------:|--------------------------:|
| memory | 85,613 | 1,144,923 |
| memory, wal fsync-each | 88,524 | 1,149,992 |
| memory, wal grouped | 93,276 | 1,113,569 |
| disk, zstd level 1 | 34,259 | 159,799 |
| disk, zstd level 19 | 268 | 321 |
| compaction threshold 4 | 48,967 | 924,074 |
| compaction threshold 64 | 499,578 | 1,252,950 |

Two things stand out:

- **zstd level 19 on the disk backend is roughly 130x slower than level 1** for
  append-heavy and compaction-stress, because every append calls `Disk::flush()`, which
  recompresses the entire file at the configured level. Level 19 is not a realistic default
  for write-heavy workloads; `DEFAULT_ZSTD_LEVEL` (3) is much closer to level 1's numbers than
  level 19's.
- **A higher compaction threshold (64) gives roughly 10x the append throughput of a low one
  (4)** on the memory backend, since compaction runs less often. The trade-off is more layers
  to scan per query between compactions.

## Reproducibility contract

Any number quoted in tau's docs, README, demo slides, or blog posts must state:

1. the seed and scale,
2. the config cell,
3. whether it is the engine or wire layer,
4. resource caps, if run via the capped Docker stack, and
5. the commit.

If you cannot reproduce a number from its stated parameters, treat it as stale and re-run
`benchtau`.
