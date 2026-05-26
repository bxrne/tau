# libtau

The core library. Everything the server and any future client library needs lives here.

## What it is

`libtau` implements a time-series database based on **immutable, layered temporal intervals**. The central idea is that data is never corrected in-place. When a value changes, a new layer is appended on top of existing ones, and the newest layer always wins at query time. This is the same model used in bitemporal databases and event-sourced systems - it makes the full correction history available for free and eliminates write-write conflicts entirely.

## Design decisions

### Generics over a dynamic dispatch boundary

The core types (`Tau<V>`, `Layer<V>`, `Database<V>`) are generic over the value type. This lets a consumer embed a typed database - a `Database<f64>` for sensor readings - without boxing every value. The executor adds a single dynamic layer at the top: it uses `Database<Value>` where `Value` is an enum that covers all supported types. The cost is that the executor can only be used with dynamic values; the typed API is available for library consumers who want to skip the executor entirely.

### Newest-layer-wins without tombstones

There is no delete operation in the traditional sense. Deleting a value means appending a new layer whose taus cover the region you want gone, but with a `null` value - this is visible in `RANGE` output as gaps. The immutability guarantee means consumers can snapshot a layer stack and query it without worrying about concurrent writes invalidating their view.

### Derived lenses are purely lazy

A derived lens is an AST expression stored at definition time. Every `AT` or `RANGE` query re-evaluates the expression from scratch - there is no materialisation, no caching, and no incremental maintenance. This is correct and simple, but it means a deeply nested chain of derived lenses re-evaluates each intermediate step on every point lookup. For 1.0, this is fine; for hot derived lenses over large ranges it may need a materialised view path.

### Crypto is a standalone module

`crypto` has no dependency on the storage or executor modules. It exposes a small, focused API: `encrypt`/`decrypt` for symmetric AES-256-GCM with a random 12-byte nonce per blob. The server wires it into the WAL and Disk paths; the library doesn't force any security policy on embedders.

### Multi-user authorisation lives next to the executor

`users` defines a 5-bit `Perm` bitmap (`C`, `R`, `U`, `D`, `A`) and a `UserStore` that maps `database_name -> Perm` per user - with `"*"` as a wildcard that grants on every database (including ones created later). The `Executor` owns one of these and exposes `exec_as(stmt, caller)` / `exec_read_as` that check the matched user's grants before delegating to the plain `exec`/`exec_read` path. Unrestricted `exec` and `exec_read` are still available for library / test use and for schema-replay startup. Persistence is a single text file, rewritten atomically on every mutation.

### Metrics are shared via Arc

The `Executor` owns an `Arc<Metrics>` field. The TCP server receives a clone of that Arc and shares it with the metrics HTTP thread. This lets the metrics endpoint read counters without going through the executor lock - the counters use `Relaxed` atomics because they are best-effort observability data, not synchronisation barriers.
