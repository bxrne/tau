use crate::libtau::model::{Eval, Layer, LayerId, Lens, LensKind, Timestamp};
use crate::libtau::storage::wal::Codec;
use crate::libtau::storage::{Store, Wal, WalEntry};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
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
}

impl<V> Clone for Database<V>
where
    V: Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            wal: self.wal.clone(),
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
        }
    }

    /// Attach a WAL for durability.  Every subsequent `append` is written to
    /// the log before the store is updated.
    pub fn with_wal(store: impl Store<V> + 'static, wal: Wal) -> Self {
        info!("creating database with WAL enabled");
        Self {
            store: Arc::new(RwLock::new(Box::new(store))),
            wal: Some(Arc::new(Mutex::new(wal))),
        }
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

    /// Return a handle (base `Lens`) for a named series.
    /// The lens is cheap — it holds only the name and a marker.
    pub fn lens(&self, name: impl Into<Arc<str>>) -> Lens<V> {
        Lens {
            name: name.into(),
            kind: LensKind::Base,
        }
    }

    /// Append a `Layer` to a base lens.
    ///
    /// Write-ahead semantics: the entry is fsynced to the WAL (if enabled)
    /// before the in-memory store is updated.  Returns an error if the WAL
    /// write fails; the in-memory store is **not** updated in that case.
    ///
    /// After the store update, if auto-compaction reduced the layer count, a
    /// WAL checkpoint is triggered that rewrites the WAL to contain only the
    /// live (post-compaction) layers, bounding WAL growth.
    ///
    /// Panics if `lens` is derived — derived lenses are views, not stores.
    pub fn append(&self, lens: &Lens<V>, layer: Layer<V>) -> io::Result<()>
    where
        V: Codec,
    {
        match lens.kind {
            LensKind::Base => {
                debug!(
                    lens = %lens.name,
                    layer_id = layer.id,
                    tau_count = layer.taus.len(),
                    "appending layer"
                );
                // WAL first — do not update in-memory state if this fails.
                if let Some(wal) = &self.wal {
                    let entry = WalEntry {
                        layer_id: layer.id,
                        lens: lens.name.to_string(),
                        taus: layer
                            .taus
                            .iter()
                            .map(|t| (t.start, t.end, t.value.clone()))
                            .collect(),
                    };
                    wal.lock()
                        .map_err(|_| io::Error::other("WAL mutex poisoned"))?
                        .append(&entry)?;
                    debug!(lens = %lens.name, layer_id = layer.id, "WAL entry written");
                }

                // Then store — detect whether auto-compaction fired.
                let did_compact = {
                    let mut store = self
                        .store
                        .write()
                        .map_err(|_| io::Error::other("store lock poisoned"))?;
                    let before = store.layers(&lens.name).map(|l| l.len()).unwrap_or(0);
                    store.append(&lens.name, layer)?;
                    let after = store.layers(&lens.name).map(|l| l.len()).unwrap_or(0);
                    after < before + 1
                }; // store write-lock released here

                if did_compact {
                    self.checkpoint()?;
                }

                Ok(())
            }
            LensKind::Derived(_) => {
                warn!(lens = %lens.name, "attempted append to derived lens");
                panic!("cannot append to a derived lens");
            }
        }
    }

    /// Write a schema DDL statement (e.g. `CREATE LENS temp int`) to the WAL.
    ///
    /// No-op when no WAL is configured.
    pub fn append_schema(&self, stmt_text: &str) -> io::Result<()> {
        if let Some(wal) = &self.wal {
            wal.lock()
                .map_err(|_| io::Error::other("WAL mutex poisoned"))?
                .append_schema(stmt_text)?;
        }
        Ok(())
    }

    /// Return the schema DDL statements stored in the WAL (in write order).
    ///
    /// Returns an empty vec when no WAL is configured.
    pub fn schema_stmts(&self) -> io::Result<Vec<String>> {
        match &self.wal {
            Some(wal) => wal
                .lock()
                .map_err(|_| io::Error::other("WAL mutex poisoned"))?
                .replay_schemas(),
            None => Ok(Vec::new()),
        }
    }

    /// Highest layer ID currently in the store across all lenses.
    ///
    /// Used during WAL-replay startup to restore `next_layer_id` so new
    /// layers never collide with replayed ones.
    pub fn max_layer_id(&self) -> LayerId {
        let store = self.store.read().unwrap();
        store
            .lens_names()
            .iter()
            .filter_map(|name| store.layers(name))
            .flat_map(|layers| layers.iter().map(|l| l.id))
            .max()
            .unwrap_or(0)
    }

    /// Rewrite the WAL to contain only the current live layers plus any
    /// previously written schema lines, discarding entries superseded by
    /// auto-compaction.
    ///
    /// Called automatically after compaction.  No-op when no WAL is
    /// configured.
    pub fn checkpoint(&self) -> io::Result<()>
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
                    store.layers(&name).map(|layers| {
                        layers.iter().map(move |layer| WalEntry {
                            layer_id: layer.id,
                            lens: name.clone(),
                            taus: layer
                                .taus
                                .iter()
                                .map(|t| (t.start, t.end, t.value.clone()))
                                .collect(),
                        })
                    })
                })
                .flatten()
                .collect()
        }; // store read-lock released

        let mut wal_guard = wal
            .lock()
            .map_err(|_| io::Error::other("WAL mutex poisoned"))?;
        let schema_lines = wal_guard.raw_schema_lines()?;
        wal_guard.rewrite(&schema_lines, &entries)?;
        debug!("WAL checkpoint complete");
        Ok(())
    }

    /// Point lookup at timestamp `t`.
    ///
    /// - Base: delegates to store (newest layer wins).
    /// - Derived: calls the stored closure.
    pub fn at(&self, lens: &Lens<V>, t: Timestamp) -> Option<V> {
        let result = match &lens.kind {
            LensKind::Base => self
                .store
                .read()
                .expect("store lock poisoned")
                .at(&lens.name, t),
            LensKind::Derived(f) => f(t),
        };
        debug!(lens = %lens.name, t, found = result.is_some(), "point lookup");
        result
    }

    /// Snapshot of the layer stack for `lens`, cheap-cloned through the
    /// `Arc`-backed `Layer`.  Returns `None` if the lens has never been
    /// appended to.  Used by the QL executor to gather change boundaries
    /// when materialising a `RANGE`.
    pub fn layers(&self, lens: &str) -> Option<Vec<Layer<V>>> {
        self.store
            .read()
            .expect("store lock poisoned")
            .layers(lens)
            .cloned()
    }

    /// Pure value transform: `Out = f(V)`.
    ///
    /// The returned `Lens<Out>` is derived and computed lazily per query.
    pub fn map<Out>(&self, src: Lens<V>, f: impl Fn(V) -> Out + Send + Sync + 'static) -> Lens<Out>
    where
        Out: Clone + Send + Sync + 'static,
    {
        let db = self.clone();
        let name = src.name.clone();
        let eval: Eval<Out> = Arc::new(move |t| db.at(&src, t).map(&f));
        Lens {
            name,
            kind: LensKind::Derived(eval),
        }
    }

    /// Timestamp-aware transform: `Out = f(t, V)`.
    ///
    /// Useful for derivatives, windowed deltas, and any computation that needs
    /// to sample the series at a different time.
    pub fn adapt<Out>(
        &self,
        src: Lens<V>,
        f: impl Fn(Timestamp, V) -> Out + Send + Sync + 'static,
    ) -> Lens<Out>
    where
        Out: Clone + Send + Sync + 'static,
    {
        let db = self.clone();
        let name = src.name.clone();
        let eval: Eval<Out> = Arc::new(move |t| db.at(&src, t).map(|v| f(t, v)));
        Lens {
            name,
            kind: LensKind::Derived(eval),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libtau::model::Tau;
    use crate::libtau::storage::{Disk, InMemory, Wal};

    fn db() -> Database<i64> {
        Database::new(InMemory::new())
    }

    fn layer(id: u64, items: &[(i64, i64, i64)]) -> Layer<i64> {
        Layer::new(
            id,
            items.iter().map(|&(s, e, v)| Tau::new(s, e, v)).collect(),
        )
    }

    #[test]
    fn base_lookup_returns_none_when_empty() {
        let db = db();
        let l = db.lens("x");
        assert_eq!(db.at(&l, 5), None);
    }

    #[test]
    fn base_lookup_after_append() {
        let db = db();
        let l = db.lens("x");
        db.append(&l, layer(1, &[(0, 10, 42)])).unwrap();
        assert_eq!(db.at(&l, 5), Some(42));
        assert_eq!(db.at(&l, 10), None);
    }

    #[test]
    fn newest_layer_shadows_older() {
        let db = db();
        let l = db.lens("x");
        db.append(&l, layer(1, &[(0, 20, 1)])).unwrap();
        db.append(&l, layer(2, &[(5, 15, 2)])).unwrap();
        assert_eq!(db.at(&l, 3), Some(1)); // only layer 1
        assert_eq!(db.at(&l, 7), Some(2)); // layer 2 wins
        assert_eq!(db.at(&l, 17), Some(1)); // only layer 1 again
    }

    #[test]
    fn map_transforms_value() {
        let db = db();
        let l = db.lens("x");
        db.append(&l, layer(1, &[(0, 10, 5)])).unwrap();
        let doubled = db.map(l, |v| v * 2);
        assert_eq!(db.at(&doubled, 5), Some(10));
    }

    #[test]
    fn map_composes() {
        let db = db();
        let l = db.lens("x");
        db.append(&l, layer(1, &[(0, 10, 5)])).unwrap();
        let step1 = db.map(l, |v| v * 2);
        let step2 = db.map(step1, |v| v + 10);
        assert_eq!(db.at(&step2, 5), Some(20));
    }

    #[test]
    fn adapt_receives_timestamp() {
        let db = db();
        let l = db.lens("x");
        db.append(&l, layer(1, &[(0, 20, 100)])).unwrap();
        let echo_t = db.adapt(l, |t, _v| t);
        assert_eq!(db.at(&echo_t, 7), Some(7));
        assert_eq!(db.at(&echo_t, 13), Some(13));
    }

    #[test]
    fn database_clone_shares_store() {
        let db1 = db();
        let db2 = db1.clone();
        let l = db1.lens("x");
        db1.append(&l, layer(1, &[(0, 10, 99)])).unwrap();
        let l2 = db2.lens("x");
        assert_eq!(db2.at(&l2, 5), Some(99));
    }

    #[test]
    #[should_panic(expected = "cannot append to a derived lens")]
    fn append_to_derived_panics() {
        let db = db();
        let l = db.lens("x");
        db.append(&l, layer(1, &[(0, 10, 1)])).unwrap();
        let derived = db.map(l, |v| v);
        db.append(&derived, layer(2, &[(0, 10, 2)])).unwrap();
    }

    #[test]
    fn wal_with_disk_backend() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let disk_path = tmp_dir.path().join("store.dat");
        let wal_path = tmp_dir.path().join("test.wal");

        let disk_store = Disk::<i64>::create(&disk_path, None).unwrap();
        let wal = Wal::open(&wal_path, None).unwrap();
        let db = Database::with_wal(disk_store, wal);

        let sensor = db.lens("temp");
        db.append(&sensor, layer(1, &[(0, 10, 42)])).unwrap();
        assert_eq!(db.at(&sensor, 5), Some(42));

        let store_data = db.store.read().unwrap().at("temp", 5);
        assert_eq!(store_data, Some(42));
    }

    #[test]
    fn checkpoint_rewrites_wal_after_compaction() {
        use crate::libtau::storage::store::COMPACT_THRESHOLD;
        let tmp_dir = tempfile::tempdir().unwrap();
        let wal_path = tmp_dir.path().join("test.wal");

        let store = InMemory::<i64>::new();
        let db = Database::open(store, &wal_path, None).unwrap();
        let l = db.lens("x");

        // Append enough layers to trigger compaction (COMPACT_THRESHOLD + 1).
        for i in 0..=(COMPACT_THRESHOLD as i64) {
            db.append(&l, layer((i + 1) as u64, &[(i * 10, i * 10 + 10, i)]))
                .unwrap();
        }

        // WAL should now contain a single compacted entry (one layer ID remains).
        let wal = Wal::open(&wal_path, None).unwrap();
        let ids = wal.data_layer_ids().unwrap();
        assert_eq!(
            ids.len(),
            1,
            "WAL should hold exactly one layer after checkpoint, got {:?}",
            ids
        );
    }
}
