+++
title = "How it works"
date = 2026-05-28
template = "page.html"
+++

Tau is a time-series database for recording how values change over time. It is not a general-purpose relational store: no rows, no tables, no indexes. Only temporal intervals, a query language built around them, and a storage model that makes correction cheap.

This document describes *why* Tau is built the way it is, not just how it works. Decisions that might look odd from the outside have reasons. Knowing the reasons lets you contribute without fighting the grain of the design.

---

## The Problem

Most databases assume a row represents current truth. To record history you add timestamps, but the model stays mutation-oriented: an update replaces the old value.

Tau starts from the opposite assumption. Every fact has a time range over which it was true. A measurement saying "temperature was 22 °C from noon to 1 pm" is a first-class value, not a derived view. Updating it means appending a correction: a new layer covering some or all of the same range with a newer value. The old layer is never touched.

This makes Tau correct by default for append-only workloads:

- Sensor streams where values arrive out of order
- Financial time series where prices are restated
- Audit trails where the history of corrections is itself interesting

The cost is that every query must resolve which layer wins at each point in time. That resolution logic is the sweep-line compaction algorithm and the layered query model.

---

## Architecture

Tau is structured as a library (`libtau`) consumed by two binaries: the TCP server (`tau`) and the interactive client (`tauctl`, a ratatui TUI that requires an interactive terminal). The library exposes a clean `Executor` API; auth, TLS, and network concerns live exclusively in the server.

```
Stmt → Executor → Database<Value> → Store<V> + optional Wal
```

---

## Primitives

### `Tau<V>`

An atomic temporal fact: value `V` is true over the half-open interval `[start, end)`.

```
Tau { start: i64, end: i64, value: V }
```

The half-open interval is intentional. Adjacent intervals tile cleanly: `[0, 10)` and `[10, 20)` cover `[0, 20)` with no overlap and no gap. Equality on the boundary belongs unambiguously to the later interval.

`Tau::new` asserts `start < end`. There are no zero-width taus.

Timestamps are `i64` (nanoseconds, milliseconds, or any other unit the caller agrees on). Tau treats them as opaque ordered integers.

### `Layer<V>`

A batch of taus that arrived together: a sorted, non-overlapping `Arc<[Tau<V>]>`.

```
Layer { id: u64, min_start: i64, max_end: i64, taus: Arc<[Tau<V>]> }
```

Layers are immutable once created. Cloning a layer is an atomic reference-count bump.

`min_start` and `max_end` are skip-check bounds. A point query for timestamp `t` can skip an entire layer with two comparisons (`t < min_start || t >= max_end`) before touching the data.

Within the slice, a binary search (`partition_point` on `tau.end <= t`) locates the candidate in O(log n).

### Lenses

A lens is a named temporal function. It is not a single type — the executor tracks the two kinds in separate maps on each `DbState`:

```
base_types: HashMap<name, Type>   # base lens — declared value type; data lives in the store
derived:    HashMap<name, Expr>   # derived lens — the TauQL expression AST
```

A **base** lens delegates to the store's layer stack for its declared `Type`. A **derived** lens stores the parsed `Expr` directly; there is no compilation step and no caching. At query time `eval_expr` walks the AST live, resolving identifier nodes to other lenses, so derivations chain: `DERIVE c AS a + b` re-evaluates `a` and `b` at the requested timestamp on every lookup.

Cycle detection runs at `DERIVE` time by walking the dependency graph (`would_cycle`).

---

## Storage

### Backends

**`InMemory`**: a `HashMap<name, Vec<Layer<V>>>` with no I/O. Used for tests and ephemeral workloads.

**`Disk`**: one compressed binary file per database (`<name>.dat`):

```
header
  magic   "TAUZ" (4 bytes)
  version u8         # 1; the only supported version
  flags   u8         # bit 0 = encrypted body
  crc32   u32 LE     # over magic+version+flags
body  zstd-compressed payload, AES-256-GCM-encrypted after compression when flagged
  payload
    schema_count u32 LE        # persisted DDL statements
    [ len u32 LE, utf8 bytes ] # CREATE LENS / DERIVE LENS / SET TTL / DROP LENS
    [ DiskEntry... ]           # layer_id, written_at_ms, lens name, taus — until EOF
```

