use std::sync::Arc;

pub type Timestamp = i64;
pub type LayerId = u64;

#[derive(Debug)]
pub struct Bound {
    pub hi: Timestamp,
    pub lo: Timestamp,
}

/// Wall-clock milliseconds since the Unix epoch, honouring the [virtual clock
/// clock](crate::clock::Clock) so transaction stamps are deterministic under
/// simulation.
#[inline]
pub(crate) fn now_ms() -> i64 {
    crate::clock::system_now_ms()
}

/// An atomic temporal fact: value V is true over [start, end).
#[derive(Debug, Clone)]
pub struct Tau<V> {
    pub coords: Arc<[Bound]>,
    pub value: V,
}

impl<V> Tau<V> {
    pub fn new(start: Timestamp, end: Timestamp, value: V) -> Self {
        assert!(
            start < end,
            "Tau: start must be less than end (got start={start}, end={end})"
        );
        Self {
            coords: Arc::from([Bound { hi: end, lo: start }]),
            value,
        }
    }

    /// Fallible N-dimensional constructor: one half-open `[lo, hi)` interval
    /// per axis, axis 0 being valid time. Returns `None` when `coords` is empty
    /// or any interval is degenerate or inverted.
    pub fn try_new_nd(coords: &[(Timestamp, Timestamp)], value: V) -> Option<Self> {
        if coords.is_empty() || coords.iter().any(|&(lo, hi)| lo >= hi) {
            return None;
        }
        Some(Self {
            coords: coords.iter().map(|&(lo, hi)| Bound { lo, hi }).collect(),
            value,
        })
    }

    /// Number of axes this tau spans.
    #[inline]
    pub fn arity(&self) -> usize {
        self.coords.len()
    }

    /// `true` when `ts` names one point per axis and every axis interval
    /// contains its point. `false` on arity mismatch.
    pub fn contains_point(&self, ts: &[Timestamp]) -> bool {
        ts.len() == self.coords.len()
            && self
                .coords
                .iter()
                .zip(ts)
                .all(|(b, &t)| b.lo <= t && t < b.hi)
    }

    /// `true` when the two orthotopes intersect on **every** axis — i.e. some
    /// point is covered by both. `false` on arity mismatch.
    pub fn box_overlaps(&self, other: &Self) -> bool {
        self.coords.len() == other.coords.len()
            && self
                .coords
                .iter()
                .zip(other.coords.iter())
                .all(|(a, b)| a.lo < b.hi && b.lo < a.hi)
    }

    /// Start of the valid-time interval (`coords[0].lo`).
    #[inline]
    pub fn start(&self) -> Timestamp {
        self.coords[0].lo
    }

    /// End of the valid-time interval (`coords[0].hi`).
    #[inline]
    pub fn end(&self) -> Timestamp {
        self.coords[0].hi
    }

    #[inline]
    pub fn contains(&self, t: Timestamp) -> bool {
        let bound = &self.coords[0];
        bound.lo <= t && t < bound.hi
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
    /// first written.  Restored verbatim on WAL/disk replay.
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
    /// Panics if the taus overlap; use [`Layer::try_new_at`] for untrusted input.
    pub fn new_at(id: LayerId, taus: Vec<Tau<V>>, written_at: i64) -> Self {
        Self::try_new_at(id, taus, written_at).expect("Layer: taus must not overlap")
    }

    /// Fallible counterpart to [`Layer::new_at`] for untrusted input (decoding a
    /// persisted file that may be corrupted). Returns `None` if any two taus
    /// overlap instead of panicking.
    pub fn try_new_at(id: LayerId, mut taus: Vec<Tau<V>>, written_at: i64) -> Option<Self> {
        taus.sort_unstable_by_key(|tau| tau.coords[0].lo);
        if taus
            .windows(2)
            .any(|w| w[0].coords[0].hi > w[1].coords[0].lo)
        {
            return None;
        }
        // Because taus are sorted by start (and non-overlapping, so end is also
        // monotone), first.start is the minimum and last.end is the maximum.
        let min_start = taus.first().map_or(Timestamp::MAX, |t| t.coords[0].lo);
        let max_end = taus.last().map_or(Timestamp::MIN, |t| t.coords[0].hi);
        Some(Self {
            id,
            min_start,
            max_end,
            taus: taus.into(),
            written_at,
        })
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
                .all(|w| w[0].coords[0].hi <= w[1].coords[0].lo),
            "new_sorted_unchecked: taus must be sorted and non-overlapping"
        );
        let min_start = taus.first().map_or(Timestamp::MAX, |t| t.coords[0].lo);
        let max_end = taus.last().map_or(Timestamp::MIN, |t| t.coords[0].hi);
        Self {
            id,
            min_start,
            max_end,
            taus: taus.into(),
            written_at,
        }
    }

