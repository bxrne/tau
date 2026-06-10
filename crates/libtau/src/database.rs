use crate::metrics::Metrics;
use crate::model::{Layer, LayerId, Timestamp};
use crate::storage::{Store, Wal, WalEntry, wal::Codec};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tracing::{debug, info, instrument, warn};

/// Registry + orchestration layer.
///
/// `Database` owns a pluggable `Store` behind an `Arc<RwLock<…>>` so it is
/// cheap to clone and safe to share across threads.  All public API lives here;
/// `model` and `storage` are internal implementation details.
///
/// An optional WAL provides durability: every `append` is written to disk
/// before the in-memory store is updated.  Use `Database::with_wal` to enable
/// it, or `Database::open` to replay an existing log on startup.
pub struct Database<V>
where
    V: Clone + Send + Sync + 'static,
{
    store: Arc<RwLock<Box<dyn Store<V>>>>,
    wal: Option<Arc<Mutex<Wal>>>,
    /// Serializes `append`, `append_schema`, and `checkpoint` against each
    /// other. Without this, a `checkpoint` can snapshot the store *before* a
    /// concurrent `append`'s store-write lands, then rotate the WAL to a file
    /// that omits that append's WAL entry — the layer ends up in the
    /// in-memory store and reported as durably written, but is unrecoverable
    /// after a crash. Holding this mutex for each operation's full duration
    /// makes "WAL write + store write" and "snapshot store + rotate WAL"
    /// mutually exclusive. Query paths (`at_name`, range/reduce reads) do not
    /// take this lock and remain fully concurrent.
    write_lock: Arc<Mutex<()>>,
    /// When `true` (default), `append` calls `checkpoint()` immediately after a
    /// store-side compaction so the WAL never holds superseded entries.  Set to
    /// `false` to defer that work to an explicit caller - useful in bulk-load
    /// paths where many compactions fire and a single trailing checkpoint
    /// suffices.
    auto_checkpoint: AtomicBool,
    /// Optional shared metrics sink.  Populated when the database is created
    /// through an executor that has metrics enabled (always true for the server).
    pub(crate) metrics: Option<Arc<Metrics>>,
}

impl<V> Clone for Database<V>
where
    V: Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            wal: self.wal.clone(),
            write_lock: self.write_lock.clone(),
            auto_checkpoint: AtomicBool::new(self.auto_checkpoint.load(Ordering::Relaxed)),
            metrics: self.metrics.clone(),
        }
    }
}