On open, the header is integrity-checked, the body decompressed (and decrypted when flagged), the schema section read, then layer entries replayed into the in-memory layer stack with their original `written_at` timestamps — so `AT … AS OF` keeps working across a restart. The file is rewritten atomically (`.tmp` + rename) on each flush — a flush fires on every append and every schema change, so an acknowledged write is durable before the call returns. Because schema DDL is part of the file, `CREATE DATABASE <name>` re-opens an existing `<name>.dat` and replays its schema, so lenses and TTL policies survive a restart.

Encryption is AES-256-GCM with a random 12-byte nonce. The key is never stored; it must be supplied via `TAU_ENCRYPTION_KEY` at startup. The `FLAG_ENCRYPTED` bit prevents accidentally opening an encrypted file without a key.

### Write-Ahead Log

The WAL sits between the caller and the store. Every mutation writes to the WAL first, fsyncs, then writes to the store. On startup the WAL is replayed before the store is opened.

WAL entries are line-oriented text:

```
<crc32> <layer_id> <written_at_ms> <lens> <s:e:v ...>   # data entry
E:<base64>                                              # encrypted data entry
S:<crc32> <CREATE LENS ...>                             # schema DDL
SE:<base64>                                             # encrypted schema DDL
```

Schema entries carry the raw TauQL text of the DDL that defines a lens (`CREATE LENS`, `DERIVE LENS`, `SET TTL`, `UNSET TTL`, `DROP LENS`). On replay, these are re-parsed and executed with `in_replay = true`, which suppresses re-appending them to the WAL. The disk backend persists the same DDL set in its own file (see above).

The WAL is checkpointed after compaction: a fresh snapshot of in-memory state is written to a new file and swapped in, bounding disk usage.

### Compaction

Each base lens accumulates layers over time. A point query must walk layers newest-first until it finds a covering tau. With many layers this is linear in the layer count.

Auto-compaction fires when a lens exceeds a threshold (default: 8 layers, configurable via `--compact-threshold`). It runs a sweep-line algorithm over all layers:

1. Build a list of start/end events, one pair per tau across all layers.
2. Sort events by timestamp; ends before starts at ties.
3. Walk events. A max-heap keyed by `(layer_idx, tau_idx)` tracks which layers are active at each point. The layer with the highest index (newest) wins.
4. Emit a merged segment whenever the winning value changes.

This is O(E log E) where E is the total number of taus. After compaction, the lens has exactly one layer.

`Store::append` returns a `bool` indicating whether compaction fired. The `Database` layer uses this to decide whether to WAL-checkpoint.

---

## Database and Executor

### `Database<V>`

`Database<V>` owns a `Store<V>` behind `RwLock` and an optional `Wal` behind `Mutex`. Append order is always:

```
WAL.write(entry) → WAL.fsync() → Store.append(layer)
```

A WAL fsync failure leaves the in-memory store unchanged; the entry is not committed. No partial-write window is visible to readers.

### `Executor`

`Executor` is the top-level query processor. It owns a `HashMap<String, DbState>` of named databases, an active-database pointer, and a `UserStore`.

Each `DbState` carries:
- A `Database<Value>` (live store + WAL)
- A `HashMap<name, Type>` for base lens declarations
- A `HashMap<name, Expr>` for derived lens ASTs
- A monotonic `next_layer_id` counter

**Two entry-point pairs:**

`exec` / `exec_read`: unrestricted. Used by library consumers, tests, and schema replay (`in_replay = true` prevents DDL from being re-appended).

`exec_as(stmt, caller)` / `exec_read_as(stmt, caller)`: looks up `caller` in `self.users`, calls `check_permission`, then delegates. Used by the TCP server for every authenticated session. `SHOW DATABASES` is post-filtered to only databases the caller holds any grant on.

The split is intentional: embedding Tau as a library bypasses auth entirely. Auth is a server concern, not an engine concern.

---

## Query Language

