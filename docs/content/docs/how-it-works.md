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

Tau is structured as a library (`libtau`) consumed by two binaries: the TCP server (`tau`) and the interactive client (`tauctl`, a ratatui TUI that requires an interactive terminal). The library is a **syscall-routing microkernel**: a `Kernel` owns four built-in services — db (mutations), query (reads), auth (users and grants), and metrics — and routes every statement to the service that owns it, applying per-user policy on the way. TLS and network concerns live exclusively in the server.

```
Stmt → Kernel ─┬→ query service (reads) ──┐
               ├→ db service (mutations) ──┴→ shared Registry → Database<Value> → Store<V> + optional Wal
               └→ auth service (user management)
```

---

## Primitives

### `Tau<V>`

An atomic temporal fact: value `V` is true over one half-open `[lo, hi)` interval per axis.

```
Tau { coords: Arc<[Bound]>, value: V }   # Bound { lo: i64, hi: i64 }
```

Axis 0 is always valid time; `Tau::new(start, end, v)` builds the common single-axis form, and a multi-axis lens (`CREATE LENS … AXES (…)`) adds filter axes so a tau is an N-orthotope. The half-open interval is intentional. Adjacent intervals tile cleanly: `[0, 10)` and `[10, 20)` cover `[0, 20)` with no overlap and no gap; equality on the boundary belongs unambiguously to the later interval. `Tau::new` asserts `start < end` on every axis; there are no zero-width taus.

Timestamps are `i64` (nanoseconds, milliseconds, or any other unit the caller agrees on). Tau treats them as opaque ordered integers.

### `Layer<V>`

A batch of taus that arrived together, sorted by valid-time start:

```
Layer { id: u64, min_start: i64, max_end: i64, taus: Arc<[Tau<V>]>, written_at: i64 }
```

Layers are immutable once created. Cloning a layer is an atomic reference-count bump. `written_at` is the transaction timestamp stamped at append time (and restored on replay) — the axis `AT … AS OF` filters on.

`min_start` and `max_end` are valid-axis skip-check bounds. A point query for timestamp `t` can skip an entire layer with two comparisons (`t < min_start || t >= max_end`) before touching the data. Within a single-axis slice a binary search locates the candidate in O(log n); multi-axis point lookup fast-skips on the valid axis then checks the remaining axes.

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

**`InMemory`**: a `HashMap<name, Arc<[Layer<V>]>>` with no I/O. Reads snapshot the per-lens stack with a pointer bump; appends rebuild it copy-on-write (RCU). Used for tests and ephemeral workloads.

**`Sstable`** (the `disk` backend): a memtable (the same RCU `Arc<[Layer]>` shape as `InMemory`) absorbs appends; on checkpoint it flushes into a new **immutable run file** instead of rewriting anything that already exists on disk, and a small atomically-rewritten **manifest** (`<name>.manifest`) tracks the live run ids:

```
run file (<name>.run.<id>)
header
  magic   "TAUR" (4 bytes)
  version u8
  flags   u8         # bit 0 = encrypted body
  crc32   u32 LE     # over magic+version+flags
body  zstd-compressed, AES-256-GCM-encrypted after compression when flagged
  entry_count u32 LE
  entries, sorted by (lens, coords[0].lo, coords[1..], written_at desc):
    lens (len + utf8), layer_id u64 LE, written_at i64 LE, epoch u64 LE,
    arity u8, arity × (lo i64 LE, hi i64 LE), value (encoded)
footer  uncompressed (so a skip-check never decompresses the body), encrypted when flagged
  per lens: name, min_start i64 LE, max_end i64 LE, count u32 LE,
            a range-bucketed bloom filter over covered points
trailer footer_len u32 LE (last 4 bytes — lets a reader seek from EOF to find the footer)
```

A run is skipped **without decoding its body** when the footer proves the queried lens is
absent, out of range, or (for a point query) the bloom filter rules the point out; a decoded
run body is cached in memory, since a run is immutable once written. Reads merge the memtable
with the runs and resolve newest-wins / `AS OF` **at read time** (stab the covering versions,
`argmax written_at`, optionally `<= as_of`) instead of at write time.

Compaction has exactly one trigger: a per-lens `layer_count` — the total across the memtable
*and* every run, persisted in the manifest so it survives a restart — crossing
`compact_threshold`, at which point every not-yet-absorbed run plus the memtable is merged into
the canonical per-generation result (bumping the lens's epoch so the now-redundant run entries
are ignored on read). This mirrors `InMemory`'s own threshold condition exactly. A separate,
purely space-driven pass merges run *files* (not lens data) once their count grows too large;
correctness never depends on that pass running.

`DROP LENS` bumps a per-lens epoch (persisted in the manifest) so pre-drop run data is shadowed
without touching old run files. Durability for individual appends comes from the per-database
WAL described below; the manifest and run files are the checkpoint-time durability mechanism.

