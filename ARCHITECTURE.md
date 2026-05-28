# Architecture

Tau is a time-series database for recording how values change over time. It is not a general-purpose relational store. It has no rows, no tables, and no indexes -- only temporal intervals, a query language designed around them, and a storage model that makes correction cheap.

This document describes why Tau is built the way it is, not just how it works. Decisions that might look odd from the outside have reasons. Knowing the reasons lets you contribute without fighting the grain of the design.

---

## The Problem

Most databases are built around the assumption that a row represents the current truth about an entity. To record history you add timestamps, but the model remains mutation-oriented: an update replaces the old value.

Tau starts from the opposite assumption. Every fact has a time range over which it was true. A measurement saying "temperature was 22 C from noon to 1 pm" is a first-class value, not a derived view. Updating it means appending a correction -- a new layer that covers some or all of the same time range with a newer value. The old layer is never touched.

This makes Tau correct by default for append-only workloads:

- Sensor streams where values arrive out of order
- Financial time series where prices are restated
- Audit trails where the history of corrections is itself interesting

The cost is that every query must resolve which layer wins at each point in time. That resolution logic is the sweep-line compaction algorithm and the layered query model.

---

## Primitives

Three types form everything else in Tau.

### `Tau<V>`

An atomic temporal fact: value `V` is true over the half-open interval `[start, end)`.

```
Tau { start: i64, end: i64, value: V }
```

The half-open interval is intentional. Adjacent intervals tile cleanly: `[0, 10)` and `[10, 20)` cover `[0, 20)` with no overlap and no gap. Equality on the boundary belongs unambiguously to the later interval.

`Tau::new` asserts `start < end`. There are no zero-width taus. An empty interval is not a fact -- it represents nothing.

Timestamps are `i64` nanoseconds, milliseconds, or any other unit the caller agrees on. Tau makes no assumption about the epoch; it treats timestamps as opaque integers ordered by value.

### `Layer<V>`

A batch of taus that arrived together: a sorted, non-overlapping `Arc<[Tau<V>]>`.

```
Layer { id: u64, min_start: i64, max_end: i64, taus: Arc<[Tau<V>]> }
```

Layers are immutable once created. Cloning a layer is an atomic reference-count bump -- the `Arc` is never copied.

`min_start` and `max_end` are skip-check bounds. A point query for timestamp `t` can skip an entire layer with two comparisons (`t < min_start || t >= max_end`) before touching the `taus` slice.

Within the slice, a binary search -- `partition_point` on `tau.end <= t` -- locates the candidate in O(log n).

### `Lens<V>`

A named temporal function: either a `Base` lens backed by a store, or a `Derived` lens backed by a lazy closure.

```
Lens::Base          -- delegates to the store layer stack
Lens::Derived(f)    -- f: Arc<dyn Fn(Timestamp) -> Option<V>>
```

Derived lenses are closures compiled at `DERIVE` time. At query time they call `f(t)` -- no second pass over a stored value. The closure captures references to other lenses so derivations can chain: `DERIVE c AS a + b` compiles into a closure that calls the closures of `a` and `b`.

Cycle detection runs at `DERIVE` time by walking the dependency graph. Any cycle rejects the statement before the closure is installed.

---

## Storage

### Backends

The `Store<V>` trait has two implementations:

**`InMemory`** -- a `HashMap<name, Vec<Layer<V>>>` with no I/O. Used for tests and ephemeral workloads. State is lost on drop.

**`Disk`** -- a binary file with the following format:

```
magic (3 bytes)  "TAU" (plaintext) or "TAUE" (encrypted)
[entries...]
  length (u32 LE)
  crc32 (u32 LE) or nonce+tag (encrypted)
  payload bytes
```

On open, the file is read entry by entry. Each entry is checked for integrity (CRC32 or AEAD tag), decoded, and replayed into the in-memory layer stack. The file handle is kept open in append mode; new entries are written by seeking to end and appending.

Encryption is AES-256-GCM with a random 12-byte nonce per entry. The key is never stored -- it must be supplied via `TAU_ENCRYPTION_KEY` (a 64-character hex string) at startup. Without the key, an encrypted file is unreadable. The `TAUE` magic byte prevents accidentally opening an encrypted file without a key.

### Write-Ahead Log

The WAL sits between the caller and the store. Every mutation writes to the WAL first, fsyncs, and only then writes to the store. On startup, the WAL is replayed before the store is opened.

WAL entries are line-oriented text:

```
<crc32hex> <base64-payload>   -- data entry
S:<checksum> <CREATE LENS ...>  -- schema DDL
SE:<crc32hex> <base64-encrypted-DDL>  -- encrypted schema DDL
```

