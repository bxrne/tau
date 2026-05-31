//! Reference oracle: a `BTreeMap`-backed temporal store with no layers, no
//! compaction, and no WAL — just obviously correct newest-wins semantics.
//!
//! The oracle shadows every write the test driver makes to the real engine.
//! After each read operation the test driver compares the engine's answer to
//! the oracle's answer; any divergence is a bug in the engine.
//!
//! Because the DST's time cursor is monotonically increasing, start keys are
//! unique within a lens and segments never overlap — so the newest-write-wins
//! rule reduces to a simple BTreeMap insert.

use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

/// Per-lens reference store keyed by `start` timestamp.
/// Each entry maps `start -> (end, scaled_value)` where `scaled_value` is the
/// temperature × 10 (stored as i64 so we stay in integer arithmetic).
pub struct Oracle {
    lenses: FxHashMap<String, BTreeMap<i64, (i64, i64)>>,
}

impl Oracle {
    pub fn new() -> Self {
        Self {
            lenses: FxHashMap::default(),
        }
    }

    /// Record a batch of `(start, end, value)` intervals for `lens`.
    pub fn append(&mut self, lens: &str, entries: &[(i64, i64, i64)]) {
        let map = self.lenses.entry(lens.to_string()).or_default();
        for &(s, e, v) in entries {
            map.insert(s, (e, v));
        }
    }

    /// Point lookup: return the value covering `t`, if any.
    pub fn at(&self, lens: &str, t: i64) -> Option<i64> {
        let map = self.lenses.get(lens)?;
        let (_, (end, val)) = map.range(..=t).next_back()?;
        if t < *end { Some(*val) } else { None }
    }

    /// Range query: return all `(start, end, value)` segments overlapping
    /// `[rs, re)`, clipped to that window.
    pub fn range(&self, lens: &str, rs: i64, re: i64) -> Vec<(i64, i64, i64)> {
        let Some(map) = self.lenses.get(lens) else {
            return vec![];
        };
        let mut result = Vec::new();
        for (&s, &(e, v)) in map.range(..re) {
            if e > rs {
                result.push((s.max(rs), e.min(re), v));
            }
        }
        result
    }

    /// Aggregate min/max/avg over `[rs, re)`.
    pub fn reduce_min(&self, lens: &str, rs: i64, re: i64) -> Option<i64> {
        self.range(lens, rs, re).iter().map(|(_, _, v)| *v).min()
    }

    pub fn reduce_max(&self, lens: &str, rs: i64, re: i64) -> Option<i64> {
        self.range(lens, rs, re).iter().map(|(_, _, v)| *v).max()
    }

    /// Duration-weighted average over `[rs, re)`, returned as f64.
    pub fn reduce_avg(&self, lens: &str, rs: i64, re: i64) -> Option<f64> {
        let segs = self.range(lens, rs, re);
        if segs.is_empty() {
            return None;
        }
        let total_dur: i64 = segs.iter().map(|(s, e, _)| e - s).sum();
        if total_dur == 0 {
            return None;
        }
        let weighted: f64 = segs
            .iter()
            .map(|(s, e, v)| (e - s) as f64 * *v as f64)
            .sum();
        Some(weighted / total_dur as f64)
    }

    /// Number of midpoints of the first `n` stored segments — for spot-checks.
    pub fn sample_midpoints(&self, lens: &str, n: usize) -> Vec<i64> {
        let Some(map) = self.lenses.get(lens) else {
            return vec![];
        };
        map.iter()
            .take(n)
            .map(|(&s, &(e, _))| s + (e - s) / 2)
            .collect()
    }

    pub fn total_segments(&self) -> usize {
        self.lenses.values().map(|m| m.len()).sum()
    }

    /// Clear one lens (used by fault-injection to reset after a drop).
    pub fn reset(&mut self, lens: &str) {
        self.lenses.remove(lens);
    }
}

impl Default for Oracle {
    fn default() -> Self {
        Self::new()
    }
}
