use crate::libtau::model::{Layer, Tau, Timestamp};
use std::collections::{BinaryHeap, HashSet};
use std::io;

/// Build sweep-line events from `layers`: one start and one end event per tau.
///
/// Event tuple: `(time, is_end, layer_idx, tau_idx)`.
/// Sorting by `(time, is_end)` ensures starts fire before ends at the same `t`.
fn build_sweep_events<V>(layers: &[Layer<V>]) -> Vec<(Timestamp, bool, usize, usize)> {
    let total: usize = layers.iter().map(|l| l.taus.len()).sum();
    let mut events: Vec<(Timestamp, bool, usize, usize)> = Vec::with_capacity(total * 2);
    for (layer_idx, layer) in layers.iter().enumerate() {
        for (tau_idx, tau) in layer.taus.iter().enumerate() {
            events.push((tau.start, false, layer_idx, tau_idx));
            events.push((tau.end, true, layer_idx, tau_idx));
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
                Some(last) if last.end == c && last.value == v => last.end = t,
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
    let mut closed: HashSet<(usize, usize)> = HashSet::new();
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

/// Compact `layers` down to a single layer representing the same newest-wins
/// truth.  Every unique tau boundary becomes a potential split point; the
/// effective value at each sub-interval is the one from the most-recently
/// appended layer that covers it.  Adjacent sub-intervals with equal values
/// are merged.  No-ops when there is already ≤ 1 layer.
///
/// Implementation: sweep-line over `(time, start/end, layer_idx)` events.
/// O(E log E) where E = 2 × total tau count.
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
    let max_id = layers.iter().map(|l| l.id).max().unwrap_or(0);
    let max_written_at = layers.iter().map(|l| l.written_at).max().unwrap_or(0);
    let events = build_sweep_events(layers);
    let merged = run_sweep(&events, layers);
    *layers = if merged.is_empty() {
        Vec::new()
    } else {
        vec![Layer::new_at(max_id, merged, max_written_at)]
    };
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
        let lo = taus.partition_point(|t| t.end <= range_start);
        let hi = taus.partition_point(|t| t.start < range_end);
        for tau_idx in lo..hi {
            let tau = &taus[tau_idx];
            let eff_start = tau.start.max(range_start);
            let eff_end = tau.end.min(range_end);
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

/// Number of layers per lens that triggers an automatic compaction.
pub const COMPACT_THRESHOLD: usize = 8;

/// Pluggable backing storage for layered temporal data.
///
/// Implementors are responsible for the layer stack and newest-wins semantics.
pub trait Store<V>: Send + Sync
where
    V: Clone + Send + Sync + 'static,
{
    /// Push a new layer onto the named lens.  Returns `true` when the
    /// post-append state has fewer layers than `prior+1`, i.e. compaction ran.
    ///
    /// Returns an error if the underlying storage write fails.  Callers must
    /// treat a returned error as meaning the layer was **not** persisted; they
    /// should not update any higher-level state after receiving one.
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool>;

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

    /// Remove all layers for `lens`, as if the lens never existed.
    ///
    /// Called by the executor when `DROP LENS` is executed so that recreating
    /// the same lens name starts from a clean state.  Default is a no-op;
    /// implementors should override to actually free the storage.
    fn drop_lens(&mut self, _lens: &str) {}

    /// Names of all lenses that have at least one layer.  Used by the WAL
    /// checkpoint to enumerate live data for compaction.  Default returns an
    /// empty vec; backends should override.
    fn lens_names(&self) -> Vec<String> {
        Vec::new()
    }
}
