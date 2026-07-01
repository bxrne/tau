use crate::model::{Layer, Timestamp};
use std::io;
use std::sync::Arc;

/// Number of layers per lens that triggers an automatic compaction.
pub const COMPACT_THRESHOLD: usize = 8;

/// Pluggable backing storage for layered temporal data.
///
/// Implementors are responsible for the layer stack and newest-wins semantics.
pub trait Store<V>: Send + Sync
where
    V: Clone + Send + Sync + 'static,
{
    /// Push a new layer onto the named lens.  Returns `true` when the
    /// post-append state has fewer layers than `prior+1`, i.e. compaction ran.
    ///
    /// Returns an error if the underlying storage write fails.  Callers must
    /// treat a returned error as meaning the layer was **not** persisted; they
    /// should not update any higher-level state after receiving one.
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool>;

    /// Point lookup: newest layer wins, returns cloned value.
    fn at(&self, lens: &str, t: Timestamp) -> Option<V> {
        self.layers(lens)?
            .iter()
            .rev()
            .find_map(|layer| layer.at(t))
            .cloned()
    }

    /// Snapshot the layer stack for `lens` as a shared `Arc<[Layer]>`.
    ///
    /// The per-lens stack is stored behind an `Arc`, so a snapshot is a single
    /// pointer bump — no vector clone. Appends replace the stack wholesale
    /// (copy-on-write / RCU), so a snapshot taken before an append stays valid
    /// and consistent for its whole lifetime.
    fn layers(&self, lens: &str) -> Option<Arc<[Layer<V>]>>;

    /// Remove all layers for `lens`, as if the lens never existed.
    ///
    /// Called by the executor when `DROP LENS` is executed so that recreating
    /// the same lens name starts from a clean state.  Default is a no-op;
    /// implementors should override to actually free the storage.
    fn drop_lens(&mut self, _lens: &str) {}

    /// Names of all lenses that have at least one layer.  Used by the WAL
    /// checkpoint to enumerate live data for compaction.  Default returns an
    /// empty vec; backends should override.
    fn lens_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Durably record a schema DDL statement (e.g. `CREATE LENS temp int`) so
    /// it can be replayed on restart.
    ///
    /// Only backends that own their own on-disk file need this; for
    /// WAL-backed setups schema persistence lives in the WAL and this is never
    /// called.  The default is a no-op (in-memory stores keep no schema).
    fn append_schema(&mut self, _stmt: &str) -> io::Result<()> {
        Ok(())
    }

    /// Return the persisted schema DDL statements in write order.  Default is
    /// empty; backends that persist schema override this.
    fn schema_stmts(&self) -> Vec<String> {
        Vec::new()
    }

    /// Persist the full current state to the backend's own durable file, if
    /// it has one.  Returns `Ok(true)` when a full rewrite happened, meaning
    /// any WAL entries written before this call are now redundant and can be
    /// dropped on checkpoint.  Backends with no separate file (in-memory,
    /// WAL-only) return `Ok(false)` and leave the WAL as the source of truth.
    fn checkpoint_flush(&self) -> io::Result<bool> {
        Ok(false)
    }
}
