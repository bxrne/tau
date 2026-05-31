# storage

Pluggable backing storage for the layer stack. The `Store<V>` trait is the only interface the rest of the library uses; backends are swapped at construction time.

## Store trait

```rust
pub trait Store<V>: Send + Sync {
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool>;
    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>>;
    fn at(&self, lens: &str, t: Timestamp) -> Option<V>;      // default impl
    fn drop_lens(&mut self, _lens: &str) {}                    // default no-op
    fn lens_names(&self) -> Vec<String>;
}
```

`append` returns `true` when compaction ran (the caller decides whether to WAL-checkpoint).

## Implementations

**`InMemory<V>`** — `FxHashMap<String, Vec<Layer<V>>>`. Zero I/O. Used by all unit tests, the embedded DST path, and the in-memory server mode. Compaction fires automatically when a lens exceeds the threshold.

**`Disk<V>`** — binary flat file. Header is the magic `TAU` (plaintext) or `TAUE` (encrypted). Each entry is a CRC32-prefixed `WalEntry`. Supports AES-256-GCM encryption via `TAU_ENCRYPTION_KEY` env var. Compaction rewrites the file atomically (write to `.tmp`, then rename); set `set_rewrite_on_compact(false)` to skip the rewrite on bulk-load paths.

**`Wal`** — write-ahead log. Two entry kinds: data entries (CRC32-prefixed binary) and schema entries (`S:` / `SE:` prefix carrying raw `CREATE LENS` / `DERIVE LENS` text). Schema entries are replayed separately from data on startup. Set `set_fsync_each(false)` and call `sync()` periodically for group-commit mode.

## Compaction

`compact_layers` in `store.rs` is a sweep-line algorithm: it builds `(time, start/end, layer_idx)` events, sorts them, and produces a single merged layer with newest-wins semantics. O(E log E) where E = 2 × total tau count. Adjacent segments with equal values are merged.

## Database

`Database<V>` owns a `Box<dyn Store<V>>` behind `Arc<RwLock<>>` and an optional `Wal` behind `Arc<Mutex<>>`. Append order is WAL-first then store; a WAL fsync failure leaves the in-memory state unchanged.

`Database::layers()` returns `Option<Arc<Vec<Layer<V>>>>` — an Arc-wrapped snapshot so range query phases share one allocation without re-locking.
