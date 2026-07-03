use crate::model::{Layer, Tau, Timestamp};
use rustc_hash::FxHashSet as HashSet;
use std::collections::BinaryHeap;

/// Build sweep-line events from `layers`: one start and one end event per tau.
///
/// Event tuple: `(time, is_end, layer_idx, tau_idx)`.
/// Sorting by `(time, is_end)` ensures starts fire before ends at the same `t`.
fn build_sweep_events<V>(layers: &[Layer<V>]) -> Vec<(Timestamp, bool, usize, usize)> {
    let total: usize = layers.iter().map(|l| l.taus.len()).sum();
    let mut events: Vec<(Timestamp, bool, usize, usize)> = Vec::with_capacity(total * 2);
    for (layer_idx, layer) in layers.iter().enumerate() {
        for (tau_idx, tau) in layer.taus.iter().enumerate() {
            events.push((tau.start(), false, layer_idx, tau_idx));
            events.push((tau.end(), true, layer_idx, tau_idx));
        }
    }
    events.sort_unstable_by_key(|e| (e.0, e.1));
    events
}

/// Pop lazily-closed entries off the top of `active`.
fn drain_stale(active: &mut BinaryHeap<(usize, usize)>, closed: &mut HashSet<(usize, usize)>) {
    while let Some(&top) = active.peek() {
        if closed.remove(&top) {
            active.pop();
        } else {
            break;
        }
    }
}

/// Emit a merged segment `[cursor, t)` if there is an active tau at that interval.
fn maybe_emit_segment<V>(
    cursor: Option<Timestamp>,
    t: Timestamp,
    active: &mut BinaryHeap<(usize, usize)>,
    closed: &mut HashSet<(usize, usize)>,
    merged: &mut Vec<Tau<V>>,
    layers: &[Layer<V>],
) where
    V: Clone + PartialEq,
{
    if let Some(c) = cursor
        && c < t
    {
        drain_stale(active, closed);
        if let Some(&(layer_idx, tau_idx)) = active.peek() {
            let v = layers[layer_idx].taus[tau_idx].value.clone();
            match merged.last_mut() {
                Some(last) if last.end() == c && last.value == v => {
                    // Extend the previous segment to `t` by replacing it; coords
                    // are shared behind an `Arc`, so `end` cannot be set in place.
                    *last = Tau::new(last.start(), t, v);
                }
                _ => merged.push(Tau::new(c, t, v)),
            }
        }
    }
}

/// Apply all events that share timestamp `t`, advancing `i`.
fn apply_events_at(
    events: &[(Timestamp, bool, usize, usize)],
    i: &mut usize,
    t: Timestamp,
    active: &mut BinaryHeap<(usize, usize)>,
    closed: &mut HashSet<(usize, usize)>,
) {
    while *i < events.len() && events[*i].0 == t {
        let (_, is_end, layer_idx, tau_idx) = events[*i];
        if is_end {
            closed.insert((layer_idx, tau_idx));
        } else {
            active.push((layer_idx, tau_idx));
        }
        *i += 1;
    }
}

/// Sweep events in order, producing a merged tau sequence.
fn run_sweep<V>(events: &[(Timestamp, bool, usize, usize)], layers: &[Layer<V>]) -> Vec<Tau<V>>
where
    V: Clone + PartialEq,
{
    let mut active: BinaryHeap<(usize, usize)> = BinaryHeap::new();
    let mut closed: HashSet<(usize, usize)> = HashSet::default();
    let mut merged: Vec<Tau<V>> = Vec::new();
    let mut cursor: Option<Timestamp> = None;
    let mut i = 0;
    while i < events.len() {
        let t = events[i].0;
        maybe_emit_segment(cursor, t, &mut active, &mut closed, &mut merged, layers);
        cursor = Some(t);
        apply_events_at(events, &mut i, t, &mut active, &mut closed);
    }
    merged
}

/// Per-axis half-open `[lo, hi)` coordinates of an orthotope, used by the
/// N-dimensional compaction below.
type Coords = Vec<(Timestamp, Timestamp)>;

