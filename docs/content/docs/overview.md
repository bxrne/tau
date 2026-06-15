+++
title = "Overview"
date = 2026-05-28
template = "page.html"
+++

Tau is a time-series database for workloads where data changes over time — not just grows. Sensor readings get corrected. Financial prices get restated. Audit records get amended. Tau handles this with a data model grounded in algebraic structure: half-open intervals that tile precisely, layers that form a total order, and a normalisation algorithm (compaction) that provably preserves all query results.

---

## The data model

### Half-open intervals

Every value in Tau has a time range over which it was true:

```
Tau { start: i64, end: i64, value: V }   # value V holds over [start, end)
```

The interval is **half-open**: it includes `start` and excludes `end`. This is a deliberate algebraic choice. Half-open intervals form a monoid under concatenation:

```
[a, b)  +  [b, c)  =  [a, c)
```

Adjacent intervals share exactly one boundary point. Concatenation is associative. There are no gaps, no overlaps, no ambiguity about which interval owns a boundary timestamp. `start < end` is enforced at construction; zero-width intervals do not exist.

Timestamps are opaque `i64` integers. Tau places no assumption on epoch or unit — use seconds, milliseconds, nanoseconds, or any ordered integer your application agrees on.

### Layers

A `Layer` is an immutable, sorted, non-overlapping slice of intervals:

```
Layer { id: u64, taus: Arc<[Tau<V>]>, min_start: i64, max_end: i64 }
```

Layers are the unit of append. Every call to `APPEND LENS` creates one new layer. The `Arc`-backed slice means cloning a layer is an atomic reference-count bump — no data is copied.

`min_start` and `max_end` are skip-check bounds. A point query for timestamp `t` can skip an entire layer with two comparisons before touching the underlying data.

### Lenses

A `Lens` is a named temporal function. It is either:

- **`Base`**: backed by a stack of layers in the store.
- **`Derived(f)`**: a lazy closure compiled from a TauQL expression at `DERIVE` time.

Derived lenses compose. `DERIVE LENS smooth AS avg(cpu, -600, 0)` compiles into a closure that calls `cpu`'s closure for every evaluation. The composition graph is a DAG — cycle detection runs at `DERIVE` time, not at query time.

---

## Newest-layer-wins

Every layer has a monotonically increasing ID assigned at append time. When multiple layers cover the same timestamp, the one with the **highest ID** wins. This is a deterministic rule with no configuration — not a per-lens policy, not a conflict resolution strategy, not a timestamp comparison. Just: newer layer ID wins.

This makes concurrent appends trivially composable: two writers both succeed; query results reflect both writes, with the later one taking precedence at any overlap.

---

## Compaction

Layers accumulate. A point query walks layers newest-first until it finds a covering interval — with many layers, that walk is linear in the layer count. Compaction restores constant-per-layer cost.

The algorithm is a **sweep-line normalisation**:

1. Collect all interval start/end events across all layers in the lens.
2. Sort by timestamp; ends before starts at ties.
3. Walk events, tracking a max-heap keyed by layer ID. The layer with the highest ID active at each point is the winner.
4. Emit a merged segment whenever the winning value changes.

The result is one canonical layer. **Every `AT`, `RANGE`, and `REDUCE` result is identical before and after compaction.** This equivalence is the central invariant of the storage model, and it is checked by the property-based test suite against randomised layer sequences on every build.

Compaction fires automatically when a lens accumulates more layers than the configured threshold (default: 8). It is not a background job — it runs inline after the append that crosses the threshold. For disk-backed databases, compaction does not by itself force a full rewrite of the on-disk file: that checkpoint is throttled to every 8 compactions (or sooner if the WAL size cap is reached), keeping the per-append cost of the disk backend close to the in-memory backend. See [How it works](/docs/how-it-works/#write-ahead-log) and [Benchmarks](/docs/benchmarks/).

---

## Storage backends

**In-memory:** a `HashMap<lens, Vec<Layer>>` with no I/O. State is lost on process exit. Used for tests, ephemeral workloads, and embedded use.

**Disk:** a binary append-only file with a `TAU` (plaintext) or `TAUE` (encrypted) magic header. On open, entries are replayed into the in-memory layer stack. The file handle stays open in append mode.

Encryption is AES-256-GCM with a random 12-byte nonce per entry, keyed by `TAU_ENCRYPTION_KEY`. The `TAUE` magic prevents accidentally opening an encrypted file without the key.

**Write-ahead log (WAL):** sits between the caller and the store. Every mutation writes to the WAL, fsyncs, then writes to the store. A crash between the two leaves an entry that replays on the next startup, completing the write.

---

## Concurrency

Each TCP connection runs on its own OS thread. All threads share one `Arc<RwLock<Executor>>` which guards the database registry, plus a separate `Arc<RwLock<DbState>>` per named database.

Lock routing:
- Read-only statements (`AT`, `RANGE`, `REDUCE`, `SHOW *`) take the shared executor lock and a per-database read lock, so concurrent readers never block each other.
- Data writes (`APPEND`, `CREATE LENS`, `COPY`, etc.) take only the shared executor lock plus a per-database write lock. A write to database `prod` does not block reads on database `metrics`.
- Registry writes (`CREATE DATABASE`, `DROP DATABASE`, user management, transactions) take the exclusive executor lock for their brief duration.

When a connection uses `START TRANSACTION … COMMIT`, mutations are buffered per-connection and not written to storage until `COMMIT`. During a transaction, writes route through the exclusive executor lock so the buffer is applied atomically. `ROLLBACK` discards the buffer. Nesting is not supported.

---

*For design rationale see [How it works](/docs/how-it-works/). For the query language see [TauQL Reference](/docs/tauql/).*
