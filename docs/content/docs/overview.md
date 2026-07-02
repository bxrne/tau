+++
title = "Overview"
date = 2026-05-28
template = "page.html"
+++

Tau is a time-series database for workloads where data changes over time — not just grows. Sensor readings get corrected. Financial prices get restated. Audit records get amended. Tau handles this with a data model grounded in algebraic structure: half-open intervals that tile precisely, layers that form a total order, and a normalisation algorithm (compaction) that provably preserves all query results.

---

## The data model

### Half-open intervals

Every value in Tau is true over a **half-open orthotope** — one `[lo, hi)` interval per axis:

```
Tau { coords: Arc<[Bound]>, value: V }   # Bound { lo: i64, hi: i64 }
```

The common lens has a single axis, **valid time** (`coords[0]`), so a tau is just `value V holds over [start, end)`. A multi-dimensional lens (`CREATE LENS … AXES (…)`, see below) adds filter axes; a tau then holds over the box formed by all of them. Each interval is **half-open**: it includes `lo` and excludes `hi`. This is a deliberate algebraic choice — half-open intervals form a monoid under concatenation:

```
[a, b)  +  [b, c)  =  [a, c)
```

Adjacent intervals share exactly one boundary point. Concatenation is associative. There are no gaps, no overlaps, no ambiguity about which interval owns a boundary timestamp. `lo < hi` is enforced at construction on every axis; zero-width intervals do not exist.

Timestamps are opaque `i64` integers. Tau places no assumption on epoch or unit — use seconds, milliseconds, nanoseconds, or any ordered integer your application agrees on.

Orthogonal to these queryable axes is **transaction time**: every layer records the wall-clock `written_at` when it was appended, which powers time-travel (`AT … AS OF`) and provenance (`HISTORY`).

### Layers

A `Layer` is an immutable batch of taus, sorted by valid-time start:

```
Layer { id: u64, taus: Arc<[Tau<V>]>, min_start: i64, max_end: i64, written_at: i64 }
```

Layers are the unit of append. Every call to `APPEND LENS` creates one new layer stamped with the current `written_at` (transaction time). The `Arc`-backed slice means cloning a layer is an atomic reference-count bump — no data is copied. Single-axis layers are non-overlapping on valid time; multi-axis layers may overlap on valid time as long as their boxes are disjoint somewhere.

`min_start` and `max_end` are valid-axis skip-check bounds. A point query for timestamp `t` can skip an entire layer with two comparisons before touching the underlying data.

### Lenses

A `Lens` is a named temporal function. It is either:

- **`Base`**: backed by a stack of layers in the store.
- **`Derived(f)`**: a lazy closure compiled from a TauQL expression at `DERIVE` time.

Derived lenses compose. `DERIVE LENS smooth AS avg(cpu, -600, 0)` compiles into a closure that calls `cpu`'s closure for every evaluation. The composition graph is a DAG — cycle detection runs at `DERIVE` time, not at query time.

A base lens may also be **multi-dimensional**: `CREATE LENS grid float AXES (valid, region)` declares extra filter axes (axis 0 is always valid time). Its taus are boxes appended as `APPEND LENS grid [0 100] [0 50] 1`, queried with one coordinate per axis (`AT LENS grid 10 25`), and swept along valid time with the other axes fixed (`RANGE LENS grid 0 100 AT (25)`). See the [TauQL reference](/docs/tauql/).

---

## Newest-layer-wins

Every layer has a monotonically increasing ID assigned at append time. When multiple layers cover the same point, the **newest** one wins. This is a deterministic rule with no configuration — not a per-lens policy, not a conflict resolution strategy. Just: newer layer wins.

This makes concurrent appends trivially composable: two writers both succeed; query results reflect both writes, with the later one taking precedence at any overlap.

Because each layer also carries its `written_at` transaction time, you can wind the clock back: `AT LENS x t AS OF <ts>` resolves newest-wins **restricted to layers written at or before `ts`**, and `HISTORY LENS x` lists the surviving write-times. Corrections never destroy the belief that was current before them.

---

## Compaction

Layers accumulate. A point query walks layers newest-first until it finds a covering interval — with many layers, that walk is linear in the layer count. Compaction normalises them while preserving every query result.

Compaction works **within each transaction-time generation** — a run of layers that share a `written_at` — and never merges across generations, so distinct write-times survive and `AT … AS OF` / `HISTORY` stay exact. Within a generation:

- **single-axis** lenses use a **sweep-line**: collect all interval boundaries, walk them tracking which layer is newest-active, and emit a merged segment whenever the winning value changes.
- **multi-axis** lenses use **orthotope subtraction**: each older tau's box has every strictly-newer box subtracted from it (into point-disjoint fragments), then coplanar equal-value fragments are merged.

Both are provable normalisations: **every `AT`, `RANGE`, `REDUCE`, `AS OF`, and `HISTORY` result is identical before and after compaction** — the central invariant of the storage model, checked by property tests and the deterministic simulation tester on every build. (A burst of same-instant appends shares one generation and collapses to a single layer; corrections made at distinct times remain distinct generations, which is exactly what preserves time-travel.)

Compaction fires automatically when a lens accumulates more layers than the configured threshold (default: 8). It is not a background job — it runs inline after the append that crosses the threshold. For disk-backed databases, compaction does not by itself force a full rewrite of the on-disk file: that checkpoint is throttled to every 8 compactions (or sooner if the WAL size cap is reached), keeping the per-append cost of the disk backend close to the in-memory backend. See [How it works](/docs/how-it-works/#write-ahead-log).

---

## Storage backends

**In-memory:** a `HashMap<lens, Arc<[Layer]>>` with no I/O. Reads snapshot the per-lens stack with a pointer bump; appends rebuild it copy-on-write (RCU) so in-flight readers keep a consistent view. State is lost on process exit. Used for tests, ephemeral workloads, and embedded use.

**Disk:** one zstd-compressed binary file per database (`TAUZ` magic + a version byte; layer data **and** schema DDL), rewritten atomically on checkpoint. On open, entries are replayed into the in-memory layer stack with their original `written_at`, so `AT … AS OF` keeps working across a restart. The current format is version 2 (a per-tau axis count for multi-dimensional lenses); version 1 files remain readable.

Encryption is AES-256-GCM with a random 12-byte nonce, keyed by `TAU_ENCRYPTION_KEY` (compress-then-encrypt). A flag in the header prevents accidentally opening an encrypted file without the key.

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