impl<V> Database<V>
where
    V: Clone + Send + Sync + 'static,
{
    /// Create an in-memory-only database (no durability).
    pub fn new(store: impl Store<V> + 'static) -> Self {
        info!("creating in-memory database (no WAL)");
        Self {
            store: Arc::new(RwLock::new(Box::new(store))),
            wal: None,
            write_lock: Arc::new(Mutex::new(())),
            auto_checkpoint: AtomicBool::new(true),
            metrics: None,
        }
    }

    pub fn set_metrics(&mut self, m: Arc<Metrics>) {
        self.metrics = Some(m);
    }

    /// Attach a WAL for durability.  Every subsequent `append` is written to
    /// the log before the store is updated.
    pub fn with_wal(store: impl Store<V> + 'static, wal: Wal) -> Self {
        info!("creating database with WAL enabled");
        Self {
            store: Arc::new(RwLock::new(Box::new(store))),
            wal: Some(Arc::new(Mutex::new(wal))),
            write_lock: Arc::new(Mutex::new(())),
            auto_checkpoint: AtomicBool::new(true),
            metrics: None,
        }
    }

    /// Control whether `append` automatically rewrites the WAL after a
    /// compaction.  When disabled, the WAL grows past the live layer set but
    /// each `append` avoids the rewrite cost (full read of every live layer
    /// + atomic rename).  Call `Database::checkpoint` manually to compact.
    pub fn set_auto_checkpoint(&self, on: bool) {
        self.auto_checkpoint.store(on, Ordering::Relaxed);
    }

    /// Control per-record `flush + sync_data` on the WAL.  When `false` the
    /// WAL buffers writes for throughput; call `Wal::flush` explicitly for
    /// periodic group-commit durability.  No-op when no WAL is configured.
    pub fn set_wal_fsync_each(&self, on: bool) {
        if let Some(wal) = &self.wal {
            wal.lock().expect("wal lock poisoned").set_fsync_each(on);
        }
    }

    /// Set a soft WAL file-size cap.  Once the WAL reaches `bytes` on disk,
    /// `append` will trigger a checkpoint (rewrite) to compact it, even when
    /// no compaction fired.  No-op when no WAL is configured.
    pub fn set_wal_max_bytes(&self, bytes: u64) {
        if let Some(wal) = &self.wal {
            wal.lock().expect("wal lock poisoned").set_max_bytes(bytes);
        }
    }

    /// Return the on-disk size of the WAL file in bytes, or `0` when no WAL is
    /// configured or the file cannot be stat-ed.
    pub fn wal_size_bytes(&self) -> u64 {
        self.wal
            .as_ref()
            .map(|wal| wal.lock().expect("wal lock poisoned").size_bytes())
            .unwrap_or(0)
    }

    /// Flush and fsync the WAL explicitly.  Used for group-commit: after
    /// `set_wal_fsync_each(false)`, call this periodically to enforce a
    /// durability boundary without syncing on every record.
    pub fn wal_flush(&self) -> io::Result<()> {
        if let Some(wal) = &self.wal {
            wal.lock().expect("wal lock poisoned").sync()?;
        }
        Ok(())
    }

    /// Open an existing WAL, replay it into `store`, then return a live
    /// `Database` with durability enabled.
    ///
    /// Pass `Some(key)` to enable AES-256-GCM encryption of WAL entries.
    /// This is the standard startup path when persistence is required.
    #[instrument(skip(store, path, key), fields(path = %path.as_ref().display()))]
    pub fn open(
        mut store: impl Store<V> + 'static,
        path: impl AsRef<Path>,
        key: Option<[u8; 32]>,
    ) -> io::Result<Self>
    where
        V: Codec,
    {
        info!("opening WAL and replaying into store");
        let wal = Wal::open(path, key)?;
        wal.replay(&mut store)?;
        info!("replay complete");
        Ok(Self::with_wal(store, wal))
    }

    /// Append a `Layer` to a named base lens.
    ///
    /// Write-ahead semantics: the entry is fsynced to the WAL (if enabled)
    /// before the in-memory store is updated.  Returns an error if the WAL
    /// write fails; the in-memory store is **not** updated in that case.
    ///
    /// After the store update, if auto-compaction reduced the layer count, a
    /// WAL checkpoint is triggered that rewrites the WAL to contain only the
    /// live (post-compaction) layers, bounding WAL growth.
    pub fn append(&self, name: &str, layer: Layer<V>) -> io::Result<()>
    where
        V: Codec,
    {
        debug!(
            lens = name,
            layer_id = layer.id,
            tau_count = layer.taus.len(),
            "appending layer"
        );

        let _write_guard = self
            .write_lock
            .lock()
            .map_err(|_| io::Error::other("write lock poisoned"))?;

        // WAL first - do not update in-memory state if this fails.
        let wal_needs_rotation = if let Some(wal) = &self.wal {
            let wal_t0 = Instant::now();
            let mut guard = wal
                .lock()
                .map_err(|_| io::Error::other("WAL mutex poisoned"))?;
            guard.append_layer(name, &layer)?;
            let rotation = guard.needs_rotation();
            drop(guard);
            debug!(lens = name, layer_id = layer.id, "WAL entry written");
            if let Some(m) = &self.metrics {
                m.record_wal_write(wal_t0.elapsed().as_nanos() as u64);
            }
            rotation
        } else {
            false
        };

        // Then store - append returns whether compaction fired.
        let did_compact = {
            let mut store = self
                .store
                .write()
                .map_err(|_| io::Error::other("store lock poisoned"))?;
            store.append(name, layer)?
        }; // store write-lock released here

        if did_compact {
            if let Some(m) = &self.metrics {
                m.record_compaction();
            }
            if self.auto_checkpoint.load(Ordering::Relaxed) {
                self.checkpoint_locked()?;
            }
        } else if wal_needs_rotation && self.auto_checkpoint.load(Ordering::Relaxed) {
            debug!("WAL size cap reached; triggering checkpoint");
            self.checkpoint_locked()?;
        }

        Ok(())
    }

    /// Remove all stored layers for `lens`.
    ///
    /// Called by the executor when `DROP LENS` is processed so that a
    /// subsequent `CREATE LENS` with the same name starts from a clean state.
    pub fn drop_lens(&self, lens: &str) {
        if let Ok(mut store) = self.store.write() {
            store.drop_lens(lens);
        }
    }

    /// Write a schema DDL statement (e.g. `CREATE LENS temp int`) to durable
    /// storage.  Persisted in the WAL when one is attached, otherwise in the
    /// store's own file (the disk backend).  No-op for a pure in-memory store.
    pub fn append_schema(&self, stmt_text: &str) -> io::Result<()> {
        let _write_guard = self
            .write_lock
            .lock()
            .map_err(|_| io::Error::other("write lock poisoned"))?;

        if let Some(wal) = &self.wal {
            wal.lock()
                .map_err(|_| io::Error::other("WAL mutex poisoned"))?
                .append_schema(stmt_text)?;
        } else {
            self.store
                .write()
                .map_err(|_| io::Error::other("store lock poisoned"))?
                .append_schema(stmt_text)?;
        }
        Ok(())
    }

    /// Return the persisted schema DDL statements in write order, from the WAL
    /// when attached, otherwise from the store's own file.  Empty for a pure
    /// in-memory store.
    pub fn schema_stmts(&self) -> io::Result<Vec<String>> {
        match &self.wal {
            Some(wal) => wal
                .lock()
                .map_err(|_| io::Error::other("WAL mutex poisoned"))?
                .replay_schemas(),
            None => Ok(self
                .store
                .read()
                .map_err(|_| io::Error::other("store lock poisoned"))?
                .schema_stmts()),
        }
    }

    /// Highest layer ID currently in the store across all lenses.
    ///
    /// Used during WAL-replay startup to restore `next_layer_id` so new
    /// layers never collide with replayed ones.
    pub fn max_layer_id(&self) -> LayerId {
        let store = self.store.read().expect("store lock poisoned");
        store
            .lens_names()
            .iter()
            .filter_map(|name| store.layers(name))
            .flat_map(|layers| layers.iter().map(|l| l.id))
            .max()
            .unwrap_or(0)
    }

    /// Rotate the WAL to contain only the current live layers plus any
    /// previously written schema lines, discarding entries superseded by
    /// auto-compaction.
    ///
    /// # Locking
    ///
    /// Caller must hold `self.write_lock` for the duration of this call.
    /// This is private precisely so that invariant can be enforced by its
    /// two callers (`append`'s internal calls, and the public `checkpoint`
    /// wrapper below).
    fn checkpoint_locked(&self) -> io::Result<()>
    where
        V: Codec,
    {
        let wal = match &self.wal {
            Some(w) => w,
            None => return Ok(()),
        };

        let entries: Vec<WalEntry<V>> = {
            let store = self
                .store
                .read()
                .map_err(|_| io::Error::other("store lock poisoned"))?;
            store
                .lens_names()
                .into_iter()
                .filter_map(|name| {
                    store
                        .layers(&name)
                        .map(|layers| layers.iter().map(move |l| WalEntry::from_layer(&name, l)))
                })
                .flatten()
                .collect()
        }; // store read-lock released

        let mut wal_guard = wal
            .lock()
            .map_err(|_| io::Error::other("WAL mutex poisoned"))?;
        let schema_lines = wal_guard.raw_schema_lines()?;
        wal_guard.rotate(&schema_lines, &entries)?;
        debug!("WAL checkpoint complete");
        Ok(())
    }

    /// Rotate the WAL to contain only the current live layers plus any
    /// previously written schema lines, discarding entries superseded by
    /// auto-compaction.
    ///
    /// Safe to call concurrently with `append`/`append_schema`: this method
    /// and those serialize on an internal write lock so a checkpoint can
    /// never rotate the WAL to a file that omits a layer/schema line that a
    /// concurrent `append`/`append_schema` has already (or is about to)
    /// write.
    pub fn checkpoint(&self) -> io::Result<()>
    where
        V: Codec,
    {
        let _write_guard = self
            .write_lock
            .lock()
            .map_err(|_| io::Error::other("write lock poisoned"))?;
        self.checkpoint_locked()
    }

    /// Point lookup for a base lens at timestamp `t`. Newest layer wins.
    #[inline]
    pub fn at_name(&self, name: &str, t: Timestamp) -> Option<V> {
        self.store.read().expect("store lock poisoned").at(name, t)
    }

    /// Snapshot of the layer stack for `lens` wrapped in `Arc` so all query
    /// phases (bounds collection, segment building, etc.) share one allocation.
    /// Layer clones are pointer bumps; the Vec wrap is one extra Arc for the
    /// batch.  Returns `None` if the lens has never been appended to.
    pub fn layers(&self, lens: &str) -> Option<Arc<Vec<Layer<V>>>> {
        self.store
            .read()
            .expect("store lock poisoned")
            .layers(lens)
            .map(|v| Arc::new(v.clone()))
    }

    /// Export all layers for every lens as owned data.
    ///
    /// Returns a vec of `(lens_name, layers)` pairs in arbitrary order.
    /// Layers are cheap to clone because their tau slices are `Arc`-backed.
    /// Used by the executor's `BACKUP DATABASE` handler.
    pub fn export_layers(&self) -> Vec<(String, Vec<Layer<V>>)>
    where
        V: Clone,
    {
        let store = self.store.read().expect("store lock poisoned");
        store
            .lens_names()
            .into_iter()
            .filter_map(|name| store.layers(&name).map(|layers| (name, layers.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tau;
    use crate::storage::{Disk, InMemory, Wal};

    fn db() -> Database<i64> {
        Database::new(InMemory::new())
    }

    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    fn layer(id: u64, items: &[(i64, i64, i64)]) -> Layer<i64> {
        Layer::new(
            id,
            items.iter().map(|&(s, e, v)| Tau::new(s, e, v)).collect(),
        )
    }

    /// Sorted, non-overlapping taus for a `Layer<i64>`.
    fn taus_gen() -> impl Generator<Vec<Tau<i64>>> {
        gs::vecs(
            gs::integers::<i64>()
                .min_value(1)
                .max_value(20)
                .flat_map(|width| {
                    gs::integers::<i64>()
                        .min_value(0)
                        .max_value(10)
                        .flat_map(move |gap| gs::integers::<i64>().map(move |v| (width, gap, v)))
                }),
        )
        .max_size(10)
        .map(|specs| {
            let mut taus = Vec::with_capacity(specs.len());
            let mut cursor: i64 = 0;
            for (width, gap, v) in specs {
                let s = cursor + gap;
                let e = s + width;
                taus.push(Tau::new(s, e, v));
                cursor = e;
            }
            taus
        })
    }

    #[hegel::test]
    fn pbt_base_lookup_matches_layer_lookup(tc: TestCase) {
        let taus = tc.draw(taus_gen());
        let probe = tc.draw(gs::integers::<i64>().min_value(-5).max_value(500));
        let db = db();
        if !taus.is_empty() {
            db.append("x", Layer::new(1, taus.clone())).unwrap();
        }
        let expected = taus.iter().find(|t| t.contains(probe)).map(|t| t.value);
        assert_eq!(db.at_name("x", probe), expected);
    }

    #[hegel::test]
    fn pbt_newest_layer_wins_after_multiple_appends(tc: TestCase) {
        let taus_a = tc.draw(taus_gen().filter(|v| !v.is_empty()));
        let taus_b = tc.draw(taus_gen().filter(|v| !v.is_empty()));
        let probe = tc.draw(gs::integers::<i64>().min_value(-5).max_value(500));
        let db = db();
        db.append("x", Layer::new(1, taus_a.clone())).unwrap();
        db.append("x", Layer::new(2, taus_b.clone())).unwrap();

        let expected = taus_b
            .iter()
            .find(|t| t.contains(probe))
            .map(|t| t.value)
            .or_else(|| taus_a.iter().find(|t| t.contains(probe)).map(|t| t.value));
        assert_eq!(db.at_name("x", probe), expected);
    }

    #[hegel::test]
    fn pbt_database_clone_shares_store(tc: TestCase) {
        let taus = tc.draw(taus_gen().filter(|v| !v.is_empty()));
        let probe = tc.draw(gs::integers::<i64>().min_value(-5).max_value(500));
        let db1 = db();
        let db2 = db1.clone();
        db1.append("x", Layer::new(1, taus.clone())).unwrap();
        let expected = taus.iter().find(|t| t.contains(probe)).map(|t| t.value);
        assert_eq!(db2.at_name("x", probe), expected);
    }

    #[test]
    fn wal_with_disk_backend() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let disk_path = tmp_dir.path().join("store.dat");
        let wal_path = tmp_dir.path().join("test.wal");

        let disk_store = Disk::<i64>::create(&disk_path, None).unwrap();
        let wal = Wal::open(&wal_path, None).unwrap();
        let db = Database::with_wal(disk_store, wal);

        db.append("temp", layer(1, &[(0, 10, 42)])).unwrap();
        assert_eq!(db.at_name("temp", 5), Some(42));
    }

    #[test]
    fn checkpoint_rewrites_wal_after_compaction() {
        use crate::storage::store::COMPACT_THRESHOLD;
        let tmp_dir = tempfile::tempdir().unwrap();
        let wal_path = tmp_dir.path().join("test.wal");

        let store = InMemory::<i64>::new();
        let db = Database::open(store, &wal_path, None).unwrap();

        for i in 0..=(COMPACT_THRESHOLD as i64) {
            db.append("x", layer((i + 1) as u64, &[(i * 10, i * 10 + 10, i)]))
                .unwrap();
        }

        let wal = Wal::open(&wal_path, None).unwrap();
        let ids = wal.data_layer_ids().unwrap();
        assert_eq!(
            ids.len(),
            1,
            "WAL should hold exactly one layer after checkpoint, got {ids:?}"
        );
    }

    #[test]
    fn concurrent_append_and_checkpoint_preserve_all_layers() {
        use std::thread;

        let tmp_dir = tempfile::tempdir().unwrap();
        let wal_path = tmp_dir.path().join("test.wal");

        let store = InMemory::<i64>::new();
        let db = Database::open(store, &wal_path, None).unwrap();
        db.set_auto_checkpoint(false); // isolate the explicit checkpoint below

        const N: i64 = 50;
        let db_appender = db.clone();
        let appender = thread::spawn(move || {
            for i in 0..N {
                db_appender
                    .append("x", layer((i + 1) as u64, &[(i * 10, i * 10 + 10, i)]))
                    .unwrap();
            }
        });

        let db_checkpointer = db.clone();
        let checkpointer = thread::spawn(move || {
            for _ in 0..N {
                db_checkpointer.checkpoint().unwrap();
            }
        });

        appender.join().unwrap();
        checkpointer.join().unwrap();
        db.checkpoint().unwrap(); // final checkpoint to settle the WAL

        // Every appended layer must be queryable...
        for i in 0..N {
            assert_eq!(
                db.at_name("x", i * 10),
                Some(i),
                "layer for t={} missing from in-memory store",
                i * 10
            );
        }

        // ...and recoverable from the WAL after a fresh open (simulated restart).
        drop(db);
        let store2 = InMemory::<i64>::new();
        let db2 = Database::open(store2, &wal_path, None).unwrap();
        for i in 0..N {
            assert_eq!(
                db2.at_name("x", i * 10),
                Some(i),
                "layer for t={} not recovered from WAL after restart",
                i * 10
            );
        }
    }
}
