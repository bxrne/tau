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