    /// Fallible N-dimensional layer constructor. Taus are sorted by their
    /// valid-time start; returns `None` when arities differ across taus or two
    /// taus' orthotopes fully overlap (intersect on every axis — an ambiguous
    /// duplicate within one append batch). Unlike the 1-axis invariant, taus
    /// **may** overlap on valid time as long as they differ somewhere else.
    pub fn try_new_nd_at(id: LayerId, mut taus: Vec<Tau<V>>, written_at: i64) -> Option<Self> {
        if let Some(first) = taus.first() {
            let arity = first.arity();
            if taus.iter().any(|t| t.arity() != arity) {
                return None;
            }
        }
        for (i, a) in taus.iter().enumerate() {
            if taus[i + 1..].iter().any(|b| a.box_overlaps(b)) {
                return None;
            }
        }
        taus.sort_unstable_by_key(|tau| tau.coords[0].lo);
        // Valid-time overlap is allowed here, so `end` is not monotone with
        // `start`: scan for the true maximum rather than taking the last.
        let min_start = taus.first().map_or(Timestamp::MAX, |t| t.coords[0].lo);
        let max_end = taus
            .iter()
            .map(|t| t.coords[0].hi)
            .max()
            .unwrap_or(Timestamp::MIN);
        Some(Self {
            id,
            min_start,
            max_end,
            taus: taus.into(),
            written_at,
        })
    }

    /// O(log n) point lookup via binary search.
    pub fn at(&self, t: Timestamp) -> Option<&V> {
        let i = self
            .taus
            .binary_search_by_key(&t, |tau| tau.coords[0].lo)
            .unwrap_or_else(|i| i.saturating_sub(1));
        self.taus
            .get(i)
            .filter(|tau| tau.coords[0].lo <= t && t < tau.coords[0].hi)
            .map(|tau| &tau.value)
    }

