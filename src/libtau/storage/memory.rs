use crate::libtau::model::Layer;
use crate::libtau::storage::store::{COMPACT_THRESHOLD, Store, compact_layers};
use std::collections::HashMap;
use std::io;

/// Reference in-memory store. Zero dependencies, suitable for tests and
/// small embedded workloads.
pub struct InMemory<V> {
    lenses: HashMap<String, Vec<Layer<V>>>,
    compact_threshold: usize,
}

impl<V> Default for InMemory<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> InMemory<V> {
    pub fn new() -> Self {
        Self::with_threshold(COMPACT_THRESHOLD)
    }

    pub fn with_threshold(compact_threshold: usize) -> Self {
        Self {
            lenses: HashMap::new(),
            compact_threshold,
        }
    }
}

impl<V> Store<V> for InMemory<V>
where
    V: Clone + PartialEq + Send + Sync + 'static,
{
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool> {
        // Hot path: lens already known — borrow without allocating a String.
        let layers = if let Some(l) = self.lenses.get_mut(lens) {
            l
        } else {
            self.lenses.entry(lens.to_string()).or_default()
        };
        let before = layers.len();
        layers.push(layer);
        let mut did_compact = false;
        if layers.len() > self.compact_threshold {
            compact_layers(layers);
            did_compact = layers.len() < before + 1;
        }
        Ok(did_compact)
    }

    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>> {
        self.lenses.get(lens)
    }

    fn lens_names(&self) -> Vec<String> {
        self.lenses.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libtau::model::Tau;

    fn layer(id: u64, items: &[(i64, i64, i32)]) -> Layer<i32> {
        Layer::new(
            id,
            items.iter().map(|&(s, e, v)| Tau::new(s, e, v)).collect(),
        )
    }

    #[test]
    fn new_and_default_produce_empty_store() {
        let a: InMemory<i32> = InMemory::new();
        let b: InMemory<i32> = InMemory::default();
        assert_eq!(a.at("x", 0), None);
        assert_eq!(b.at("x", 0), None);
    }

    #[test]
    fn append_then_at_returns_value() {
        let mut store = InMemory::new();
        store.append("temp", layer(1, &[(0, 10, 42)])).unwrap();
        assert_eq!(store.at("temp", 0), Some(42));
        assert_eq!(store.at("temp", 9), Some(42));
    }

    #[test]
    fn at_exclusive_end_returns_none() {
        let mut store = InMemory::new();
        store.append("temp", layer(1, &[(0, 10, 1)])).unwrap();
        assert_eq!(store.at("temp", 10), None);
    }

    #[test]
    fn at_returns_none_for_unknown_lens() {
        let store: InMemory<i32> = InMemory::new();
        assert_eq!(store.at("missing", 5), None);
    }

    #[test]
    fn at_returns_none_in_gap_between_taus() {
        let mut store = InMemory::new();
        store
            .append("s", layer(1, &[(0, 5, 10), (10, 20, 20)]))
            .unwrap();
        assert_eq!(store.at("s", 5), None);
        assert_eq!(store.at("s", 7), None);
    }

    #[test]
    fn newest_layer_shadows_older_layer() {
        let mut store = InMemory::new();
        store.append("s", layer(1, &[(0, 20, 1)])).unwrap();
        store.append("s", layer(2, &[(5, 15, 2)])).unwrap();
        assert_eq!(store.at("s", 3), Some(1)); // only layer 1
        assert_eq!(store.at("s", 7), Some(2)); // layer 2 shadows
        assert_eq!(store.at("s", 17), Some(1)); // only layer 1 again
    }

    #[test]
    fn multiple_lenses_stored_independently() {
        let mut store = InMemory::new();
        store.append("a", layer(1, &[(0, 10, 1)])).unwrap();
        store.append("b", layer(2, &[(0, 10, 2)])).unwrap();
        assert_eq!(store.at("a", 5), Some(1));
        assert_eq!(store.at("b", 5), Some(2));
    }

    #[test]
    fn layers_returns_none_for_unknown_lens() {
        let store: InMemory<i32> = InMemory::new();
        assert!(store.layers("missing").is_none());
    }

    #[test]
    fn layers_returns_all_appended_layers_in_order() {
        let mut store = InMemory::new();
        store.append("s", layer(1, &[(0, 5, 10)])).unwrap();
        store.append("s", layer(2, &[(5, 10, 20)])).unwrap();
        let layers = store.layers("s").unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].id, 1);
        assert_eq!(layers[1].id, 2);
    }

    #[test]
    fn append_creates_lens_entry_on_first_insert() {
        let mut store: InMemory<i32> = InMemory::new();
        assert!(store.layers("new").is_none());
        store.append("new", layer(1, &[(0, 1, 0)])).unwrap();
        assert!(store.layers("new").is_some());
    }

    #[test]
    fn compaction_triggers_after_threshold() {
        use crate::libtau::storage::store::COMPACT_THRESHOLD;
        let mut store: InMemory<i32> = InMemory::new();
        // Push COMPACT_THRESHOLD + 1 non-overlapping layers to trigger compaction.
        for i in 0..(COMPACT_THRESHOLD + 1) as i64 {
            store
                .append("s", layer(i as u64 + 1, &[(i * 10, i * 10 + 10, i as i32)]))
                .unwrap();
        }
        // After compaction the lens has exactly one layer.
        assert_eq!(store.layers("s").unwrap().len(), 1);
    }

    #[test]
    fn compaction_preserves_newest_wins_semantics() {
        let mut store: InMemory<i32> = InMemory::new();
        // Layer 1: [0,20) = 1
        store.append("s", layer(1, &[(0, 20, 1)])).unwrap();
        // Layer 2: [5,15) = 2  (shadows middle of layer 1)
        store.append("s", layer(2, &[(5, 15, 2)])).unwrap();
        // Push enough extra layers to trigger compaction.
        for i in 3..=10 {
            store
                .append("s", layer(i, &[(100, 101, i as i32)]))
                .unwrap();
        }
        // Compaction must have happened; query the original range.
        assert_eq!(store.at("s", 3), Some(1));
        assert_eq!(store.at("s", 7), Some(2));
        assert_eq!(store.at("s", 17), Some(1));
    }

    #[test]
    fn compaction_merges_adjacent_equal_value_taus() {
        let mut store: InMemory<i32> = InMemory::new();
        // Two consecutive layers with the same value → should merge into one tau.
        for i in 0..=(COMPACT_THRESHOLD as i64) {
            store
                .append("s", layer(i as u64 + 1, &[(i * 5, i * 5 + 5, 42)]))
                .unwrap();
        }
        let layers = store.layers("s").unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(
            layers[0].taus.len(),
            1,
            "adjacent equal-value segments should merge"
        );
    }
}
