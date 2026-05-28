+++
title = "Overview"
date = 2026-05-28
template = "page.html"
+++

# Overview

Tau is a time-series database built on **immutable, layered temporal intervals**. It is not a relational store: no rows, no tables, no indexes. There are *lenses* (named temporal functions), a query language designed around temporal semantics, and a storage model that makes correction cheap.

---

## The core idea

Most databases assume a row represents current truth. To record history you add timestamps. Updates replace old values.

Tau starts from the opposite assumption: **every fact has a time range over which it was true**. A measurement saying "temperature was 22 °C from noon to 1 pm" is a first-class value. Correcting it means appending a new interval on top: a newer layer covering the same range with a newer value. The old data is never touched.

This model is correct by default for workloads where:

- Values arrive out of order (sensor streams with delivery lag)
- Data is periodically restated (financial time series, billing)
- The history of corrections is itself valuable (audit logs)

---

## Three primitives

### `Tau<V>`

```
Tau { start: i64, end: i64, value: V }
```

A value `V` is true over the **half-open interval `[start, end)`**. The half-open boundary is intentional: adjacent intervals tile cleanly with no overlap and no gap. `start < end` is enforced at construction time.

Timestamps are opaque `i64` integers. Tau places no assumption on epoch or unit: seconds, milliseconds, nanoseconds, or any other unit your application agrees on.

### `Layer<V>`

```
Layer { id: u64, taus: Arc<[Tau<V>]>, min_start: i64, max_end: i64 }
```

A layer is an immutable, sorted, non-overlapping slice of taus. Cloning a layer is an atomic reference-count bump; the data is never copied.

`min_start` and `max_end` allow point queries to skip entire layers with two comparisons before touching the data.

### `Lens<V>`

A lens is either:
- **`Base`**: backed by a stack of layers in a store
- **`Derived(f)`**: a lazy closure compiled from a TauQL expression at `DERIVE` time

Derived lenses are evaluated on demand. They compose: `DERIVE smooth AS avg(cpu, -600, 0)` compiles into a closure that calls `cpu`'s closure for every evaluation. Cycle detection runs at `DERIVE` time.

---

## Newest-layer-wins

When multiple layers cover the same timestamp, the one with the highest layer ID (newest append) wins. There is no conflict resolution vocabulary: the rule is always the newest layer.

This makes concurrent appends trivially correct: both succeed, and the query result reflects both, with the newer one taking precedence in any overlap.

---

## Storage

### InMemory

A `HashMap<lens, Vec<Layer>>`. No I/O. State is lost on drop. Used for tests, ephemeral workloads, and embedded use.

### Disk

A binary append-only file with a `TAU` (plain) or `TAUE` (encrypted) magic header. On open, all entries are replayed into the in-memory layer stack. The file handle is kept open in append mode for new writes.

Encryption is AES-256-GCM with a random 12-byte nonce per entry, keyed by `TAU_ENCRYPTION_KEY` (a 64-hex-char string). Without the key, an encrypted file is unreadable.

### Write-Ahead Log

The WAL sits between the caller and the store. Every mutation is written to the WAL, fsynced, then written to the store. A crash between WAL and store leaves an entry that replays on the next startup. No partial write is ever visible to readers.

WAL entries are line-oriented text with CRC32 checksums. Schema DDL (`CREATE LENS`, `DERIVE LENS`) is stored as schema entries and replayed separately at startup.

---

## Compaction

Layers accumulate over time. A point query walks layers newest-first, so many layers means slower queries. Auto-compaction fires when a lens exceeds a threshold (default: 4 layers).

The compaction algorithm is a **sweep-line merge** over all layers:

1. Build a list of interval start/end events across all taus in all layers.
2. Sort events by timestamp; ends before starts at ties.
3. Walk events, maintaining a max-heap keyed by layer ID to track which layer is active at each point.
4. Emit a merged segment whenever the winning value changes.

This is O(E log E) where E is the total number of taus. After compaction, the lens has exactly one layer. Query results are identical before and after compaction.

---

## The executor

```
Stmt -> Executor -> Database<Value> -> Store<V> + optional Wal
```

The `Executor` owns a map of named databases, an active-database pointer, and a `UserStore`. Each database carries its own store, WAL, and lens definitions.

Two entry-point pairs with different semantics:

- `exec` / `exec_read`: unrestricted, no permission check. Used by library consumers, tests, and WAL replay.
- `exec_as(stmt, user)` / `exec_read_as(stmt, user)`: looks up the user, calls `check_permission`, then delegates. The TCP server uses these for every authenticated session.

Auth is a server concern. Embedding Tau as a library bypasses permission checks entirely.

---

## Server concurrency

Each TCP connection runs on its own OS thread. All threads share one `Arc<RwLock<Executor>>`. Read-only statements (`AT`, `RANGE`, `REDUCE`, `SHOW *`) take the read lock and run concurrently. Write statements take the exclusive write lock.

---

*For deeper coverage of design decisions see [Architecture](/docs/architecture/). For the query language see [TauQL Reference](/docs/tauql/).*