Schema entries (`S:` / `SE:`) carry the raw TauQL text of `CREATE LENS` and `DERIVE LENS` statements. On replay, these are re-parsed and executed against a fresh executor with `in_replay = true`, which suppresses re-writing them back to the WAL.

The WAL is checkpointed after compaction. Checkpointing writes a fresh snapshot of the current in-memory state to a new file and swaps it in, bounding disk usage.

### Compaction

Each base lens accumulates layers over time. A point query must walk layers newest-first until it finds a covering tau. With many layers this is linear in the layer count, not O(log n).

Auto-compaction fires when a lens exceeds a threshold (default: 4 layers). It runs a sweep-line algorithm over all layers, producing a single canonical layer that gives identical query results:

1. Build a list of start/end events, one pair per tau across all layers.
2. Sort events by timestamp, ends before starts at ties (so a closing interval emits before a new one opens).
3. Walk events. A max-heap keyed by `(layer_idx, tau_idx)` tracks which layers are "active" at each point. The layer with the highest index (newest) wins when multiple layers overlap.
4. Emit a merged segment whenever the winning value at the current cursor differs from the last emitted segment.

This is O(E log E) where E is the total number of taus across all layers. After compaction, the lens has exactly one layer.

The `Store::append` return value is a `bool` indicating whether compaction fired. The `Database` layer uses this signal to decide whether to WAL-checkpoint.

---

## Database and Executor

### The `Database<V>` layer

`Database<V>` owns a `Store<V>` behind `RwLock` and an optional `Wal` behind `Mutex`. It presents a single entry point for mutations:

```
WAL.write(entry)  -->  WAL.fsync()  -->  Store.append(layer)
```

A WAL fsync failure leaves the in-memory store unchanged -- the entry is not committed. There is no partial-write window visible to readers.

### `Executor`

`Executor` is the top-level query processor. It owns a `HashMap<String, DbState>` keyed by database name, an active-database pointer, and a `UserStore`.

Each `DbState` carries:
- A `Database<Value>` (the live store + WAL)
- A `HashMap<name, Type>` for base lens type declarations
- A `HashMap<name, Expr>` for derived lens ASTs
- A monotonic `next_layer_id` counter

**Two entry-point pairs:**

`exec` / `exec_read` -- unrestricted. Used by library consumers, tests, and schema replay (where `in_replay = true` prevents DDL from being re-appended to the WAL).

`exec_as(stmt, caller)` / `exec_read_as(stmt, caller)` -- looks up `caller` in `self.users`, calls `check_permission`, then delegates to `exec`/`exec_read`. The TCP server uses these for every authenticated session. `SHOW DATABASES` post-filters to only databases the caller has any grant on.

The split is intentional: embedding Tau as a library bypasses auth entirely. Auth is a server concern, not an engine concern.

---

## Query Language

TauQL is a line-oriented command language. One statement in, one response line out. The grammar is minimal by design -- there is no implicit join, no subquery, and no transaction syntax.

Statements fall into three categories:

**DDL** -- `CREATE DATABASE`, `DROP DATABASE`, `USE DATABASE`, `CREATE LENS`, `DROP LENS`, `DERIVE LENS`

**DML** -- `APPEND LENS`, `COPY LENS FROM`

**Query** -- `AT LENS`, `RANGE LENS`, `REDUCE LENS`

**Auth** -- `CREATE USER`, `DROP USER`, `GRANT`, `REVOKE`, `SHOW USERS`, `SHOW GRANTS`

The parser is a `nom` combinator in `libtau::ql::parser`. It returns a single `Stmt` variant. Adding a new statement requires changes to four files: `ast.rs` (new variant + `Display`), `parser.rs` (production + `alt` entry), `executor.rs` (handler + `check_permission` arm), and `bin/tau/main.rs` (output formatter).

Operator precedence from low to high: `||`, `&&`, comparison (`== != < <= > >=`), additive (`+ -`), multiplicative (`* / %`), unary (`- !`), primary.

Aggregation expressions (`avg(lens, rel_start, rel_end)`) are first-class nodes in the expression grammar, available inside `DERIVE` and `WHERE`. They evaluate by querying the named lens over the window `[t + rel_start, t + rel_end)` relative to the evaluation timestamp.

---

## Server

### Protocol

The TCP server speaks a line-oriented text protocol: a single TauQL statement on a line in, a single response line out. Responses are:

```
OK <result>
ERR <kind>: <message>
```

`AT` returns `OK <value>` or `OK nil`. `RANGE` returns `OK [start,end,value;...]`. `REDUCE` returns `OK <scalar>`. DDL returns `OK`. Auth failure returns `ERR auth:`.

