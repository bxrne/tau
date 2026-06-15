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

All numbers below come from the resource-capped Docker stack, not a bare `cargo run`, so
they are comparable across machines:

- source: [`tau-v0.4.0`](https://github.com/bxrne/tau/tree/tau-v0.4.0) plus the
  checkpoint-throttling fix to `Database::append` (`CHECKPOINT_COMPACTION_INTERVAL`),
  landed after that tag
- `--seed 42 --scale 2000`
- engine layer (in-process `Executor`, no network)
- Docker stack caps: `1.0` CPU, `512M` memory (the stack defaults - see
  [Containers](/docs/containers/))
- build host: 16-core `Intel(R) Core(TM) i9-9980HK @ 2.40GHz`, 32 GiB RAM (irrelevant to the
  numbers themselves beyond "had enough headroom to not throttle the 1-CPU/512M container")
- single run (not averaged across repeats)
- produced with:

```bash
TAU_BENCH_PRESET=security TAU_BENCH_SCALE=2000 TAU_BENCH_SEED=42 \
  docker compose -f container/docker-compose.bench.yml up --build --abort-on-container-exit
TAU_BENCH_PRESET=storage TAU_BENCH_SCALE=2000 TAU_BENCH_SEED=42 \
  docker compose -f container/docker-compose.bench.yml up --abort-on-container-exit
```

Each run writes `/data/results.json` inside the `tau-bench` container (the `bench_results`
volume); copy it out with `docker cp tau-bench:/data/results.json .`.

These are limited-scale numbers meant to compare configurations and catch regressions over
time. They are **not** competitive benchmarks against other databases, and absolute
throughput will still vary with the host's available CPU even under the cap. Always quote
the seed, scale, cell, and source tag/commit alongside any number you cite.

## Results: security grid (engine layer)

`append-heavy` and `point-query`, across the `security` preset cells. Full results for all
seven workloads and all cells are in `results.json`.

| Cell | append-heavy ops/s | append-heavy p99 (us) | point-query ops/s | point-query p99 (us) |
|------|--------------------:|----------------------:|--------------------:|-----------------------:|
| plain | 35,964 | 369.85 | 1,452,668 | 0.734 |
| tls | 37,703 | 377.19 | 1,442,957 | 0.754 |
| auth | 38,161 | 371.15 | 1,434,110 | 0.759 |
| tls+auth | 35,749 | 398.73 | 1,313,380 | 0.949 |
| wal+encryption | 38,364 | 396.87 | 1,324,244 | 0.835 |
| disk+encryption | 25,441 | 732.81 | 1,375,618 | 0.821 |
| disk+tls+auth+encryption | 24,961 | 700.42 | 1,335,417 | 0.826 |

Point queries are effectively free of TLS/auth/encryption overhead at the engine layer (all
cells land around 1.3-1.5M ops/s, since TLS and auth apply to the wire layer, not the
engine). The disk cells run at roughly 0.65-0.70x of the plain/memory cells for append-heavy.
`Database::append` checkpoints (and therefore runs `Disk::flush`: compress + optionally
encrypt + atomic rename of the whole file) at most every `CHECKPOINT_COMPACTION_INTERVAL`
compactions, which bounds how often that cost is paid. Encryption itself adds little on top
of the disk backend's remaining cost.

## Results: storage grid (engine layer)

`append-heavy` and `compaction-stress`, across the `storage` preset cells.

| Cell | append-heavy ops/s | compaction-stress ops/s |
|------|--------------------:|--------------------------:|
| memory | 36,781 | 647,331 |
| memory, wal fsync-each | 40,629 | 739,414 |
| memory, wal grouped | 40,175 | 781,521 |
| disk, zstd level 1 | 30,489 | 247,403 |
| disk, zstd level 19 | 4,381 | 9,802 |
| compaction threshold 4 | 20,900 | 582,596 |
| compaction threshold 64 | 257,382 | 707,397 |

Two things stand out:

- **The disk backend at zstd level 1 is close to memory** (30,489 vs 36,781 ops/s
  append-heavy, 247,403 vs 647,331 ops/s compaction-stress). `Disk::flush()` (which
  recompresses the whole file) runs at most every `CHECKPOINT_COMPACTION_INTERVAL`
  compactions, so its cost is amortised across many appends.
- **zstd level 1 and level 19 are the two ends of the compression range the storage grid
  exercises** - level 1 is the fastest setting, level 19 is close to zstd's maximum, and the
  real default (`DEFAULT_ZSTD_LEVEL`, 3) sits much closer to level 1 in practice. zstd level
  19 is roughly 7-25x slower than level 1 for append-heavy and compaction-stress, because each
  `Disk::flush()` that fires recompresses the entire file at that level. Level 19 is not a
  sane default for write-heavy workloads; it is included to show the cost of choosing a
  high compression level, not as a recommendation.
- **A higher compaction threshold (64) gives roughly 12x the append throughput of a low one
  (4)** on the memory backend, since compaction runs less often. The trade-off is more layers
  to scan per query between compactions.

## Reproducibility contract

Any number quoted in tau's docs, README, demo slides, or blog posts must state:

1. the seed and scale,
2. the config cell,
3. whether it is the engine or wire layer,
4. resource caps, if run via the capped Docker stack, and
5. a tag (e.g. [`tau-v0.4.0`](https://github.com/bxrne/tau/tree/tau-v0.4.0)), linked, plus any
   changes on top of it.

If you cannot reproduce a number from its stated parameters, treat it as stale and re-run
`benchtau`.
