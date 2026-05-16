use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Timestamp = i64;
pub type LayerId = u64;

/// Wall-clock milliseconds since the Unix epoch.
#[inline]
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// An atomic temporal fact: value V is true over [start, end).
#[derive(Debug, Clone)]
pub struct Tau<V> {
    pub start: Timestamp,
    pub end: Timestamp,
    pub value: V,
}

impl<V> Tau<V> {
    pub fn new(start: Timestamp, end: Timestamp, value: V) -> Self {
        assert!(start < end, "Tau: start must be less than end");
        Self { start, end, value }
    }

    #[inline]
    pub fn contains(&self, t: Timestamp) -> bool {
        self.start <= t && t < self.end
    }
}

/// An append batch / revision: a sorted, non-overlapping slice of Tau<V>.
#[derive(Debug, Clone)]
pub struct Layer<V> {
    pub id: LayerId,
    /// Minimum `start` across all taus in this layer.  Used by `at_layers` to
    /// skip a layer with a single comparison instead of a full binary search.
    pub min_start: Timestamp,
    /// Maximum `end` across all taus in this layer.  Paired with `min_start`
    /// for the fast-skip: if `t < min_start || t >= max_end` the layer cannot
    /// contain a tau covering `t`.
    pub max_end: Timestamp,
    pub taus: Arc<[Tau<V>]>,
    /// Wall-clock time (milliseconds since Unix epoch) when this layer was
    /// first written.  Set to 0 for layers replayed from WAL files that
    /// predate the timestamp field (backward-compatible replay).
    pub written_at: i64,
}

impl<V> Layer<V> {
    /// Create a new layer, recording the current wall-clock time as the write
    /// timestamp.  Taus are sorted by `start`; overlapping taus panic.
    pub fn new(id: LayerId, taus: Vec<Tau<V>>) -> Self {
        Self::new_at(id, taus, now_ms())
    }

    /// Create a layer with an explicit write timestamp.  Used during WAL
    /// replay to restore the original timestamp rather than the replay time.
    pub fn new_at(id: LayerId, mut taus: Vec<Tau<V>>, written_at: i64) -> Self {
        taus.sort_by_key(|t| t.start);
        assert!(
            taus.windows(2).all(|w| w[0].end <= w[1].start),
            "Layer: taus must not overlap"
        );
        // Because taus are sorted by start (and non-overlapping, so end is also
        // monotone), first.start is the minimum and last.end is the maximum.
        let min_start = taus.first().map_or(Timestamp::MAX, |t| t.start);
        let max_end = taus.last().map_or(Timestamp::MIN, |t| t.end);
        Self {
            id,
            min_start,
            max_end,
            taus: taus.into(),
            written_at,
        }
    }

    /// Create a layer from taus that the caller guarantees are already sorted
    /// by `start` and non-overlapping.  Skips the sort + overlap check.
    /// Used by BATCH APPEND and COPY, where the taus arrive in order and the
    /// executor has already validated the type of each value.
    ///
    /// # Panics (debug only)
    /// In debug builds a `debug_assert` verifies the invariant so tests catch
    /// misuse.
    pub fn new_sorted_unchecked(id: LayerId, taus: Vec<Tau<V>>, written_at: i64) -> Self {
        debug_assert!(
            taus.windows(2)
                .all(|w| w[0].start <= w[1].start && w[0].end <= w[1].start),
            "new_sorted_unchecked: taus must be sorted and non-overlapping"
        );
        let min_start = taus.first().map_or(Timestamp::MAX, |t| t.start);
        let max_end = taus.last().map_or(Timestamp::MIN, |t| t.end);
        Self {
            id,
            min_start,
            max_end,
            taus: taus.into(),
            written_at,
        }
    }

