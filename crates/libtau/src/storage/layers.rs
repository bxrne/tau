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
        let events = build_sweep_events(group);
        let merged = run_sweep(&events, group);
        if !merged.is_empty() {
            result.push(Layer::new_at(max_id, merged, generation));
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