/// Subtract orthotope `b` from `a` (equal arity), returning the parts of `a`
/// **not** covered by `b` as point-disjoint orthotopes. Returns `[a]` when they
/// do not intersect, `[]` when `b` fully covers `a`. The standard slab
/// decomposition: for each axis, peel off the portion of the (progressively
/// clamped) remainder that lies below / above `b` on that axis, then clamp the
/// remainder to the overlap for subsequent axes.
fn subtract_box(a: &[(Timestamp, Timestamp)], b: &[(Timestamp, Timestamp)]) -> Vec<Coords> {
    if a.iter()
        .zip(b)
        .any(|(&(alo, ahi), &(blo, bhi))| alo >= bhi || blo >= ahi)
    {
        return vec![a.to_vec()];
    }
    let mut result = Vec::new();
    let mut remaining = a.to_vec();
    for axis in 0..a.len() {
        let (alo, ahi) = remaining[axis];
        let (blo, bhi) = b[axis];
        if alo < blo {
            let mut slab = remaining.clone();
            slab[axis] = (alo, blo);
            result.push(slab);
        }
        if bhi < ahi {
            let mut slab = remaining.clone();
            slab[axis] = (bhi, ahi);
            result.push(slab);
        }
        remaining[axis] = (alo.max(blo), ahi.min(bhi));
    }
    result
}

/// The single axis on which `a` and `b` are mergeable: equal on every other
/// axis and exactly adjacent (touching, no gap or overlap) on this one. `None`
/// when they differ on more than one axis or are not adjacent.
fn mergeable_axis(a: &[(Timestamp, Timestamp)], b: &[(Timestamp, Timestamp)]) -> Option<usize> {
    let mut diff = None;
    for (k, (&(alo, ahi), &(blo, bhi))) in a.iter().zip(b).enumerate() {
        if (alo, ahi) == (blo, bhi) {
            continue;
        }
        if diff.is_some() {
            return None;
        }
        if ahi == blo || bhi == alo {
            diff = Some(k);
        } else {
            return None;
        }
    }
    diff
}

/// Merge coplanar, adjacent, equal-value cells to a fixpoint. Inputs are
/// point-disjoint; each merge replaces two cells with their bounding box (which
/// equals their union), preserving disjointness.
fn merge_cells<V: PartialEq>(cells: &mut Vec<(Coords, V)>) {
    let mut changed = true;
    while changed {
        changed = false;
        'outer: for i in 0..cells.len() {
            for j in (i + 1)..cells.len() {
                if cells[i].1 != cells[j].1 {
                    continue;
                }
                if let Some(axis) = mergeable_axis(&cells[i].0, &cells[j].0) {
                    let (alo, ahi) = cells[i].0[axis];
                    let (blo, bhi) = cells[j].0[axis];
                    cells[i].0[axis] = (alo.min(blo), ahi.max(bhi));
                    cells.remove(j);
                    changed = true;
                    break 'outer;
                }
            }
        }
    }
}

/// Compact one transaction-time generation of a multi-axis lens: resolve
/// newest-wins across the group's layers by subtracting every strictly-newer
/// orthotope from each tau's box, then merge the resulting fragments. The output
/// covers exactly the same N-space region as the input with the same value at
/// every point (lossless), as point-disjoint orthotopes.
fn compact_generation_nd<V: Clone + PartialEq>(group: &[Layer<V>]) -> Vec<Tau<V>> {
    // Append order = age; taus within one layer are already point-disjoint, so
    // only strictly-newer taus (later in this flat order) can occlude.
    let all: Vec<(Coords, V)> = group
        .iter()
        .flat_map(|l| l.taus.iter())
        .map(|t| {
            (
                t.coords.iter().map(|b| (b.lo, b.hi)).collect::<Coords>(),
                t.value.clone(),
            )
        })
        .collect();

    let mut cells: Vec<(Coords, V)> = Vec::new();
    for (i, (coords, value)) in all.iter().enumerate() {
        let mut frags = vec![coords.clone()];
        for (newer, _) in &all[i + 1..] {
            if frags.is_empty() {
                break;
            }
            frags = frags.iter().flat_map(|f| subtract_box(f, newer)).collect();
        }
        for frag in frags {
            cells.push((frag, value.clone()));
        }
    }

    merge_cells(&mut cells);
    cells
        .into_iter()
        .filter_map(|(c, v)| Tau::try_new_nd(&c, v))
        .collect()
}

