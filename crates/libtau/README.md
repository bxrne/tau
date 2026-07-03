# libtau

## What it is

The tau engine library — everything else (`tau`, `tauctl`, `dst`) depends on this crate. It is a **syscall-routing microkernel**: a `Kernel` owns four built-in services — db (mutations), query (reads), auth (users and grants), metrics — plus two per-kernel capabilities, a virtual `Clock` and a `FaultInjector`. Every statement flows through the kernel, which applies per-user policy and routes it to the owning service; no service ever calls another directly, and permission checks live in the kernel, never in a service.

Three primitives form the data model. A `Tau<V>` is an immutable value over one half-open `[lo, hi)` interval per axis (axis 0 is valid time; `AXES (…)` adds filter axes). A `Layer<V>` is a batch of taus sharing a `written_at` transaction time, `Arc`-backed so clones are pointer bumps. A **lens** is a named temporal function: *base* lenses are storage-backed with a declared type, *derived* lenses (`DERIVE`) are expressions evaluated lazily at query time, and *materialised* lenses (`XDERIVE`) store the result eagerly and auto-refresh when a source lens is corrected.

## How it works

Layers are append-only and **newest-layer-wins** on overlap at query time; corrections never overwrite. Compaction normalises layers *within* each transaction-time generation (never across, so `AT … AS OF` and `HISTORY` stay exact) once a per-lens threshold is crossed.

The kernel routes read-only statements to the query service, mutations (DDL, appends, transactions, backup/restore) to the db service, and user management to the auth service. The two statement services share one registry of named databases; the query service takes only read locks, so lookups never serialise on writers or each other. Storage is pluggable per database: the `InMemory` and `Sstable` (on-disk) drivers plus a `Wal`, composed by the db service — appends are always **WAL-first, then store**, so a failed WAL write leaves memory untouched. The TauQL parser (`ql`) stays outside the kernel.

Determinism is a first-class capability: `kernel.clock()` pins transaction stamps and TTL "now" for one kernel only, and `kernel.faults()` arms a clean failure of a chosen upcoming WAL write — the deterministic-simulation suite drives both, with no process-global state.

## Using it

All `Kernel` methods take `&self`; share it as a plain `Arc<Kernel>` — locking is internal.

```rust
use libtau::{Kernel, parse};

let kernel = Kernel::new();
for q in [
    "CREATE DATABASE mydb",
    "CREATE LENS temperature float",
    "APPEND LENS temperature 0 3600 21.5",
] {
    let (_, stmt) = parse(q).unwrap();
    kernel.exec(&stmt).unwrap();
}
let (_, at) = parse("AT LENS temperature 1800").unwrap();
let result = kernel.exec_read(&at).unwrap();
```

`exec` / `exec_read` are unrestricted (library embedding bypasses auth); `exec_as` / `exec_read_as` enforce CRUDA grants and back the TCP server. Backends come from the constructors: `Kernel::new` / `with_threshold` (memory), `with_wal[_threshold]` (memory + write-ahead log), `with_disk_backend` (SSTable + per-database WAL). Performance levers: `set_wal_fsync_each(false)` + periodic `flush_wal()` for group commit, `set_wal_max_bytes(n)` to bound WAL growth, and the disk backend's zstd `compression_level`.

Focused module docs: [ql](src/ql/README.md) (grammar) and [services/store](src/services/store/README.md) (storage drivers).
