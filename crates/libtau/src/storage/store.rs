use crate::model::{Layer, LayerId, Timestamp};
use std::io;
use std::sync::Arc;

/// Number of layers per lens that triggers an automatic compaction.
pub const COMPACT_THRESHOLD: usize = 8;

/// Per-layer metadata surfaced by [`Store::layer_infos`] for `HISTORY`.
/// `(id, written_at, min_start, max_end, tau_count)`.
pub type LayerInfoTuple = (LayerId, i64, Timestamp, Timestamp, usize);

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

    /// N-dimensional point lookup with MVCC: the newest layer (written at or
    /// before `as_of` when `Some`) whose tau contains every coordinate wins.
    /// `coords` has one entry per axis (axis 0 is valid time). The default
    /// implementation resolves over [`Store::layers`]; on-disk backends override
    /// it to answer without materialising the stack.
    fn get(&self, lens: &str, coords: &[Timestamp], as_of: Option<i64>) -> Option<V> {
        let layers = self.layers(lens)?;
        layers
            .iter()
            .rev()
            .filter(|l| as_of.is_none_or(|a| l.written_at <= a))
            .find_map(|l| {
                if coords.len() == 1 {
                    l.at(coords[0])
                } else {
                    l.at_nd(coords)
                }
            })
            .cloned()
    }

    /// Valid-axis range scan over `[start, end)` with every non-valid axis fixed
    /// at `fixed` (empty for single-axis lenses), resolved newest-wins (MVCC,
    /// `as_of`-scoped when `Some`) into adjacent-merged `(start, end, value)`
    /// segments. Default implementation sweeps [`Store::layers`].
    fn scan(
        &self,
        lens: &str,
        start: Timestamp,
        end: Timestamp,
        fixed: &[Timestamp],
        as_of: Option<i64>,
    ) -> Vec<(Timestamp, Timestamp, V)>
    where
        V: PartialEq,
    {
        let Some(layers) = self.layers(lens) else {
            return Vec::new();
        };
        if start >= end {
            return Vec::new();
        }
        let arity = fixed.len() + 1;
        let filtered: Vec<Layer<V>> = layers
            .iter()
            .filter(|l| as_of.is_none_or(|a| l.written_at <= a))
            .map(|l| {
                if fixed.is_empty() {
                    l.clone()
                } else {
                    let taus = l
                        .taus
                        .iter()
                        .filter(|t| {
                            t.coords.len() == arity
                                && t.coords[1..]
                                    .iter()
                                    .zip(fixed)
                                    .all(|(b, &p)| b.lo <= p && p < b.hi)
                        })
                        .cloned()
                        .collect();
                    Layer::new_sorted_unchecked(l.id, taus, l.written_at)
                }
            })
            .collect();
        crate::storage::layers::sweep_range(&filtered, start, end)
            .into_iter()
            .map(|t| (t.start(), t.end(), t.value))
            .collect()
    }

    /// Per-layer metadata for `HISTORY`, newest last. Default reads
    /// [`Store::layers`]; on-disk backends override.
    fn layer_infos(&self, lens: &str) -> Vec<LayerInfoTuple> {
        self.layers(lens)
            .map(|ls| {
                ls.iter()
                    .map(|l| (l.id, l.written_at, l.min_start, l.max_end, l.taus.len()))
                    .collect()
            })
            .unwrap_or_default()
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
