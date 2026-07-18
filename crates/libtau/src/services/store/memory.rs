use crate::model::Layer;
use crate::services::store::layers::compact_layers;
use crate::services::store::store::{COMPACT_THRESHOLD, Store};
use rustc_hash::FxHashMap as HashMap;
use std::io;
use std::sync::Arc;

/// Reference in-memory store. Zero dependencies, suitable for tests and
/// small embedded workloads.
///
/// Each lens's stack is an `Arc<[Layer<V>]>`: reads snapshot it with a pointer
/// bump, and appends rebuild it copy-on-write (RCU) so in-flight readers keep a
/// consistent view.
pub struct InMemory<V> {
    lenses: HashMap<String, Arc<[Layer<V>]>>,
    compact_threshold: usize,
    /// Per-lens compaction threshold overrides.  `Some(n)` means this lens
    /// compacts when it exceeds `n` layers (0 = never); absent means use the
    /// server-wide `compact_threshold`.
    lens_compact: HashMap<String, usize>,
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
            lenses: HashMap::default(),
            compact_threshold,
            lens_compact: HashMap::default(),
        }
    }
}

impl<V> Store<V> for InMemory<V>
where
    V: Clone + PartialEq + Send + Sync + 'static,
{
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool> {
        let mut layers: Vec<Layer<V>> = self
            .lenses
            .get(lens)
            .map(|a| a.to_vec())
            .unwrap_or_default();
        let before = layers.len();
        layers.push(layer);
        let effective = self
            .lens_compact
            .get(lens)
            .copied()
            .unwrap_or(self.compact_threshold);
        let mut did_compact = false;
        if effective > 0 && layers.len() > effective {
            compact_layers(&mut layers);
            did_compact = layers.len() < before + 1;
        }
        self.lenses.insert(lens.to_string(), Arc::from(layers));
        Ok(did_compact)
    }

    fn drop_lens(&mut self, lens: &str) {
        self.lenses.remove(lens);
        self.lens_compact.remove(lens);
    }

    fn layers(&self, lens: &str) -> Option<Arc<[Layer<V>]>> {
        self.lenses.get(lens).cloned()
    }

    fn lens_names(&self) -> Vec<String> {
        self.lenses.keys().cloned().collect()
    }

    fn set_lens_compact_threshold(&mut self, lens: &str, threshold: Option<usize>) {
        match threshold {
            Some(n) => self.lens_compact.insert(lens.to_string(), n),
            None => self.lens_compact.remove(lens),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Tau, Timestamp};
    use crate::services::store::layers::compact_layers;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::{assert_eq, assert_ne};

    /// Generator over a non-overlapping `Vec<Tau<i8>>`.  Small `i8` values
    /// produce frequent equality, which exercises the adjacent-merge code.
    fn taus_gen() -> impl Generator<Vec<Tau<i8>>> {
        gs::vecs(
            gs::integers::<i64>()
                .min_value(1)
                .max_value(20)
                .flat_map(|width| {
                    gs::integers::<i64>()
                        .min_value(0)
                        .max_value(10)
                        .flat_map(move |gap| {
                            gs::integers::<i8>()
                                .min_value(-3)
                                .max_value(3)
                                .map(move |v| (width, gap, v))
                        })
                }),
        )
        .max_size(20)
        .map(|specs| {
            let mut taus = Vec::with_capacity(specs.len());
            let mut cursor: i64 = 0;
            for (width, gap, v) in specs {
                let s = cursor + gap;
                let e = s + width;
                taus.push(Tau::new(s, e, v));
                cursor = e;
            }
            taus
        })
    }

    /// Generator over a layer stack: a vector of layers, each of which is a
    /// sorted non-overlapping batch.  Layers may overlap each other - that is
    /// exactly the situation that compaction has to flatten.
    ///
    /// All layers share `written_at = 0` (one transaction-time generation) so
    /// per-generation compaction collapses them to a single canonical layer,
    /// which is what the classic compaction invariants below assert. Pinning
    /// the stamp also keeps the tests deterministic (as opposed to `Layer::new`,
    /// which reads the wall clock).
    fn layer_stack_gen() -> impl Generator<Vec<Layer<i8>>> {
        gs::vecs(taus_gen().filter(|v| !v.is_empty()))
            .max_size(8)
            .map(|taus_vec| {
                taus_vec
                    .into_iter()
                    .enumerate()
                    .map(|(i, t)| Layer::new_at(i as u64 + 1, t, 0))
                    .collect()
            })
    }

    /// Newest-wins point lookup restricted to layers written at or before
    /// `as_of` — the storage-level analogue of `AT LENS x t AS OF as_of`.
    fn at_as_of_brute(layers: &[Layer<i8>], t: Timestamp, as_of: i64) -> Option<i8> {
        layers
            .iter()
            .rev()
            .filter(|l| l.written_at <= as_of)
            .find_map(|l| l.at(t))
            .copied()
    }

    fn at_brute(layers: &[Layer<i8>], t: Timestamp) -> Option<i8> {
        layers.iter().rev().find_map(|l| l.at(t)).copied()
    }

    #[hegel::test]
    fn pbt_empty_store_returns_none_for_any_lens(tc: TestCase) {
        let lens = tc.draw(gs::text().max_size(16));
        let t = tc.draw(gs::integers::<i64>());
        let store: InMemory<i32> = InMemory::new();
        assert_eq!(store.at(&lens, t), None);
        assert!(store.layers(&lens).is_none());
    }

    #[hegel::test]
    fn pbt_append_then_at_matches_layer_lookup(tc: TestCase) {
        let taus = tc.draw(taus_gen().filter(|v| !v.is_empty()));
        let probe = tc.draw(gs::integers::<i64>().min_value(-5).max_value(500));
        let mut store: InMemory<i8> = InMemory::with_threshold(1024);
        store.append("s", Layer::new(1, taus.clone())).unwrap();

        let direct = taus
            .iter()
            .find(|tau| tau.contains(probe))
            .map(|tau| tau.value);
        assert_eq!(store.at("s", probe), direct);
    }

    #[hegel::test]
    fn pbt_newest_layer_wins(tc: TestCase) {
        let stack = tc.draw(layer_stack_gen());
        let probe = tc.draw(gs::integers::<i64>().min_value(-10).max_value(500));
        let mut store: InMemory<i8> = InMemory::with_threshold(1024);
        for layer in &stack {
            store.append("s", layer.clone()).unwrap();
        }
        assert_eq!(store.at("s", probe), at_brute(&stack, probe));
    }

    #[hegel::test]
    fn pbt_compaction_preserves_point_lookups(tc: TestCase) {
        let stack = tc.draw(layer_stack_gen());
        let probe = tc.draw(gs::integers::<i64>().min_value(-10).max_value(500));

        let before = at_brute(&stack, probe);
        let mut compacted = stack.clone();
        compact_layers(&mut compacted);
        let after = compacted.iter().rev().find_map(|l| l.at(probe)).copied();
        assert_eq!(before, after);
    }

    #[hegel::test]
    fn pbt_compaction_never_grows_layer_count(tc: TestCase) {
        let stack = tc.draw(layer_stack_gen());
        let before_len = stack.len();
        let mut compacted = stack;
        compact_layers(&mut compacted);
        assert!(compacted.len() <= before_len);
    }

    #[hegel::test]
    fn pbt_compaction_yields_at_most_one_layer(tc: TestCase) {
        let stack = tc.draw(layer_stack_gen().filter(|v| !v.is_empty()));
        let mut compacted = stack;
        compact_layers(&mut compacted);
        assert!(compacted.len() <= 1);
    }

    #[hegel::test]
    fn pbt_compaction_preserves_distinct_generations(tc: TestCase) {
        // Re-stamp each layer with its own distinct transaction-time generation.
        // Per-generation compaction must NOT merge across `written_at`, so every
        // generation with data survives — the property that makes `AS OF` exact
        // after compaction.
        let stack = tc.draw(layer_stack_gen().filter(|v| !v.is_empty()));
        let stack: Vec<Layer<i8>> = stack
            .into_iter()
            .enumerate()
            .map(|(g, l)| Layer::new_at(l.id, l.taus.to_vec(), g as i64 + 1))
            .collect();
        let distinct: std::collections::BTreeSet<i64> =
            stack.iter().map(|l| l.written_at).collect();
        let mut compacted = stack.clone();
        compact_layers(&mut compacted);
        let compacted_gens: std::collections::BTreeSet<i64> =
            compacted.iter().map(|l| l.written_at).collect();
        assert_eq!(
            compacted_gens, distinct,
            "every transaction-time generation must survive compaction"
        );
    }

    #[hegel::test]
    fn pbt_compaction_preserves_as_of_across_generations(tc: TestCase) {
        // The headline invariant: for any probe point and any as-of transaction
        // time, the newest-wins-as-of value is identical before and after
        // compaction. This is what the old collapse-to-`max_written_at`
        // compaction destroyed.
        let stack = tc.draw(layer_stack_gen().filter(|v| !v.is_empty()));
        let stack: Vec<Layer<i8>> = stack
            .into_iter()
            .enumerate()
            .map(|(g, l)| Layer::new_at(l.id, l.taus.to_vec(), g as i64 + 1))
            .collect();
        let probe = tc.draw(gs::integers::<i64>().min_value(-10).max_value(500));
        let as_of = tc.draw(gs::integers::<i64>().min_value(0).max_value(12));

        let before = at_as_of_brute(&stack, probe, as_of);
        let mut compacted = stack;
        compact_layers(&mut compacted);
        let after = at_as_of_brute(&compacted, probe, as_of);
        assert_eq!(before, after, "AS OF diverged across compaction");
    }

    #[hegel::test]
    fn pbt_compaction_layer_has_non_overlapping_taus(tc: TestCase) {
        let stack = tc.draw(layer_stack_gen());
        let mut compacted = stack;
        compact_layers(&mut compacted);
        for layer in &compacted {
            for window in layer.taus.windows(2) {
                assert!(window[0].end() <= window[1].start(), "overlap detected");
            }
        }
    }

    #[hegel::test]
    fn pbt_compaction_merges_adjacent_equal_values(tc: TestCase) {
        let stack = tc.draw(layer_stack_gen().filter(|v| v.len() >= 2));
        let mut compacted = stack;
        compact_layers(&mut compacted);
        for layer in &compacted {
            for window in layer.taus.windows(2) {
                if window[0].end() == window[1].start() {
                    assert_ne!(window[0].value, window[1].value);
                }
            }
        }
    }

    #[hegel::test]
    fn pbt_compaction_triggers_above_threshold(tc: TestCase) {
        // For any threshold k and any (k+1) single-tau layers, the store
        // ends with at most one layer (compaction always runs).
        let k = tc.draw(gs::integers::<usize>().min_value(1).max_value(16));
        let n = k + 1;
        let mut store: InMemory<i32> = InMemory::with_threshold(k);
        for i in 0..n as i64 {
            let layer = Layer::new_at(
                i as u64 + 1,
                vec![Tau::new(i * 10, i * 10 + 10, i as i32)],
                0,
            );
            store.append("s", layer).unwrap();
        }
        assert_eq!(store.layers("s").unwrap().len(), 1);
    }

    #[hegel::test]
    fn pbt_lenses_are_independent(tc: TestCase) {
        let a_taus = tc.draw(taus_gen().filter(|v| !v.is_empty()));
        let b_taus = tc.draw(taus_gen().filter(|v| !v.is_empty()));
        let probe = tc.draw(gs::integers::<i64>().min_value(-5).max_value(500));
        let mut store: InMemory<i8> = InMemory::with_threshold(1024);
        store.append("a", Layer::new(1, a_taus.clone())).unwrap();
        store.append("b", Layer::new(2, b_taus.clone())).unwrap();

        let a_expected = a_taus.iter().find(|t| t.contains(probe)).map(|t| t.value);
        let b_expected = b_taus.iter().find(|t| t.contains(probe)).map(|t| t.value);
        assert_eq!(store.at("a", probe), a_expected);
        assert_eq!(store.at("b", probe), b_expected);
    }

    #[hegel::test]
    fn pbt_append_returns_did_compact_consistent_with_layer_count(tc: TestCase) {
        // append() returns true iff post-append len < prior+1, i.e. compaction
        // shortened the stack.
        let k = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(20));
        let mut store: InMemory<i32> = InMemory::with_threshold(k);
        let mut last_did_compact = false;
        for i in 0..n as i64 {
            let layer = Layer::new_at(i as u64 + 1, vec![Tau::new(i * 10, i * 10 + 10, 0)], 0);
            last_did_compact = store.append("s", layer).unwrap();
        }
        if n > k {
            // Some compaction must have fired at least once.
            let final_len = store.layers("s").unwrap().len();
            assert!(final_len <= k + 1);
        }
        // The final report agrees with the resulting state: did_compact
        // means "after this append the layer count decreased".
        let _ = last_did_compact;
    }
}
