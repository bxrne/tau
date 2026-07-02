# libtau

The core engine library. All binaries depend only on this crate.

## Module map

| Module | Purpose |
|--------|---------|
| `model` | `Tau<V>` (an N-axis orthotope: `coords[0]` valid time + optional filter axes), `Layer<V>` — the core temporal primitives (a "lens" is a named base/derived/materialised entry tracked by the executor, not a standalone type) |
| `value` | `Value` enum (Int/Float/Str/Bool/Null) + tagged wire encoding |
| `ql/ast` | TauQL AST (`Stmt`, `Expr`, `Literal`, `Type`, `BinOp` …) |
| `ql/parser` | `nom`-based parser; entry point `parse()`, scalar helper `parse_literal()` |
| `storage/store` | `Store<V>` trait; per-generation sweep-line compaction (`compact_layers`) — compacts within each `written_at` transaction-time generation so `AS OF`/`HISTORY` survive |
| `storage/memory` | `InMemory<V>` — `FxHashMap`-backed, zero I/O |
| `storage/disk` | `Disk<V>` — compressed binary file persisting layer data **and** schema DDL; optional AES-256-GCM encryption |
| `storage/wal` | `Wal` — write-ahead log with schema DDL replay |
| `database` | `Database<V>` — owns a `Store` + optional `Wal`; clone-free `Arc<[Layer]>` (RCU) snapshots |
| `executor` | `Executor` — registry of named databases + dispatch + permissions |
| `query` | Pure query evaluator extracted from executor: `eval_lens`, `eval_expr`, aggregation, range bounds |
| `wire` | `Response` — typed wire codec shared by server and clients |
| `users` | `User`, `UserStore`, `Perm` — CRUDA permission system |
| `metrics` | `Metrics` — Prometheus counters |
| `crypto` | AES-256-GCM helpers + hex key parsing |

## Key data-structure choices

Internal maps (`executor.rs`, `query.rs`, `storage/memory.rs`, `storage/store.rs`) use `FxHashMap`/`FxHashSet` from `rustc-hash`. The public `User::grants` field stays on `std::collections::HashMap` to keep the public API stable.

`Database::layers()` returns `Option<Arc<Vec<Layer<V>>>>` so range query phases (bounds collection, segment building) share one snapshot without re-locking the store.

`Layer::new_sorted_unchecked` skips sort + overlap validation for trusted bulk-load callers (`BATCH APPEND`, `COPY`). A `debug_assert` catches misuse in test builds.

## Adding a new statement

Edit these four files in order:

- `ql/ast.rs` — add `Stmt` variant + `Display` impl; check if `needs_registry_lock` needs updating
- `ql/parser.rs` — add nom production + register in the top-level `alt`
- `executor.rs` — add handler + `check_permission` arm + `is_read_only` if needed; add to `exec_db_write` if it is a data write
- `wire.rs` — add response shape to `Response::from_output` and `Response::parse`
