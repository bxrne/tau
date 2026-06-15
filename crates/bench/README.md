# bench

Deterministic benchmark workloads and runner for tau, used to produce reproducible
throughput/latency numbers for the docs, demo slides, and blog posts.

## Why a custom harness, not TSBS/YCSB

TauQL's data model (`Tau<V>` over `[start, end)`, layers, newest-layer-wins corrections,
derived lenses) does not map onto standard time-series or KV benchmark suites without
distortion: TSBS-style workloads assume immutable point writes, and YCSB has no notion of a
correction creating a new layer over an existing range. Rather than bend tau's semantics to
fit a generic harness (or bend a generic harness's results to mean something for tau), `bench`
generates TauQL workloads that exercise tau's actual operations: appends, corrections, point
lookups, range scans, aggregates, derived lenses, and compaction.

## Workloads

Each workload is a deterministic sequence of TauQL statements (text, generated via
`libtau::parse` at run time), seeded so that the same `--seed`/`--scale` always produces the
same statements:

| Workload | Exercises |
|----------|-----------|
| `append-heavy` | Sequential `APPEND LENS` over disjoint ranges |
| `correction-heavy` | Repeated `APPEND LENS` over the *same* ranges (new layer each time) |
| `point-query` | `AT LENS` lookups across the appended history |
| `range-scan` | `RANGE LENS` over windows spanning many layers |
| `reduce-agg` | `REDUCE LENS` aggregates (`avg`, `sum`, `min`, `max`, `count`) |
| `derived-lens` | `DERIVE LENS ... AS (...)` followed by queries against the derived lens |
| `compaction-stress` | Enough appends to repeatedly cross `COMPACT_THRESHOLD` and trigger `compact_layers` |

`--workload <name>` runs one; omit it to run all seven. `DEFAULT_SEED = 42`.

## Layers

- **Engine** — `libtau::Executor` directly, via `Executor::exec`/`exec_read`. No network, no
  wire codec. Measures the domain logic and storage backend.
- **Wire** — a live `tau` server spawned in-process with
  `tau::harness::EphemeralServer::spawn`, talked to over TCP (optionally TLS, optionally with
  `AUTH`). Measures the same operations plus the wire codec, connection handling, and
  (optionally) TLS/auth overhead.

`--layer engine|wire|both` (default `both`).

## Config grid

`ConfigCell` (`src/grid.rs`) mirrors the dimensions of `tau::config::Config`: storage backend
(memory/disk), WAL (and fsync mode), TLS, auth, AES-256-GCM encryption-at-rest, zstd
compression level, and compaction threshold. Each cell builds its own `Executor` and, for the
wire layer, its own `EphemeralServer`.

Presets (`--preset <name>`):

| Preset | Cells | Focus |
|--------|-------|-------|
| `quick` | `memory`, `memory_wal`, `disk`, `tls` | Fast sanity sweep |
| `security` | `plain`, `tls`, `auth`, `tls_auth`, `wal_encrypted`, `disk_encrypted`, `disk_tls_auth_encrypted` | TLS/auth/encryption overhead |
| `storage` | `memory`, `memory_wal_fsync`, `memory_wal_grouped`, `disk_zstd1`, `disk_zstd19`, `compact_low`, `compact_high` | Backend, WAL fsync mode, compression, compaction threshold |
| `full` | 32 cells | Full cartesian product of backend x wal x tls x auth x encryption. Opt-in only: large and slow. |

Omit `--preset` to run a single cell with `ConfigCell::default()` (memory, no WAL, no TLS, no
auth, no encryption, default compression/compaction thresholds from `libtau::storage`).

## Running

```bash
# Single workload, single cell, both layers
cargo run --release -p bench --bin benchtau -- --workload append-heavy

# All workloads, the security preset, JSON output
cargo run --release -p bench --bin benchtau -- --preset security --format json

# Write JSON to a file (useful when stdout isn't captured, e.g. in containers)
cargo run --release -p bench --bin benchtau -- --preset quick --format json --output results.json
```

Flags: `--workload`, `--layer {engine,wire,both}`, `--scale`, `--seed`, `--value-type
{int,float,str,bool}`, `--preset {quick,security,storage,full}`, `--format {table,json}`,
`--output <path>`.

## Divan microbenchmarks

```bash
cargo bench -p bench           # both `engine` and `wire` bench targets
cargo bench -p bench --bench engine
cargo bench -p bench --bench wire
```

`benches/engine.rs` covers all seven workloads at `SCALE=200` against memory, plus
append-heavy/point-query/compaction-stress against disk. `benches/wire.rs` covers
plain/TLS/auth wire connections at `SCALE=100`, spawning a fresh `EphemeralServer` per
sample.

## Capped Docker stack

`container/docker-compose.bench.yml` runs `benchtau` in a resource-capped, read-only
container and writes JSON to a volume:

```bash
docker compose -f container/docker-compose.bench.yml up
```

Caps and preset are controlled via `TAU_BENCH_PRESET`, `TAU_BENCH_SCALE`, `TAU_BENCH_SEED`,
`TAU_BENCH_CPU_LIMIT`, `TAU_BENCH_MEM_LIMIT` (see `container/README.md`).

## Reproducibility contract

Every number quoted anywhere (docs, README, slides, blog) must be accompanied by:

- the **seed** and **scale** used,
- the **cell** (config) name,
- the **resource caps** (CPU/memory) if run via Docker, and
- the **commit** the binary was built from.

These are limited-scale, single-host numbers intended to catch regressions and give a rough
sense of overhead between configurations. They are not competitive benchmarks against other
databases.