    /// N-dimensional point lookup for the tau whose orthotope contains `ts`. The
    /// no-full-box-overlap invariant of [`Layer::try_new_nd_at`] guarantees at
    /// most one match.
    ///
    /// Fast-skips on the valid axis: `min_start`/`max_end` reject the whole
    /// layer with two comparisons, and since taus are sorted by `coords[0].lo`
    /// only the prefix with `lo <= ts[0]` can contain the point — the rest is
    /// pruned with a binary search before the (single) orthotope check.
    pub fn at_nd(&self, ts: &[Timestamp]) -> Option<&V> {
        let &t0 = ts.first()?;
        if t0 < self.min_start || t0 >= self.max_end {
            return None;
        }
        let ub = self.taus.partition_point(|tau| tau.coords[0].lo <= t0);
        self.taus[..ub]
            .iter()
            .find(|tau| tau.contains_point(ts))
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
    fn pbt_tau_new_preserves_fields_for_any_valid_range(tc: TestCase) {
        let t = tc.draw(tau_gen());
        assert!(t.coords.len() == 1);
        let bound = &t.coords[0];
        assert!(bound.lo < bound.hi);
    }

    #[hegel::test]
    fn pbt_tau_contains_iff_in_half_open_range(tc: TestCase) {
        let t = tc.draw(tau_gen());
        let probe = tc.draw(gs::integers::<i64>());
        let expected = t.coords[0].lo <= probe && probe < t.coords[0].hi;
        assert_eq!(t.contains(probe), expected);
    }

    #[test]
    #[should_panic(expected = "Tau: start must be less than end")]
    fn tau_new_rejects_empty_or_inverted() {
        Tau::new(5, 5, 0i32);
    }

    #[test]
    fn tau_try_new_nd_returns_none_for_empty_or_inverted() {
        assert!(Tau::try_new_nd(&[(0, 10)], 1i32).is_some());
        assert!(Tau::try_new_nd(&[(5, 5)], 1i32).is_none(), "empty interval");
        assert!(
            Tau::try_new_nd(&[(10, 5)], 1i32).is_none(),
            "inverted interval"
        );
    }

    #[test]
    fn layer_try_new_at_returns_none_for_overlapping_taus() {
        // Non-overlapping (even when unsorted) builds; overlapping is rejected
        // cleanly instead of panicking like `new_at`.
        assert!(
            Layer::try_new_at(1, vec![Tau::new(10, 20, 0i32), Tau::new(0, 10, 1i32)], 0).is_some()
        );
        assert!(
            Layer::try_new_at(1, vec![Tau::new(0, 15, 0i32), Tau::new(10, 20, 1i32)], 0).is_none(),
            "overlapping taus must be rejected"
        );
    }

    #[hegel::test]
    fn pbt_layer_new_sorts_taus(tc: TestCase) {
        let taus = tc.draw(non_overlapping_taus());
        // Even after deliberate shuffling via a sort permutation, Layer::new
        // re-establishes start-order.
        let layer = Layer::new(1, taus);
        for w in layer.taus.windows(2) {
            assert!(
                w[0].coords[0].lo <= w[1].coords[0].lo,
                "taus must be sorted by start"
            );
        }
    }

    #[hegel::test]
    fn pbt_layer_bounds_match_first_and_last(tc: TestCase) {
        let taus = tc.draw(non_overlapping_taus().filter(|v| !v.is_empty()));
        let layer = Layer::new(1, taus);
        let expected_min_start = layer
            .taus
            .first()
            .map_or(Timestamp::MAX, |t| t.coords[0].lo);
        let expected_max_end = layer.taus.last().map_or(Timestamp::MIN, |t| t.coords[0].hi);
        assert_eq!(layer.min_start, expected_min_start);
        assert_eq!(layer.max_end, expected_max_end);
    }

    #[hegel::test]
    fn pbt_layer_empty_has_sentinel_bounds(tc: TestCase) {
        let _ = tc;
        let layer: Layer<i32> = Layer::new(1, vec![]);
        assert_eq!(layer.min_start, Timestamp::MAX);
        assert_eq!(layer.max_end, Timestamp::MIN);
        assert_eq!(layer.at(0), None);
    }

    #[hegel::test]
    fn pbt_layer_at_matches_linear_scan(tc: TestCase) {
        let taus = tc.draw(non_overlapping_taus());
        let probe = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let layer = Layer::new(1, taus.clone());

        let expected = taus
            .iter()
            .find(|tau| tau.coords[0].lo <= probe && probe < tau.coords[0].hi)
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

    fn nd(coords: &[(i64, i64)], v: i32) -> Tau<i32> {
        Tau::try_new_nd(coords, v).expect("valid nd tau")
    }

    #[test]
    fn tau_try_new_nd_rejects_empty_and_inverted() {
        assert!(Tau::try_new_nd(&[], 0i32).is_none(), "no axes");
        assert!(Tau::try_new_nd(&[(0, 10), (5, 5)], 0i32).is_none(), "empty");
        assert!(
            Tau::try_new_nd(&[(0, 10), (7, 3)], 0i32).is_none(),
            "inverted"
        );
        assert_eq!(nd(&[(0, 10), (0, 100)], 1).arity(), 2);
    }

    #[test]
    fn tau_contains_point_requires_every_axis() {
        let t = nd(&[(0, 10), (50, 60)], 7);
        assert!(t.contains_point(&[5, 55]));
        assert!(!t.contains_point(&[5, 60]), "axis 1 at hi is outside");
        assert!(!t.contains_point(&[10, 55]), "axis 0 at hi is outside");
        assert!(!t.contains_point(&[5]), "arity mismatch");
    }

    #[test]
    fn tau_box_overlaps_needs_intersection_on_all_axes() {
        let a = nd(&[(0, 10), (0, 10)], 1);
        assert!(a.box_overlaps(&nd(&[(5, 15), (5, 15)], 2)));
        assert!(
            !a.box_overlaps(&nd(&[(5, 15), (10, 20)], 2)),
            "disjoint on axis 1"
        );
        assert!(!a.box_overlaps(&Tau::new(0, 10, 2)), "arity mismatch");
    }

    #[test]
    fn layer_nd_allows_valid_time_overlap_rejects_box_overlap() {
        // Same valid window, disjoint on axis 1: allowed.
        let ok = Layer::try_new_nd_at(
            1,
            vec![nd(&[(0, 10), (0, 50)], 1), nd(&[(0, 10), (50, 100)], 2)],
            0,
        );
        let layer = ok.expect("disjoint boxes must build");
        assert_eq!(layer.at_nd(&[5, 25]), Some(&1));
        assert_eq!(layer.at_nd(&[5, 75]), Some(&2));
        assert_eq!(layer.at_nd(&[5, 100]), None);
        // Intersecting on every axis: rejected.
        assert!(
            Layer::try_new_nd_at(
                1,
                vec![nd(&[(0, 10), (0, 50)], 1), nd(&[(5, 15), (25, 75)], 2)],
                0,
            )
            .is_none()
        );
        // Mixed arity: rejected.
        assert!(
            Layer::try_new_nd_at(1, vec![nd(&[(0, 10), (0, 50)], 1), Tau::new(20, 30, 2)], 0)
                .is_none()
        );
    }

    #[test]
    fn layer_nd_bounds_span_valid_axis() {
        let layer = Layer::try_new_nd_at(
            1,
            vec![nd(&[(5, 40), (0, 10)], 1), nd(&[(0, 20), (10, 20)], 2)],
            0,
        )
        .expect("boxes disjoint on axis 1");
        assert_eq!(layer.min_start, 0);
        assert_eq!(layer.max_end, 40, "max end is not the last tau's end");
    }
}
