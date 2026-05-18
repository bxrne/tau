use crate::libtau::model::{Layer, Timestamp};
use crate::libtau::storage::store::Store;
use std::collections::HashMap;

/// Reference in-memory store. Zero dependencies, suitable for tests and
/// small embedded workloads.
pub struct InMemory<V> {
    lenses: HashMap<String, Vec<Layer<V>>>,
}

impl<V> Default for InMemory<V> {
    fn default() -> Self {
        Self {
            lenses: HashMap::new(),
        }
    }
}

impl<V> InMemory<V> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<V> Store<V> for InMemory<V>
where
    V: Clone + Send + Sync + 'static,
{
    fn append(&mut self, lens: &str, layer: Layer<V>) {
        let layers = self.lenses.entry(lens.to_string()).or_default();
        let pos = layers.partition_point(|l| l.id < layer.id);
        layers.insert(pos, layer);
    }

    /// Newest layer wins: iterate in reverse, return first hit.
    fn at(&self, lens: &str, t: Timestamp) -> Option<V> {
        self.lenses
            .get(lens)?
            .iter()
            .rev()
            .filter(|l| l.min_ts <= t && t < l.max_ts)
            .find_map(|layer| layer.at(t))
            .cloned()
    }

    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>> {
        self.lenses.get(lens)
    }

    fn replace_layers(&mut self, lens: &str, layer: Layer<V>) {
        self.lenses.insert(lens.to_string(), vec![layer]);
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
        store.append("temp", layer(1, &[(0, 10, 42)]));
        assert_eq!(store.at("temp", 0), Some(42));
        assert_eq!(store.at("temp", 9), Some(42));
    }

    #[test]
    fn at_exclusive_end_returns_none() {
        let mut store = InMemory::new();
        store.append("temp", layer(1, &[(0, 10, 1)]));
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
        store.append("s", layer(1, &[(0, 5, 10), (10, 20, 20)]));
        assert_eq!(store.at("s", 5), None);
        assert_eq!(store.at("s", 7), None);
    }

    #[test]
    fn newest_layer_shadows_older_layer() {
        let mut store = InMemory::new();
        store.append("s", layer(1, &[(0, 20, 1)]));
        store.append("s", layer(2, &[(5, 15, 2)]));
        assert_eq!(store.at("s", 3), Some(1)); // only layer 1
        assert_eq!(store.at("s", 7), Some(2)); // layer 2 shadows
        assert_eq!(store.at("s", 17), Some(1)); // only layer 1 again
    }

    #[test]
    fn multiple_lenses_stored_independently() {
        let mut store = InMemory::new();
        store.append("a", layer(1, &[(0, 10, 1)]));
        store.append("b", layer(2, &[(0, 10, 2)]));
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
        store.append("s", layer(1, &[(0, 5, 10)]));
        store.append("s", layer(2, &[(5, 10, 20)]));
        let layers = store.layers("s").unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].id, 1);
        assert_eq!(layers[1].id, 2);
    }

    #[test]
    fn append_creates_lens_entry_on_first_insert() {
        let mut store: InMemory<i32> = InMemory::new();
        assert!(store.layers("new").is_none());
        store.append("new", layer(1, &[(0, 1, 0)]));
        assert!(store.layers("new").is_some());
    }
}
