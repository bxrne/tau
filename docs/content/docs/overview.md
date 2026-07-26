+++
title = "Overview"
date = 2026-05-28
template = "page.html"
+++

Tau is a bitemporal time-series database for workloads where data changes over time — not just grows. Financial prices get restated. Corporate actions amend history. Risk signals need rolling computation over corrected data. Tau handles this with a data model grounded in algebraic structure: half-open intervals that tile precisely, layers that form a total order, and a normalisation algorithm (compaction) that provably preserves all query results. **Lua triggers** let you compute Sharpe ratios, rolling stats, and risk metrics on write or on a schedule, sandboxed inside the kernel.

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
- **`Materialised`**: an `XDERIVE` lens — the result is computed and stored eagerly, auto-refreshing whenever a source lens is corrected.

Derived lenses compose. `DERIVE LENS spread AS aapl - msft` compiles into a closure that calls both lenses for every evaluation. The composition graph is a DAG — cycle detection runs at `DERIVE` time, not at query time. Stack as many transformations as you need over the same base data: a raw price lens feeds a returns lens feeds a rolling-mean lens feeds a signal lens — all re-evaluated against the latest corrections, all time-travel-aware.

A base lens may also be **multi-dimensional**: `CREATE LENS quote float AXES (time, instrument, venue)` declares extra filter axes (axis 0 is always valid time). Its taus are boxes appended as `APPEND LENS quote [0 3600] [1 2] [10 20] 100.0`, queried with one coordinate per axis (`AT LENS quote 1800 1 10`), and swept along valid time with the other axes fixed (`RANGE LENS quote 0 7200 AT (1, 10)`). See the [TauQL reference](/docs/tauql/).

---

## Newest-layer-wins

Every layer has a monotonically increasing ID assigned at append time. When multiple layers cover the same point, the **newest** one wins. This is a deterministic rule with no configuration — not a per-lens policy, not a conflict resolution strategy. Just: newer layer wins.

This makes concurrent appends trivially composable: two writers both succeed; query results reflect both writes, with the later one taking precedence at any overlap.

Because each layer also carries its `written_at` transaction time, you can wind the clock back: `AT LENS px t AS OF <ts>` resolves newest-wins **restricted to layers written at or before `ts`**, and `HISTORY LENS px` lists the surviving write-times. Corrections never destroy the belief that was current before them — the foundation of point-in-time-correct backtesting.

---

## Lua scripting

Tau embeds **LuaJIT** via `mlua`. You write functions in Lua, register them with `CREATE FUNCTION`, and they fire on triggers, on a schedule, or on demand. The host API is capability-gated: each function declares which `tau.*` calls it may use.

```sql
-- A rolling Sharpe ratio computed on every write to the returns lens.
CREATE FUNCTION sharpe ON WRITE LENS returns CAPS exec, range, clock
AS "
  local s, e = tau.clock_window(86400000)   -- last 24h in ms
  local n, sum, sum2 = 0, 0.0, 0.0
  for _, row in ipairs(tau.range('returns', s, e)) do
    local r = row.v; n = n + 1; sum = sum + r; sum2 = sum2 + r*r
  end
  if n == 0 then return end
  local mean = sum / n
  local sd = (sum2 / n - mean*mean) ^ 0.5
  local sh = (sd > 0) and (mean / sd) or 0.0
  tau.exec(('APPEND LENS sharpe 0 %d %f'):format(e, sh))
"
```

### Trigger kinds

| Kind | Fires when | Use case |
|------|-----------|----------|
| `ON WRITE [LENS x]` | After an `APPEND` to a matching lens | Recompute a derived signal, update a position, fire an alert |
| `SCHEDULE EVERY <secs>` | Periodically from the host loop | End-of-day rollups, TTL sweeps, scheduled risk refreshes |
| `ON PERMISSION` | Before a statement executes | Custom access control, compliance gates, trade limits |
| On-demand | `CALL FUNCTION name(args)` | Ad-hoc computation, parameterised queries |

### Host API (`tau.*`)

| Call | Capability | Description |
|------|-----------|-------------|
| `tau.exec(stmt)` | `exec` | Run a TauQL statement (mutation or read) |
| `tau.at(lens, t)` | `at` | Point lookup — read-only fast path |
| `tau.range(lens, s, e)` | `range` | Range scan — returns `{s, e, v}` triples |
| `tau.reduce(lens, s, e, func)` | `range` | Aggregate (`min`/`max`/`avg`/`sum`/`count`) |
| `tau.log(msg)` | `log` | Structured log into the kernel's tracing sink |
| `tau.metric(name, val)` | `metric` | Record a custom metric |
| `tau.clock()` | `clock` | Current virtual time (ms) |
| `tau.clock_window(ms)` | `clock` | `[now-ms, now)` — rolling window bounds |
| `tau.last_write_span()` | `clock` | `[lo, hi)` of the taus that fired an `ON WRITE` trigger |
| `tau.faults()` | `faults` | Armed fault-injection state (read-only) |

### Sandboxing

The Lua environment is **sandboxed**: `os`, `io`, `loadfile`, `dofile`, `require`, `debug`, and `package` are stripped from `_G`. The only I/O a function can do is through the `tau.*` host API, gated by declared capabilities. A function that tries `os.execute(...)` gets a clean error, not a shell.

### Persistence

`CREATE FUNCTION` and `DROP FUNCTION` persist in the schema WAL (like `DERIVE`/`XDERIVE`), so functions survive restarts. The Lua source is stored as text and recompiled on replay.

See [Lua Scripting](/docs/scripting/) for the full reference.

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

**Disk (`Sstable`):** a memtable (same `Arc<[Layer]>` shape as in-memory) absorbs appends; on checkpoint it flushes into a new immutable, zstd-compressed run file per database (`TAUR` magic) instead of rewriting anything already on disk, and a small atomically-rewritten manifest (`TAUM` magic) tracks the live runs. Reads merge the memtable with the runs and resolve newest-wins/`AS OF` at read time (MVCC), so `AT … AS OF` keeps working across a restart. Compaction has one trigger — a per-lens layer count spanning memtable and every run, persisted in the manifest — that merges a lens's data into one canonical set per transaction-time generation once it crosses the configured threshold.

Encryption is AES-256-GCM with a random 12-byte nonce, keyed by `TAU_ENCRYPTION_KEY` (compress-then-encrypt), applied separately to each run's body and footer. A flag in the header prevents accidentally opening an encrypted file without the key.

**Write-ahead log (WAL):** sits between the caller and the store. Every mutation writes to the WAL, fsyncs, then writes to the store. A crash between the two leaves an entry that replays on the next startup, completing the write.

---

## Concurrency

Each TCP connection runs on its own OS thread; all threads share one `Arc<Kernel>`. The kernel routes each statement to the service that owns it (query for reads, db for mutations, auth for user management) and locks internally, so readers never block each other and a write to one database does not block reads on another. Transactions buffer mutations until `COMMIT`; `ROLLBACK` discards them.

Locking tiers, transaction semantics, and design rationale live in [How it works](/docs/how-it-works/) — this page stays conceptual.

---

*For design rationale see [How it works](/docs/how-it-works/). For the query language see [TauQL Reference](/docs/tauql/). For Lua scripting see [Scripting](/docs/scripting/).*
