# storage

Pluggable backing storage for the layer stack. The `Store<V>` trait is the only interface the rest of the library uses; backends are swapped at construction time.

## Store trait

```rust
pub trait Store<V>: Send + Sync {
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool>;
    fn layers(&self, lens: &str) -> Option<Arc<[Layer<V>]>>;
    fn at(&self, lens: &str, t: Timestamp) -> Option<V>;                         // default impl
    fn get(&self, lens: &str, coords: &[Timestamp], as_of: Option<i64>) -> Option<V>;  // default impl
    fn scan(&self, lens: &str, start: Timestamp, end: Timestamp,
            fixed: &[Timestamp], as_of: Option<i64>) -> Vec<(Timestamp, Timestamp, V)>; // default impl
    fn drop_lens(&mut self, _lens: &str) {}                    // default no-op
    fn lens_names(&self) -> Vec<String> { Vec::new() }         // default empty
    fn append_schema(&mut self, _stmt: &str) -> io::Result<()> { Ok(()) }  // default no-op
    fn schema_stmts(&self) -> Vec<String> { Vec::new() }       // default empty
    fn checkpoint_flush(&self) -> io::Result<bool> { Ok(false) }  // default no-op
}
```

`append` returns `true` when compaction ran (the caller decides whether to WAL-checkpoint). `get`/`scan` are
the pushdown read path (point/range, N-D, MVCC `as_of`) the executor calls directly instead of always
materialising the full layer stack via `layers()`; their default implementations resolve over `layers()`,
so a backend only needs to override them for a real performance win (`Sstable` does).

`append_schema` / `schema_stmts` let a self-persisting backend (the disk store) keep schema DDL durable. They are no-ops for `InMemory`; WAL-backed setups persist schema in the WAL instead, so `Database` only calls them when no WAL is attached.

## Implementations

**`InMemory<V>`** — `FxHashMap<String, Vec<Layer<V>>>`. Zero I/O. Used by all unit tests, and the in-memory server mode. Compaction fires automatically when a lens exceeds the threshold.

**`Sstable<V>`** — the on-disk backend, configurable via `[disk] backend = "disk"` in `config.toml` (default: `"memory"`): a memtable (same `Arc<[Layer]>` RCU shape as `InMemory`) flushes to immutable, zstd-compressed run files (`TAUR` magic) on checkpoint instead of rewriting anything already on disk, listed by a small atomically-rewritten manifest (`TAUM` magic). Reads merge the memtable with runs and resolve newest-wins/`AS OF` at query time; a run is skipped without decoding when its footer (per-lens min/max + a range-bucketed bloom filter) proves it can't cover the query, and a decoded run body is cached (immutable once written) so repeat queries don't re-decompress it. Compaction has exactly one trigger — see `sstable.rs`'s module doc for why that matters and what replaced the earlier three-trigger design. `DROP LENS` bumps a per-lens epoch (persisted in the manifest) so pre-drop run data is shadowed without rewriting old files. Adjust the compression trade-off with `set_compression_level(level)` (1 = fastest, 22 = best ratio); exposed via `[disk] compression_level` in `config.toml`. Encryption is AES-256-GCM (compress-then-encrypt), keyed by `TAU_ENCRYPTION_KEY`, applied separately to each run's body and footer.

**`Wal`** — write-ahead log. Two entry kinds: data entries (CRC32-prefixed binary) and schema entries (`S:` / `SE:` prefix carrying raw lens DDL — `CREATE LENS`, `DERIVE LENS`, `XDERIVE LENS`, `SET TTL`, `UNSET TTL`, `DROP LENS`). Schema entries are replayed separately from data on startup. Set `set_fsync_each(false)` and call `sync()` periodically for group-commit mode.

## Compaction

`compact_layers` in `layers.rs` compacts **within** each transaction-time generation (a run of layers sharing a `written_at`) and never merges across generations, so `AS OF`/`HISTORY` stay exact. Single-axis lenses use a sweep-line over `(time, start/end, layer_idx)` events, O(E log E) where E = 2 × total tau count, merging adjacent equal-value segments; multi-axis lenses use orthotope subtraction. Its input must have equal-`written_at` layers contiguous (callers sort by `(written_at, id)` first if they can't otherwise guarantee that, e.g. after concatenating multiple `Sstable` runs' reconstructions).

### Per-layer indexing: linear fast-skip vs. an interval tree

Evaluated once for `InMemory`'s in-process layers and again for `Sstable`'s on-disk runs: an interval tree
(or R-tree) only pays for itself past roughly a thousand disjoint fragments in a single layer, and is
actively worse than the simple `min_start`/`max_end` fast-skip + linear/binary search when a lens's taus
overlap heavily on the valid axis (the common case for corrected/restated data). Kept the linear approach in
both backends; revisit only if a real workload shows layers with thousands of genuinely disjoint fragments.

## Database

`Database<V>` owns a `Box<dyn Store<V>>` behind `Arc<RwLock<>>` and an optional `Wal` behind `Arc<Mutex<>>`. Append order is WAL-first then store; a WAL fsync failure leaves the in-memory state unchanged.

`Database::layers()` returns `Option<Arc<[Layer<V>]>>` — an Arc-wrapped snapshot so range query phases share one allocation without re-locking.