TauQL is a line-oriented command language: one statement in, one response line out. The grammar is minimal: no implicit join, no subquery. Multi-statement atomicity is provided by `START TRANSACTION / COMMIT / ROLLBACK`.

The parser is a `nom` combinator in `libtau::ql::parser`. Adding a new statement requires changes to four files: `ast.rs` (new variant + `Display`), `parser.rs` (production + `alt` entry), `executor.rs` (handler + `check_permission` arm), and `libtau::wire` (`Response::from_output` and `Response::parse`).

Operator precedence from low to high: `||`, `&&`, comparison, additive, multiplicative, unary, primary.

---

## Server

### Protocol

Line-oriented text: one TauQL statement per line in, one response line out.

### Concurrency

Each accepted connection runs on its own OS thread. The executor holds one `Arc<RwLock<Executor>>` for the database registry and a separate `Arc<RwLock<DbState>>` per named database.

Three lock routing tiers in `handle_query`:
- Read-only statements: shared executor lock + per-database read lock. All readers run concurrently.
- Data writes (`APPEND`, `CREATE LENS`, `COPY`, etc.): shared executor lock + per-database write lock. A write to `prod` does not block reads on `metrics`.
- Registry writes (`CREATE DATABASE`, `DROP DATABASE`, user management, transactions): exclusive executor lock for their brief duration.

The `--no-fsync-each` flag removes per-record WAL fsync from the write path entirely; a 50 ms background thread takes over durability, dramatically cutting write lock hold-time for WAL-enabled deployments.

### Connection capacity

`--max-connections N` (default 1024) caps concurrent client threads. Connections beyond the cap receive `ERR server at connection limit`. The accept loop tracks in-flight work with a single `AtomicUsize`.

`--idle-timeout-secs SECS` (default 300) installs a per-socket read/write timeout.

---

## Design Decisions

### Immutable layers over in-place mutation

Mutation would require finding and splitting or replacing existing taus. With immutable layers, a correction is an append. The WAL writes one new entry. The in-memory state gains one new layer. The old data is untouched.

The tradeoff is query cost: O(log n) per layer rather than O(log n) total. Compaction restores O(log n) by collapsing layers.

### Transaction semantics

`START TRANSACTION / COMMIT / ROLLBACK` provide per-connection multi-statement atomicity across one or more databases. Mutations after `START TRANSACTION` are buffered in memory; `COMMIT` applies the buffer under the exclusive executor write lock so no other reader sees partial state. `ROLLBACK` discards the buffer.

Each buffered statement carries the name of the database that was active when it was issued. `USE DATABASE` always executes immediately (it changes the active context for subsequent statements), so a transaction can span multiple databases: writes to `db1` and `db2` within one `START TRANSACTION / COMMIT` block are applied atomically.

Nesting is not supported. Concurrent readers on other connections see only pre-transaction state throughout. This fits the append-only workload well without the complexity of MVCC or 2PC.

For single-layer atomicity without explicit transactions, batching works equally well: `APPEND LENS x 0 10 1, 10 20 2` is one layer and one WAL entry.

### `exec` vs `exec_as` split

Auth is a transport concern. A Tau binary embedded in another process, reading sensor data directly, has no need for network authentication. Keeping the auth check out of `exec` means embedded use never pays the overhead or requires a dummy user.

### Arc-backed layers

Layer data is immutable once created. Sharing it across the read path without copying is safe. A compacted layer replaces the old stack atomically; concurrent readers holding references to old layers read consistent data until they finish.

### WAL-first ordering

Writing to the WAL before writing to the store means a crash between the two leaves an entry that replays on next startup, completing the write. The only risk is a duplicate replay, which the idempotent `append` semantics handle: replaying a layer that already exists adds it again, but the query result is identical (newest-layer-wins picks the same value either way).

### Thread-per-connection instead of async

The server uses `std::thread` rather than Tokio or async-std. For a database server where each connection is long-lived and query processing is CPU-bound (compaction, expression evaluation) rather than I/O-bound, synchronous threads are simpler to reason about. Async would complicate the `RwLock` usage without a clear throughput gain for the expected connection counts.