Encryption is AES-256-GCM with a random 12-byte nonce, applied separately to the run body and
footer. The key is never stored; it must be supplied via `TAU_ENCRYPTION_KEY` at startup. The
encrypted-body flag prevents accidentally opening an encrypted file without a key.

### Write-Ahead Log

The WAL sits between the caller and the store. Every mutation writes to the WAL first, fsyncs, then writes to the store. On startup the WAL is replayed before the store is opened.

WAL entries are line-oriented text:

```
<crc32> <layer_id> <written_at_ms> <lens> <s:e:v ...>   # data entry (1-axis tau)
                                          <N<k>:lo:hi:…:v>  # k-axis tau
E:<base64>                                              # encrypted data entry
S:<crc32> <CREATE LENS ...>                             # schema DDL
SE:<base64>                                             # encrypted schema DDL
```

Schema entries carry the raw TauQL text of the DDL that defines a lens (`CREATE LENS`, `DERIVE LENS`, `SET TTL`, `UNSET TTL`, `DROP LENS`). On replay, these are re-parsed and executed with `in_replay = true`, which suppresses re-appending them to the WAL.

The WAL is checkpointed when `[wal] max_size_mb` is reached, or every `CHECKPOINT_COMPACTION_INTERVAL` (8) compactions, whichever comes first: for the in-memory backend, a fresh snapshot of in-memory state is written to a new WAL file and swapped in, bounding disk usage. For the disk backend, the checkpoint instead flushes the `Sstable` memtable into a new run file and truncates the WAL to just its schema lines — the manifest/run files and WAL together always hold exactly the live state, with the WAL covering everything appended since the last flush. Compactions between checkpoints still shrink each lens to one canonical set of layers per transaction-time generation (in memory, in the runs, and in the WAL's logical replay); they just don't each force a run rewrite — only `consolidate_lens`'s cross-fragment merge does that, and only when a lens's total layer count crosses `compact_threshold`.

### Disk backend + WAL

The `disk` backend pairs every database's `Sstable` store (`<name>.manifest` + `<name>.run.<id>` files) with a `<name>.wal` file in the same directory. `APPEND` writes go to the WAL first (fsynced by default) and only update the in-memory memtable; the memtable is flushed into a new run file only on checkpoint, as above — never a whole-database rewrite. On startup, the existing manifest is opened and then `<name>.wal` is replayed on top, recovering any appends made since the last checkpoint. The `[wal]` config's `no_fsync_each` and `max_size_mb` settings apply to these per-database WAL files.

### Compaction

Each base lens accumulates layers over time. A point query must walk layers newest-first until it finds a covering tau. With many layers this is linear in the layer count.

Auto-compaction fires when a lens exceeds a threshold (default: 8 layers, configurable via `--compact-threshold`). It compacts **within each transaction-time generation** — a maximal run of layers sharing a `written_at` stamp — and never across generations, so distinct write timestamps survive. Within a generation it runs a sweep-line algorithm:

1. Build a list of start/end events, one pair per tau across the generation's layers.
2. Sort events by timestamp; ends before starts at ties.
3. Walk events. A max-heap keyed by `(layer_idx, tau_idx)` tracks which layers are active at each point. The layer with the highest index (newest) wins.
4. Emit a merged segment whenever the winning value changes.

This is O(E log E) where E is the total number of taus. After compaction a lens holds one layer per surviving generation. Preserving generations is what keeps `AT … AS OF <t>` and `HISTORY` exact after compaction: the earlier collapse-to-one-layer form stamped the merged layer with `max(written_at)` and silently erased older beliefs. When all appends share a generation (the common burst-of-writes case) compaction still collapses to a single layer.

Multi-axis lenses (`CREATE LENS … AXES (…)`) compact losslessly too, but the valid-time sweep does not apply — it would ignore the filter axes. Within each generation the engine instead resolves newest-wins by **orthotope subtraction**: each older tau's box has every strictly-newer box subtracted from it (the standard slab decomposition, yielding point-disjoint fragments), then coplanar adjacent fragments of equal value are merged. The result covers the same N-space region with the same value at every point, so `AT` / `AS OF` / `RANGE` / `HISTORY` are unchanged — collapsing a generation's layers into one while dropping fully-occluded regions.

`Store::append` returns a `bool` indicating whether compaction fired. The `Database` layer counts these and triggers a WAL-checkpoint every `CHECKPOINT_COMPACTION_INTERVAL` (8) compactions (or sooner, if `[wal] max_size_mb` is reached first) rather than on every single one, which keeps the per-append cost of the disk backend close to the in-memory backend.

---

## Database and Kernel

### `Database<V>`

`Database<V>` owns a `Store<V>` behind `RwLock` and an optional `Wal` behind `Mutex`. Append order is always:

```
WAL.write(entry) → WAL.fsync() → Store.append(layer)
```

A WAL fsync failure leaves the in-memory store unchanged; the entry is not committed. No partial-write window is visible to readers.

### `Kernel`

`Kernel` is the top-level statement processor. It owns four services and a shared `Registry` of named databases (plus an active-database pointer). The **db service** executes mutations (lens DDL, appends, transactions, backup/restore); the **query service** evaluates reads over the same registry with read locks only; the **auth service** owns users and grants; **metrics** counts everything. Each kernel also owns a virtual `Clock` (transaction stamps, TTL "now") and a `FaultInjector` — both per-kernel capabilities, pinned by deterministic simulation.

Each `DbState` in the registry carries a `Database<Value>` (live store + WAL), the base-lens type declarations, derived-lens ASTs, a monotonic `next_layer_id`, and the kernel's clock.

**Two entry-point pairs**, all `&self`:

`exec` / `exec_read`: unrestricted. Used by library consumers, tests, and DST.

`exec_as(stmt, caller)` / `exec_read_as(stmt, caller)`: resolves `caller` in the auth service, applies the kernel's permission policy, then routes. Used by the TCP server for every authenticated session. `SHOW DATABASES` is post-filtered to only databases the caller holds any grant on.

The split is intentional: embedding Tau as a library bypasses auth entirely. Policy lives in the kernel, not in any service — no service ever sees a statement the caller wasn't allowed to run.

---

## Query Language

TauQL is a line-oriented command language: one statement in, one response line out. The grammar is minimal: no implicit join, no subquery. Multi-statement atomicity is provided by `START TRANSACTION / COMMIT / ROLLBACK`.

The parser is a `nom` combinator in `libtau::ql::parser`, deliberately outside the kernel. Adding a new statement touches the AST (`Stmt` variant + `Display` + `is_read_only`), the parser, the owning service's handler, the kernel's permission policy, and the wire codec.

Operator precedence from low to high: `||`, `&&`, comparison, additive, multiplicative, unary, primary.

---

## Server

### Protocol

Line-oriented text: one TauQL statement per line in, one response line out.

### Concurrency

Each accepted connection runs on its own OS thread. All threads share one plain `Arc<Kernel>`; the kernel routes each statement to its owning service and locks internally — the server has no lock router of its own.

Three locking tiers inside the kernel:
- Read-only statements: registry read lock + per-database read lock (query service). All readers run concurrently.
- Data writes (`APPEND`, `CREATE LENS`, `COPY`, etc.): registry read lock + per-database write lock (db service). A write to `prod` does not block reads on `metrics`.
- Database DDL (`CREATE DATABASE`, `DROP DATABASE`, `USE DATABASE`, `RESTORE`): registry write lock for its brief duration. User management routes to the auth service.

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

`START TRANSACTION / COMMIT / ROLLBACK` provide multi-statement atomicity across one or more databases. Mutations after `START TRANSACTION` are buffered in the db service; `COMMIT` replays each buffered statement against the database that was active when it was buffered. `ROLLBACK` discards the buffer.

`USE DATABASE` always executes immediately (it changes the active context for subsequent statements), so a transaction can span multiple databases within one `START TRANSACTION / COMMIT` block.

Nesting is not supported. Concurrent readers on other connections see only pre-transaction state throughout. This fits the append-only workload well without the complexity of MVCC or 2PC.

For single-layer atomicity without explicit transactions, batching works equally well: `APPEND LENS x 0 10 1, 10 20 2` is one layer and one WAL entry.

### `exec` vs `exec_as` split

Auth is a transport concern. A Tau binary embedded in another process, reading sensor data directly, has no need for network authentication. Keeping the auth check out of `exec` means embedded use never pays the overhead or requires a dummy user. When auth *is* wanted, the kernel enforces it before routing — services never check permissions themselves.

### Arc-backed layers

Layer data is immutable once created. Each lens's stack is held as an `Arc<[Layer]>`, so a read snapshots it with a single pointer bump — no vector clone on the range/reduce path. Appends are copy-on-write (RCU): the writer copies the stack, mutates the copy, then swaps the `Arc` in under the write lock, so a reader holding an earlier snapshot keeps a consistent view until it finishes. Layer clones within the copy are themselves pointer bumps because tau slices are `Arc`-backed.

### WAL-first ordering

Writing to the WAL before writing to the store means a crash between the two leaves an entry that replays on next startup, completing the write. The only risk is a duplicate replay, which the idempotent `append` semantics handle: replaying a layer that already exists adds it again, but the query result is identical (newest-layer-wins picks the same value either way).

### Thread-per-connection instead of async

The server uses `std::thread` rather than Tokio or async-std. For a database server where each connection is long-lived and query processing is CPU-bound (compaction, expression evaluation) rather than I/O-bound, synchronous threads are simpler to reason about. Async would complicate the `RwLock` usage without a clear throughput gain for the expected connection counts.
