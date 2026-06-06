# storage

Pluggable backing storage for the layer stack. The `Store<V>` trait is the only interface the rest of the library uses; backends are swapped at construction time.

## Store trait

```rust
pub trait Store<V>: Send + Sync {
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool>;
    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>>;
    fn at(&self, lens: &str, t: Timestamp) -> Option<V>;       // default impl
    fn drop_lens(&mut self, _lens: &str) {}                    // default no-op
    fn lens_names(&self) -> Vec<String> { Vec::new() }         // default empty
    fn append_schema(&mut self, _stmt: &str) -> io::Result<()> { Ok(()) }  // default no-op
    fn schema_stmts(&self) -> Vec<String> { Vec::new() }       // default empty
}
```

`append` returns `true` when compaction ran (the caller decides whether to WAL-checkpoint).

`append_schema` / `schema_stmts` let a self-persisting backend (the disk store) keep schema DDL durable. They are no-ops for `InMemory`; WAL-backed setups persist schema in the WAL instead, so `Database` only calls them when no WAL is attached.

## Implementations

**`InMemory<V>`** — `FxHashMap<String, Vec<Layer<V>>>`. Zero I/O. Used by all unit tests, and the in-memory server mode. Compaction fires automatically when a lens exceeds the threshold.

**`Disk<V>`** — binary flat file (format version 2) with zstd block compression. Header is the 4-byte magic `TAUZ` + VERSION + FLAGS byte + CRC32 (covers magic/version/flags). FLAGS bit 0 signals AES-256-GCM encryption; the compressed body is encrypted when set (compress-then-encrypt order). The body is a schema section (length-prefixed DDL strings) followed by the layer entries. All writes go through `flush()`, which serialises schema + entries, compresses at the configured level (default 3), optionally encrypts, and atomically replaces the file via a `.tmp` rename. A flush fires on **every append** and on every `append_schema`, so both data and DDL (`CREATE LENS` / `DERIVE LENS` / `SET TTL` / `DROP LENS`) are durable before the call returns and survive a restart — the executor replays the schema when `CREATE DATABASE` re-opens the file. Adjust the compression trade-off with `set_compression_level(level)` (1 = fastest, 22 = best ratio); exposed via `[disk] compression_level` in `config.toml`. Version 1 files (no schema section) still open.

> **Note:** the server backend is configurable via `[disk] backend = "disk"` in `config.toml` (default: `"memory"`).

**`Wal`** — write-ahead log. Two entry kinds: data entries (CRC32-prefixed binary) and schema entries (`S:` / `SE:` prefix carrying raw lens DDL — `CREATE LENS`, `DERIVE LENS`, `SET TTL`, `UNSET TTL`, `DROP LENS`). Schema entries are replayed separately from data on startup. Set `set_fsync_each(false)` and call `sync()` periodically for group-commit mode.

## Compaction

`compact_layers` in `store.rs` is a sweep-line algorithm: it builds `(time, start/end, layer_idx)` events, sorts them, and produces a single merged layer with newest-wins semantics. O(E log E) where E = 2 × total tau count. Adjacent segments with equal values are merged.

## Database

`Database<V>` owns a `Box<dyn Store<V>>` behind `Arc<RwLock<>>` and an optional `Wal` behind `Arc<Mutex<>>`. Append order is WAL-first then store; a WAL fsync failure leaves the in-memory state unchanged.

`Database::layers()` returns `Option<Arc<Vec<Layer<V>>>>` — an Arc-wrapped snapshot so range query phases share one allocation without re-locking.
