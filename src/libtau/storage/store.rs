use crate::libtau::model::{Layer, Tau, Timestamp};
use std::io;

/// Compact `layers` down to a single layer representing the same newest-wins
/// truth.  Every unique tau boundary becomes a potential split point; the
/// effective value at each sub-interval is the one from the most-recently
/// appended layer that covers it.  Adjacent sub-intervals with equal values
/// are merged.  No-ops when there is already ≤ 1 layer.
///
/// Implementation: sweep-line over `(time, start/end, layer_idx)` events.
/// O(E log E) where E = 2 × total tau count, replacing the older
/// O(B × L) "for every boundary, scan every layer" pass.
pub fn compact_layers<V>(layers: &mut Vec<Layer<V>>)
where
    V: Clone + PartialEq,
{
    if layers.len() <= 1 {
        return;
    }

    let total_taus: usize = layers.iter().map(|l| l.taus.len()).sum();
    if total_taus == 0 {
        layers.clear();
        return;
    }

    let max_id = layers.iter().map(|l| l.id).max().unwrap_or(0);

    // Event kind: false=start (open before close at same t), true=end.
    // Sorting by (time, kind) ensures all starts at t fire before all ends at t,
    // so layers that touch (end of one == start of another) hand off cleanly.
    let mut events: Vec<(Timestamp, bool, usize, usize)> = Vec::with_capacity(total_taus * 2);
    for (layer_idx, layer) in layers.iter().enumerate() {
        for (tau_idx, tau) in layer.taus.iter().enumerate() {
            events.push((tau.start, false, layer_idx, tau_idx));
            events.push((tau.end, true, layer_idx, tau_idx));
        }
    }
    events.sort_unstable_by_key(|e| (e.0, e.1));

    // `active` is a max-heap keyed by layer_idx - newer layer wins.  We tag
    // each entry with `tau_idx` so we can drop the right one on close events
    // when a layer has multiple taus stacked at the same boundary.  Stale
    // entries (already closed) are skipped lazily.
    use std::collections::{BinaryHeap, HashSet};
    let mut active: BinaryHeap<(usize, usize)> = BinaryHeap::new();
    let mut closed: HashSet<(usize, usize)> = HashSet::new();

    let mut merged: Vec<Tau<V>> = Vec::new();
    let mut cursor: Option<Timestamp> = None;

    // Helper: pop stale entries off the top of the heap.
    let drain_stale = |active: &mut BinaryHeap<(usize, usize)>,
                       closed: &mut HashSet<(usize, usize)>| {
        while let Some(&top) = active.peek() {
            if closed.remove(&top) {
                active.pop();
            } else {
                break;
            }
        }
    };

    let mut i = 0;
    while i < events.len() {
        let t = events[i].0;

        // Emit segment [cursor, t) using the value of the top of `active`
        // before applying any events at `t`.
        if let Some(c) = cursor
            && c < t
        {
            drain_stale(&mut active, &mut closed);
            if let Some(&(layer_idx, tau_idx)) = active.peek() {
                let v = layers[layer_idx].taus[tau_idx].value.clone();
                match merged.last_mut() {
                    Some(last) if last.end == c && last.value == v => last.end = t,
                    _ => merged.push(Tau::new(c, t, v)),
                }
            }
        }
        cursor = Some(t);

        // Apply all events at time `t`.  Starts go first (sort order), then ends.
        while i < events.len() && events[i].0 == t {
            let (_, is_end, layer_idx, tau_idx) = events[i];
            if is_end {
                closed.insert((layer_idx, tau_idx));
            } else {
                active.push((layer_idx, tau_idx));
            }
            i += 1;
        }
    }

    *layers = if merged.is_empty() {
        Vec::new()
    } else {
        vec![Layer::new(max_id, merged)]
    };
}

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

    /// Names of all lenses that have at least one layer.  Used by the WAL
    /// checkpoint to enumerate live data for compaction.  Default returns an
    /// empty vec; backends should override.
    fn lens_names(&self) -> Vec<String> {
        Vec::new()
    }
}