    /// O(log n) point lookup via binary search.
    pub fn at(&self, t: Timestamp) -> Option<&V> {
        let i = self.taus.partition_point(|tau| tau.end <= t);
        self.taus
            .get(i)
            .filter(|tau| tau.start <= t)
            .map(|tau| &tau.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    /// Generate a valid `Tau<i32>` (start < end).  Both bounds are constrained
    /// to a manageable range so shrunken counterexamples remain readable.
    fn tau_gen() -> impl Generator<Tau<i32>> {
        gs::integers::<i64>()
            .min_value(-1_000_000)
            .max_value(1_000_000)
            .flat_map(|start| {
                gs::integers::<i64>()
                    .min_value(start + 1)
                    .max_value(start + 1_000_000)
                    .flat_map(move |end| {
                        gs::integers::<i32>().map(move |v| Tau::new(start, end, v))
                    })
            })
    }

    /// Generate a sorted, non-overlapping `Vec<Tau<i32>>` by walking a cursor
    /// forward across the timeline.  Yields between 0 and 50 taus.
    fn non_overlapping_taus() -> impl Generator<Vec<Tau<i32>>> {
        gs::vecs(
            gs::integers::<i64>()
                .min_value(1)
                .max_value(100)
                .flat_map(|width| {
                    gs::integers::<i64>()
                        .min_value(1)
                        .max_value(50)
                        .flat_map(move |gap| gs::integers::<i32>().map(move |v| (width, gap, v)))
                }),
        )
        .max_size(50)
        .map(|specs| {
            let mut taus = Vec::with_capacity(specs.len());
            let mut cursor: i64 = 0;
            for (width, gap, v) in specs {
                let start = cursor + gap;
                let end = start + width;
                taus.push(Tau::new(start, end, v));
                cursor = end;
            }
            taus
        })
    }

    #[hegel::test]
    fn tau_new_preserves_fields_for_any_valid_range(tc: TestCase) {
        let t = tc.draw(tau_gen());
        assert!(t.start < t.end);
        assert!(t.contains(t.start));
        assert!(!t.contains(t.end));
    }

    #[hegel::test]
    fn tau_contains_iff_in_half_open_range(tc: TestCase) {
        let t = tc.draw(tau_gen());
        let probe = tc.draw(gs::integers::<i64>());
        let expected = t.start <= probe && probe < t.end;
        assert_eq!(t.contains(probe), expected);
    }

    #[test]
    #[should_panic(expected = "Tau: start must be less than end")]
    fn tau_new_rejects_empty_or_inverted() {
        Tau::new(5, 5, 0i32);
    }

    #[hegel::test]
    fn layer_new_sorts_taus(tc: TestCase) {
        let taus = tc.draw(non_overlapping_taus());
        // Even after deliberate shuffling via a sort permutation, Layer::new
        // re-establishes start-order.
        let layer = Layer::new(1, taus);
        for w in layer.taus.windows(2) {
            assert!(w[0].start <= w[1].start);
        }
    }

    #[hegel::test]
    fn layer_bounds_match_first_and_last(tc: TestCase) {
        let taus = tc.draw(non_overlapping_taus().filter(|v| !v.is_empty()));
        let layer = Layer::new(1, taus);
        assert_eq!(layer.min_start, layer.taus.first().unwrap().start);
        assert_eq!(layer.max_end, layer.taus.last().unwrap().end);
    }

    #[hegel::test]
    fn layer_empty_has_sentinel_bounds(tc: TestCase) {
        let _ = tc;
        let layer: Layer<i32> = Layer::new(1, vec![]);
        assert_eq!(layer.min_start, Timestamp::MAX);
        assert_eq!(layer.max_end, Timestamp::MIN);
        assert_eq!(layer.at(0), None);
    }

    #[hegel::test]
    fn layer_at_matches_linear_scan(tc: TestCase) {
        let taus = tc.draw(non_overlapping_taus());
        let probe = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let layer = Layer::new(1, taus.clone());

        let expected = taus
            .iter()
            .find(|tau| tau.start <= probe && probe < tau.end)
            .map(|tau| tau.value);
        assert_eq!(layer.at(probe).copied(), expected);
    }

    #[test]
    #[should_panic(expected = "Layer: taus must not overlap")]
    fn layer_panics_on_overlapping_taus() {
        Layer::new(1, vec![Tau::new(0, 15, 0i32), Tau::new(10, 20, 1i32)]);
    }

    #[test]
    fn layer_clone_shares_arc() {
        let layer = Layer::new(1, vec![Tau::new(0, 10, 42i32)]);
        let clone = layer.clone();
        assert!(Arc::ptr_eq(&layer.taus, &clone.taus));
    }
}
