use crate::libtau::model::{Layer, Timestamp};

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
    fn at(&self, lens: &str, t: Timestamp) -> Option<V>;

    /// Expose the raw layer stack (e.g. for inspection / snapshotting).
    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>>;

    /// Replace every layer of `lens` with the single provided layer.
    /// Used by `COMPACT LENS` to collapse a layer stack into one canonical
    /// newest-wins layer.  Default impl assumes a simple Vec-backed store.
    fn replace_layers(&mut self, lens: &str, layer: Layer<V>) {
        let _ = (lens, layer);
        unimplemented!("replace_layers not supported by this store");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libtau::model::Tau;

    /// Minimal in-process Store used only to verify the trait contract.
    struct Trivial<V> {
        data: Vec<(String, Layer<V>)>,
    }

    impl<V> Trivial<V> {
        fn new() -> Self {
            Self { data: Vec::new() }
        }
    }

    impl<V> Store<V> for Trivial<V>
    where
        V: Clone + Send + Sync + 'static,
    {
        fn append(&mut self, lens: &str, layer: Layer<V>) {
            self.data.push((lens.to_string(), layer));
        }

        fn at(&self, lens: &str, t: Timestamp) -> Option<V> {
            // newest-wins: iterate in reverse
            self.data
                .iter()
                .rev()
                .filter(|(name, _)| name == lens)
                .find_map(|(_, layer)| layer.at(t))
                .cloned()
        }

        fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>> {
            // Trivial impl does not support this; always None.
            let _ = lens;
            None
        }
    }

    fn layer(id: u64, items: &[(i64, i64, i32)]) -> Layer<i32> {
        Layer::new(
            id,
            items.iter().map(|&(s, e, v)| Tau::new(s, e, v)).collect(),
        )
    }

    #[test]
    fn append_and_at_round_trip() {
        let mut store = Trivial::new();
        store.append("a", layer(1, &[(0, 10, 42)]));
        assert_eq!(store.at("a", 5), Some(42));
    }

    #[test]
    fn at_returns_none_before_any_append() {
        let store: Trivial<i32> = Trivial::new();
        assert_eq!(store.at("a", 5), None);
    }

    #[test]
    fn at_returns_none_for_unknown_lens() {
        let mut store = Trivial::new();
        store.append("a", layer(1, &[(0, 10, 1)]));
        assert_eq!(store.at("b", 5), None);
    }

    #[test]
    fn at_returns_none_outside_any_tau() {
        let mut store = Trivial::new();
        store.append("a", layer(1, &[(0, 10, 99)]));
        assert_eq!(store.at("a", 10), None); // exclusive end
        assert_eq!(store.at("a", 20), None);
    }

    #[test]
    fn newest_layer_shadows_older() {
        let mut store = Trivial::new();
        store.append("a", layer(1, &[(0, 20, 1)]));
        store.append("a", layer(2, &[(5, 15, 2)]));
        assert_eq!(store.at("a", 3), Some(1)); // only layer 1
        assert_eq!(store.at("a", 7), Some(2)); // layer 2 wins
        assert_eq!(store.at("a", 17), Some(1)); // only layer 1 again
    }

    #[test]
    fn multiple_lenses_are_independent() {
        let mut store = Trivial::new();
        store.append("a", layer(1, &[(0, 10, 1)]));
        store.append("b", layer(2, &[(0, 10, 2)]));
        assert_eq!(store.at("a", 5), Some(1));
        assert_eq!(store.at("b", 5), Some(2));
    }

    #[test]
    fn layers_returns_none_for_trivial_impl() {
        let mut store = Trivial::new();
        store.append("a", layer(1, &[(0, 10, 0)]));
        assert!(store.layers("a").is_none());
    }
}
