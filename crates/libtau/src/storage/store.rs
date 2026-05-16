use crate::model::{Layer, Timestamp};
use std::io;

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

    /// Expose the raw layer stack (e.g. for inspection / snapshotting).
    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>>;

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
}
