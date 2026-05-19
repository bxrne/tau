use crate::libtau::model::{Layer, Tau, Timestamp};

/// Compact `layers` down to a single layer representing the same newest-wins
/// truth.  Every unique tau boundary becomes a potential split point; the
/// effective value at each sub-interval is the one from the most-recently
/// appended layer that covers it.  Adjacent sub-intervals with equal values
/// are merged.  No-ops when there is already ≤ 1 layer.
pub fn compact_layers<V>(layers: &mut Vec<Layer<V>>)
where
    V: Clone + PartialEq,
{
    if layers.len() <= 1 {
        return;
    }

    let mut bounds: Vec<Timestamp> = layers
        .iter()
        .flat_map(|l| l.taus.iter().flat_map(|t| [t.start, t.end]))
        .collect();
    bounds.sort_unstable();
    bounds.dedup();

    if bounds.len() < 2 {
        layers.clear();
        return;
    }

    let max_id = layers.iter().map(|l| l.id).max().unwrap_or(0);

    let mut merged: Vec<Tau<V>> = Vec::new();
    for w in bounds.windows(2) {
        let (s, e) = (w[0], w[1]);
        if let Some(v) = layers.iter().rev().find_map(|l| l.at(s)).cloned() {
            match merged.last_mut() {
                Some(last) if last.end == s && last.value == v => last.end = e,
                _ => merged.push(Tau::new(s, e, v)),
            }
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
    /// Push a new layer onto the named lens.
    fn append(&mut self, lens: &str, layer: Layer<V>);

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
}