The server routes read-only statements (`AT`, `RANGE`, `REDUCE`, `SHOW *`) through a shared read lock on the `Arc<RwLock<Executor>>`. Write statements take an exclusive lock. Concurrent reads proceed in parallel; a write blocks new reads until it completes.

### Connection handling

Each accepted connection runs on its own OS thread. The connection loop:

1. Reads a line.
2. If auth is enabled and the session is not yet authenticated, handles `AUTH user pass` first.
3. Routes the statement through `exec_as` (authenticated) or `exec` (unauthenticated).
4. Writes the response line.
5. Resets the idle timer.

The idle timer runs on the connection thread as a `Instant`-based check after each loop iteration. The connection limit is checked atomically via a global counter before accepting each new connection.

### TLS

TLS is optional. When enabled, the server wraps each accepted `TcpStream` with a `rustls::ServerConnection` before handing it to the connection loop. The `rustls` feature is configured with `ring` as the crypto provider and `default-features = false` to avoid loading both `ring` and `aws-lc-rs` simultaneously (which panics).

`tauctl` uses a no-verify `ServerCertVerifier` to work against the server's ephemeral self-signed dev cert. This is intentional for development -- not suitable for public networks.

### Authentication

Authentication uses Argon2id password hashing. User records store a PHC-format hash string; verification calls `argon2::verify_encoded`. The `UserStore` persists atomically: write to `.tmp`, then rename.

The permission model is a 5-bit `Perm` bitmap (Create, Read, Update, Delete, Admin) per user per database. A special `"*"` key acts as a wildcard covering every database. Effective permissions are `grants[db] | grants["*"]`.

Global admin (`A` on `"*"`) is required for `CREATE DATABASE`, `CREATE USER`, `DROP USER`, `SHOW USERS`, and `SHOW GRANTS` of another user.

---

## Testing

Testing is described in detail in [TEST.md](TEST.md). The short version:

**Unit tests** (`cargo test`) are regression anchors: known-shape inputs, known-shape outputs. Wire protocol responses, error strings, WAL checksum mismatches.

**Property-based tests** (Hegel / Hypothesis) are invariants over randomized inputs: `Layer::at` agrees with a linear scan, `Value::encode`/`decode` roundtrips, `handle_query` never panics on arbitrary input. Currently 103 properties across 12 modules.

**Deterministic simulation tester** (`cargo run --bin dst`) stresses emergent system behaviour: WAL replay after compaction, concurrent correction layers, authorization interactions across hundreds of operations. A seeded PRNG makes any failure reproducible from the seed alone. An oracle -- a simple `Vec` with linear-scan semantics -- cross-checks every query result the executor returns.

---

## Design Decisions

### Immutable layers over in-place mutation

Mutation would require finding and splitting or replacing existing taus. With immutable layers, a correction is an append. The WAL writes one new entry. The in-memory state gains one new layer. The old data is untouched.

The tradeoff is query cost: point lookup is O(log n) per layer, not O(log n) total. Compaction restores O(log n) by collapsing layers. The sweep-line algorithm is the mechanism that makes this tradeoff practical.

### No transaction syntax

Tau does not expose transactions. The WAL provides durability; compaction provides space reclamation. Both happen automatically. Multi-statement atomicity would require a different coordination model (locking, MVCC, or 2PC), none of which fit the single-writer append-only model cheaply.

If multiple appends must land atomically, batch them: `APPEND LENS x 0 10 1, 10 20 2` is a single layer and a single WAL entry.

### `exec` vs `exec_as` split

Auth is a transport concern. A Tau binary embedded in another process, reading sensor data directly, has no need for network authentication. Keeping the auth check out of `exec` means embedded use never pays the overhead or requires a dummy user.

### Arc-backed layers

Layer data is immutable once created. Sharing it across the read path without copying is safe. A compacted layer replaces the old stack atomically; any concurrent reader holding a reference to the old layers reads consistent data until it finishes.

### WAL-first ordering

Writing to the WAL before writing to the store means a crash between the two leaves an entry in the WAL that is replayed on the next startup, completing the write. It does not leave a partial write visible to readers. The only risk is a duplicate replay, which the idempotent `append` semantics handle: replaying a layer that already exists adds it again, but the query result is identical (newest-layer-wins picks the same value in both cases).

### Two modes in the `dst` binary

`dst` serves as both the correctness tester and the performance measurement tool. In full mode (default), it spawns real server processes, tests all config combinations, injects faults, and outputs throughput numbers alongside pass/fail results. In embedded mode (`--quick`), it bypasses the server entirely and uses the library executor directly, covering centuries of simulated time without I/O overhead. The seed printed at startup makes any failure reproducible from a single flag.