/// Compact `layers` **within each transaction-time generation**, preserving the
/// same query results — including `AT AS OF` and `HISTORY` — exactly.
///
/// A *generation* is a maximal run of layers that share a `written_at`
/// transaction timestamp. Within a generation the sweep-line collapses the
/// layers to one canonical layer (every unique tau boundary is a potential
/// split point; the effective value at each sub-interval comes from the
/// most-recently appended layer of that generation; adjacent equal values are
/// merged). Generations are **never** merged with one another: distinct
/// `written_at` stamps must survive so a lookup `AS OF` a past transaction time
/// still sees the belief that was current then. This is the lossless-compaction
/// invariant — replacing the previous behaviour that collapsed every layer into
/// one stamped `max(written_at)` and silently destroyed the transaction-time
/// axis.
///
/// Layers are stored in append order and `written_at` is monotonic
/// non-decreasing with append order, so equal-`written_at` layers form
/// contiguous runs and can be grouped in a single linear pass.
///
/// Implementation: per generation, a sweep-line over `(time, start/end,
/// layer_idx)` events — O(E log E) where E = 2 × total tau count.
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
    // A lens has a single arity (enforced at append), so the first tau decides
    // the path: axis-0 sweep-line for 1-D, orthotope subtraction for N-D.
    let arity = layers
        .iter()
        .flat_map(|l| l.taus.iter())
        .next()
        .map_or(1, |t| t.arity());
    let mut result: Vec<Layer<V>> = Vec::new();
    let mut i = 0;
    while i < layers.len() {
        let generation = layers[i].written_at;
        let mut j = i + 1;
        while j < layers.len() && layers[j].written_at == generation {
            j += 1;
        }
        let group = &layers[i..j];
        let max_id = group.iter().map(|l| l.id).max().unwrap_or(0);
        if arity <= 1 {
            let events = build_sweep_events(group);
            let merged = run_sweep(&events, group);
            if !merged.is_empty() {
                result.push(Layer::new_at(max_id, merged, generation));
            }
        } else {
            let merged = compact_generation_nd(group);
            // The output is point-disjoint by construction; if a future edge
            // case ever violated that, keep the generation's layers uncompacted
            // rather than panicking — still lossless, just not compacted.
            match Layer::try_new_nd_at(max_id, merged, generation) {
                Some(layer) if !layer.taus.is_empty() => result.push(layer),
                Some(_) => {}
                None => result.extend(group.iter().cloned()),
            }
        }
        i = j;
    }
    *layers = result;
}

/// Build sweep-line events restricted to the half-open interval `[range_start, range_end)`.
///
/// Each tau is clamped to the query range so the sweep produces output
/// segments that stay within `[range_start, range_end)`.  Taus that do not
/// overlap the range at all are skipped.
fn build_sweep_events_in_range<V>(
    layers: &[Layer<V>],
    range_start: Timestamp,
    range_end: Timestamp,
) -> Vec<(Timestamp, bool, usize, usize)> {
    let mut events = Vec::new();
    for (layer_idx, layer) in layers.iter().enumerate() {
        if layer.max_end <= range_start || layer.min_start >= range_end {
            continue;
        }
        let taus = &layer.taus;
        let lo = taus.partition_point(|t| t.end() <= range_start);
        let hi = taus.partition_point(|t| t.start() < range_end);
        for tau_idx in lo..hi {
            let tau = &taus[tau_idx];
            let eff_start = tau.start().max(range_start);
            let eff_end = tau.end().min(range_end);
            if eff_start < eff_end {
                events.push((eff_start, false, layer_idx, tau_idx));
                events.push((eff_end, true, layer_idx, tau_idx));
            }
        }
    }
    events.sort_unstable_by_key(|e| (e.0, e.1));
    events
}

