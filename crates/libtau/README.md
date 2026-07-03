# libtau

The tau engine library. Everything else (`tau`, `tauctl`) depends on this crate.

## Data model

Three primitive types form everything:

| Type | Description |
|------|-------------|
| `Tau<V>` | An immutable value `V` over one half-open `[lo, hi)` interval per axis (`coords`); axis 0 is valid time, a multi-axis lens (`AXES (…)`) adds filter axes so a tau is an N-orthotope |
| `Layer<V>` | A batch of taus sharing a `written_at` transaction time, `Arc`-backed so clones are pointer bumps |
| lens | A named temporal function. **Base** lenses are storage-backed with a declared `Type`; **derived** lenses (`DERIVE`) are a TauQL `Expr` AST evaluated lazily at query time (no caching); **materialised** lenses (`XDERIVE`) evaluate the same expression eagerly into stored layers and auto-refresh when a source lens is corrected. The executor tracks these kinds in separate `DbState` maps — there is no single `Lens` type. |

Layers are append-only; **newest-layer-wins** on overlap at query time. Auto-compaction normalises layers **within** each transaction-time generation (never across, so `AT … AS OF`/`HISTORY` survive) once a per-lens threshold is crossed — a sweep line for single-axis lenses, orthotope subtraction for multi-axis.

## Module layout

| Module | Purpose |
|--------|---------|
| `model` | `Tau`, `Layer` — the core temporal types |
| `executor` | `Executor` — owns named databases, dispatches TauQL statements |
| `query` | Pure query evaluator — `eval_lens`, `eval_expr`, aggregations |
| `database` | `Database<V>` — wraps a store + optional WAL |
| `storage` | `Store<V>` trait; `InMemory` and `Sstable` (on-disk) implementations; `Wal` |
| `ql` | `ast`, `parser` — TauQL grammar and statement types. Statement keywords are UPPERCASE-only; type names, aggregate functions and value literals are lowercase. `format_parse_error` renders a column-anchored, human-readable message instead of nom's debug output. |
| `wire` | `Response` — shared wire codec for server encoder and client decoder |
| `users` | `User`, `UserStore`, `Perm` — multi-user auth and CRUDA grants |
| `value` | `Value` — the runtime value type with type-tagged codec |
| `metrics` | `Metrics` — Prometheus counters and histograms |
| `crypto` | AES-256-GCM at-rest encryption helpers |

## Usage

```rust
use libtau::{Executor, parse};

let mut exec = Executor::new();
let (_, stmt) = parse("CREATE DATABASE mydb").unwrap();
exec.exec(&stmt).unwrap();

let (_, stmt) = parse("CREATE LENS temperature float").unwrap();
exec.exec(&stmt).unwrap();

let (_, stmt) = parse("APPEND LENS temperature 0 3600 21.5").unwrap();
exec.exec(&stmt).unwrap();

let (_, stmt) = parse("AT LENS temperature 1800").unwrap();
let result = exec.exec_read(&stmt).unwrap();
```

## Key invariants

- `exec` / `exec_read` are unrestricted — for library consumers and tests.
- `exec_as` / `exec_read_as` enforce CRUDA grants — used by the TCP server.
- Append order: **WAL first, then store**. A WAL fsync failure leaves in-memory state unchanged.
- `Database::layers()` returns `Option<Arc<[Layer<V>]>>` — query phases share one snapshot without re-locking.

## Performance levers

| Flag / setter | Effect |
|---------------|--------|
| `Database::set_wal_fsync_each(false)` + `wal_flush()` | Group-commit mode: batch WAL flushes every 50 ms |
| `Sstable::set_compression_level(n)` | zstd level 1–22 for the disk backend (1 = fastest, 22 = best ratio) |
| `Database::set_wal_max_bytes(n)` | Trigger a WAL checkpoint rewrite when the file exceeds `n` bytes, bounding on-disk WAL growth between compactions |
