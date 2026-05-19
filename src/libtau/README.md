# libtau

The core library. Everything the server and any future client library needs lives here.

## What it is

`libtau` implements a time-series database based on **immutable, layered temporal intervals**. The central idea is that data is never corrected in-place. When a value changes, a new layer is appended on top of existing ones, and the newest layer always wins at query time. This is the same model used in bitemporal databases and event-sourced systems — it makes the full correction history available for free and eliminates write-write conflicts entirely.

## Module layout

| Module | Responsibility |
|--------|---------------|
| `model` | The three primitive types: `Tau`, `Layer`, `Lens` |
| `value` | The dynamic runtime value type used by the executor |
| `storage` | Pluggable backends: in-memory, binary disk, write-ahead log |
| `database` | Orchestration layer — owns a store and an optional WAL |
| `ql` | Query language: AST definitions and nom-based parser |
| `executor` | Wires parsed statements to a registry of live databases |
| `auth` | Username/password credential management (argon2id) |
| `crypto` | AES-256-GCM primitives shared by WAL and Disk encryption |

## Design decisions

### Generics over a dynamic dispatch boundary

The core types (`Tau<V>`, `Layer<V>`, `Database<V>`) are generic over the value type. This lets a consumer embed a typed database — a `Database<f64>` for sensor readings — without boxing every value. The executor adds a single dynamic layer at the top: it uses `Database<Value>` where `Value` is an enum that covers all supported types. The cost is that the executor can only be used with dynamic values; the typed API is available for library consumers who want to skip the executor entirely.

### Newest-layer-wins without tombstones

There is no delete operation in the traditional sense. Deleting a value means appending a new layer whose taus cover the region you want gone, but with a `null` value — this is visible in `RANGE` output as gaps. The immutability guarantee means consumers can snapshot a layer stack and query it without worrying about concurrent writes invalidating their view.

### Derived lenses are purely lazy

A derived lens is an AST expression stored at definition time. Every `AT` or `RANGE` query re-evaluates the expression from scratch — there is no materialisation, no caching, and no incremental maintenance. This is correct and simple, but it means a deeply nested chain of derived lenses re-evaluates each intermediate step on every point lookup. For 1.0, this is fine; for hot derived lenses over large ranges it may need a materialised view path.

### Auth and crypto are standalone modules

`auth` and `crypto` have no dependency on the storage or executor modules. They expose small, focused APIs: `Credentials::new`/`verify` for auth, `encrypt`/`decrypt` for symmetric AES-GCM. The server wires them together; the library doesn't force any security policy on embedders.