/// Single-pass range query using the sweep-line algorithm restricted to
/// `[start, end)`.
///
/// Returns merged `Tau` segments with newest-wins semantics applied across all
/// `layers`.  This is O(E log E) where E is the number of tau boundaries
/// within the range, which is significantly better than calling `at()` at every
/// boundary (O(M × N × log n) with M boundaries and N layers).
///
/// The returned `Tau`s use the clamped boundaries — they are always contained
/// within `[start, end)`.
pub fn sweep_range<V>(layers: &[Layer<V>], start: Timestamp, end: Timestamp) -> Vec<Tau<V>>
where
    V: Clone + PartialEq,
{
    if layers.is_empty() || start >= end {
        return Vec::new();
    }
    let events = build_sweep_events_in_range(layers, start, end);
    if events.is_empty() {
        return Vec::new();
    }
    run_sweep(&events, layers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Layer;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    fn nd(coords: &[(i64, i64)], v: i32) -> Tau<i32> {
        Tau::try_new_nd(coords, v).expect("valid nd tau")
    }

    /// Newest-wins point lookup over a raw layer stack, restricted to layers
    /// written at or before `as_of` — the ground truth the compacted stack must
    /// reproduce for every point and every as-of.
    fn brute_at_as_of(layers: &[Layer<i32>], pt: &[i64], as_of: i64) -> Option<i32> {
        layers
            .iter()
            .rev()
            .filter(|l| l.written_at <= as_of)
            .find_map(|l| l.at_nd(pt))
            .copied()
    }

    #[test]
    fn subtract_box_disjoint_returns_input() {
        let a = vec![(0, 10), (0, 10)];
        let b = vec![(20, 30), (0, 10)];
        assert_eq!(subtract_box(&a, &b), vec![a]);
    }

    #[test]
    fn subtract_box_full_cover_returns_empty() {
        let a = vec![(2, 8), (2, 8)];
        let b = vec![(0, 10), (0, 10)];
        assert!(subtract_box(&a, &b).is_empty());
    }

    #[test]
    fn subtract_box_partial_is_disjoint_partition() {
        // [0,10)x[0,10) minus [5,15)x[5,15) = the L-shape, as disjoint boxes.
        let a = vec![(0, 10), (0, 10)];
        let b = vec![(5, 15), (5, 15)];
        let frags = subtract_box(&a, &b);
        // Covers exactly a \ b: sample points.
        let covered = |p: [i64; 2]| {
            frags
                .iter()
                .any(|f| f[0].0 <= p[0] && p[0] < f[0].1 && f[1].0 <= p[1] && p[1] < f[1].1)
        };
        for x in 0..10 {
            for y in 0..10 {
                let in_b = (5..15).contains(&x) && (5..15).contains(&y);
                assert_eq!(covered([x, y]), !in_b, "point ({x},{y})");
            }
        }
        // Fragments are pairwise point-disjoint.
        for i in 0..frags.len() {
            for j in (i + 1)..frags.len() {
                let t1 = Tau::try_new_nd(&frags[i], 0i32).unwrap();
                let t2 = Tau::try_new_nd(&frags[j], 0i32).unwrap();
                assert!(!t1.box_overlaps(&t2), "fragments overlap");
            }
        }
    }

    #[test]
    fn merge_cells_joins_coplanar_adjacent_equal() {
        let mut cells = vec![
            (vec![(0, 5), (0, 10)], 1i32),
            (vec![(5, 10), (0, 10)], 1i32),
        ];
        merge_cells(&mut cells);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].0, vec![(0, 10), (0, 10)]);
    }

    #[test]
    fn merge_cells_keeps_gapped_or_unequal() {
        let mut gapped = vec![
            (vec![(0, 5), (0, 10)], 1i32),
            (vec![(6, 10), (0, 10)], 1i32),
        ];
        merge_cells(&mut gapped);
        assert_eq!(gapped.len(), 2, "a gap prevents merging");

        let mut diff_val = vec![
            (vec![(0, 5), (0, 10)], 1i32),
            (vec![(5, 10), (0, 10)], 2i32),
        ];
        merge_cells(&mut diff_val);
        assert_eq!(diff_val.len(), 2, "different values do not merge");
    }

    #[test]
    fn nd_compaction_resolves_occlusion() {
        // Older full box overwritten in its centre by a newer box.
        let layers = vec![
            Layer::try_new_nd_at(1, vec![nd(&[(0, 10), (0, 10)], 1)], 5).unwrap(),
            Layer::try_new_nd_at(2, vec![nd(&[(5, 15), (5, 15)], 2)], 5).unwrap(),
        ];
        let mut compacted = layers.clone();
        compact_layers(&mut compacted);
        assert_eq!(compacted.len(), 1, "one generation → one layer");
        for x in -2..16 {
            for y in -2..16 {
                assert_eq!(
                    brute_at_as_of(&compacted, &[x, y], i64::MAX),
                    brute_at_as_of(&layers, &[x, y], i64::MAX),
                    "point ({x},{y}) diverged"
                );
            }
        }
    }

    /// Generator: a stack of 2-axis layers spread across a few transaction-time
    /// generations, each layer a handful of small boxes.
    fn nd_stack_gen() -> impl gs::Generator<Vec<Layer<i32>>> {
        gs::vecs(
            gs::vecs(
                gs::integers::<i64>()
                    .min_value(0)
                    .max_value(8)
                    .flat_map(|x0| {
                        gs::integers::<i64>()
                            .min_value(1)
                            .max_value(6)
                            .flat_map(move |w| {
                                gs::integers::<i64>().min_value(0).max_value(8).flat_map(
                                    move |y0| {
                                        gs::integers::<i64>().min_value(1).max_value(6).flat_map(
                                            move |h| {
                                                gs::integers::<i32>()
                                                    .min_value(0)
                                                    .max_value(3)
                                                    .map(move |v| (x0, x0 + w, y0, y0 + h, v))
                                            },
                                        )
                                    },
                                )
                            })
                    }),
            )
            .min_size(1)
            .max_size(4),
        )
        .min_size(1)
        .max_size(6)
        .map(|batches| {
            // Assign generations: 2 consecutive layers share a written_at.
            let mut layers = Vec::new();
            for (i, boxes) in batches.into_iter().enumerate() {
                // Boxes within one layer must be point-disjoint; drop any that
                // clash with an earlier box in the same batch.
                let mut taus: Vec<Tau<i32>> = Vec::new();
                for (x0, x1, y0, y1, v) in boxes {
                    let cand = nd(&[(x0, x1), (y0, y1)], v);
                    if !taus.iter().any(|t| t.box_overlaps(&cand)) {
                        taus.push(cand);
                    }
                }
                if let Some(layer) = Layer::try_new_nd_at(i as u64 + 1, taus, (i as i64 / 2) + 1) {
                    layers.push(layer);
                }
            }
            layers
        })
    }

    #[hegel::test]
    fn pbt_nd_compaction_is_lossless(tc: TestCase) {
        let layers = tc.draw(nd_stack_gen().filter(|v| !v.is_empty()));
        let mut compacted = layers.clone();
        compact_layers(&mut compacted);

        // Compaction preserves the transaction-time generation set.
        let gens = |ls: &[Layer<i32>]| {
            let mut g: Vec<i64> = ls.iter().map(|l| l.written_at).collect();
            g.sort_unstable();
            g.dedup();
            g
        };
        assert_eq!(gens(&layers), gens(&compacted), "generations must survive");

        // AT / AS OF are identical at every point and as-of boundary.
        for x in -1..11 {
            for y in -1..11 {
                for as_of in [0i64, 1, 2, 3, i64::MAX] {
                    assert_eq!(
                        brute_at_as_of(&layers, &[x, y], as_of),
                        brute_at_as_of(&compacted, &[x, y], as_of),
                        "diverged at ({x},{y}) as_of {as_of}"
                    );
                }
            }
        }

        // Compacted taus within a layer are point-disjoint.
        for layer in &compacted {
            for i in 0..layer.taus.len() {
                for j in (i + 1)..layer.taus.len() {
                    assert!(
                        !layer.taus[i].box_overlaps(&layer.taus[j]),
                        "compacted layer has overlapping taus"
                    );
                }
            }
        }
    }
}
