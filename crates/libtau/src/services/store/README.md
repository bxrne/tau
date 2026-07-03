# services/store

## What it is

The pluggable persistence drivers for the layer stack, composed per database by the db service according to the kernel's `StorageBackend`. The `Store<V>` trait is the only interface the rest of the library uses: `append` (returns whether compaction fired), `layers`, pushdown reads (`at`/`get`/`scan` — point, N-D, MVCC `as_of`), plus schema-DDL persistence hooks for self-persisting backends.

## How it works

**`InMemory<V>`** is a hash map of `Arc<[Layer]>` stacks — zero I/O, used by unit tests and the in-memory server mode, with automatic threshold compaction. **`Sstable<V>`** is the on-disk backend: a memtable flushes to immutable zstd-compressed run files listed by a small atomically-rewritten manifest; reads merge memtable and runs with newest-wins/`AS OF` resolved at query time, skipping runs whose footer (min/max + bloom filter) proves irrelevance and caching decoded bodies. Compaction has exactly one trigger (see the module doc in `sstable.rs`); `DROP LENS` bumps a persisted per-lens epoch instead of rewriting files; encryption is AES-256-GCM, compress-then-encrypt, per run body and footer. **`Wal`** is the write-ahead log: CRC-checked data entries plus `S:`/`SE:` schema entries replayed separately on startup; `set_fsync_each(false)` + periodic `sync()` gives group commit.

Compaction (`layers.rs`) works **within** each transaction-time generation and never across, so `AS OF`/`HISTORY` stay exact — a sweep-line for single-axis lenses, orthotope subtraction for multi-axis. Per-layer indexing stays linear with a min/max fast-skip: an interval tree only pays past ~1000 disjoint fragments per layer and loses on the common heavily-overlapping case.

## Using it

Backends are selected at kernel construction (`Kernel::with_threshold` / `with_wal` / `with_disk_backend`) and never swapped live. For deterministic simulation, `faults.rs` provides the per-kernel `FaultInjector` (`kernel.faults()`): an armed countdown the `Wal` consults before every write, so a chosen upcoming write fails with a clean injected `io::Error` — proving the WAL-first invariant (store untouched, clean `ExecError::Io`, no panic) without touching any file.
