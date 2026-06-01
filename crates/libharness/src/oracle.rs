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

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    fn lens_name() -> impl Generator<String> {
        gs::from_regex("[a-z][a-z0-9]{0,8}").fullmatch(true)
    }

    #[hegel::test]
    fn at_returns_value_inside_interval(tc: TestCase) {
        let start = tc.draw(gs::integers::<i64>().min_value(0).max_value(1000));
        let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(100));
        let v = tc.draw(gs::integers::<i64>().min_value(-500).max_value(500));
        let t = tc.draw(
            gs::integers::<i64>()
                .min_value(start)
                .max_value(start + len - 1),
        );
        let mut o = Oracle::new();
        o.append("x", &[(start, start + len, v)]);
        assert_eq!(o.at("x", t), Some(v));
    }

    #[hegel::test]
    fn at_returns_none_at_end_boundary(tc: TestCase) {
        let start = tc.draw(gs::integers::<i64>().min_value(0).max_value(1000));
        let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(100));
        let v = tc.draw(gs::integers::<i64>());
        let mut o = Oracle::new();
        o.append("x", &[(start, start + len, v)]);
        assert_eq!(o.at("x", start + len), None);
    }

    #[hegel::test]
    fn at_returns_none_for_unknown_lens(tc: TestCase) {
        let name = tc.draw(lens_name());
        let o = Oracle::new();
        assert_eq!(o.at(&name, 0), None);
    }

    #[hegel::test]
    fn append_newest_write_wins_for_same_start(tc: TestCase) {
        let v1 = tc.draw(gs::integers::<i64>().min_value(-100).max_value(100));
        let v2 = tc.draw(gs::integers::<i64>().min_value(-100).max_value(100));
        let mut o = Oracle::new();
        o.append("x", &[(0, 10, v1)]);
        o.append("x", &[(0, 10, v2)]);
        assert_eq!(o.at("x", 5), Some(v2));
    }

    #[hegel::test]
    fn range_returns_all_overlapping_segments(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(10));
        let v = tc.draw(gs::integers::<i64>().min_value(-100).max_value(100));
        let mut o = Oracle::new();
        let entries: Vec<(i64, i64, i64)> = (0..n as i64).map(|i| (i * 2, i * 2 + 1, v)).collect();
        o.append("x", &entries);
        let result = o.range("x", 0, n as i64 * 2);
        assert_eq!(result.len(), n);
    }

    #[hegel::test]
    fn range_clips_to_query_window(tc: TestCase) {
        let v = tc.draw(gs::integers::<i64>());
        let mut o = Oracle::new();
        o.append("x", &[(0, 100, v)]);
        let result = o.range("x", 20, 50);
        assert_eq!(result, vec![(20, 50, v)]);
    }

    #[hegel::test]
    fn range_empty_for_non_overlapping_window(tc: TestCase) {
        let v = tc.draw(gs::integers::<i64>());
        let mut o = Oracle::new();
        o.append("x", &[(0, 10, v)]);
        assert!(o.range("x", 20, 30).is_empty());
    }

    #[hegel::test]
    fn reduce_min_returns_minimum_value(tc: TestCase) {
        let vals = tc.draw(
            gs::vecs(gs::integers::<i64>().min_value(-200).max_value(200))
                .min_size(1)
                .max_size(10),
        );
        let mut o = Oracle::new();
        let entries: Vec<(i64, i64, i64)> = vals
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as i64, i as i64 + 1, v))
            .collect();
        o.append("x", &entries);
        let n = vals.len() as i64;
        assert_eq!(o.reduce_min("x", 0, n), vals.iter().copied().min());
    }

    #[hegel::test]
    fn reduce_max_returns_maximum_value(tc: TestCase) {
        let vals = tc.draw(
            gs::vecs(gs::integers::<i64>().min_value(-200).max_value(200))
                .min_size(1)
                .max_size(10),
        );
        let mut o = Oracle::new();
        let entries: Vec<(i64, i64, i64)> = vals
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as i64, i as i64 + 1, v))
            .collect();
        o.append("x", &entries);
        let n = vals.len() as i64;
        assert_eq!(o.reduce_max("x", 0, n), vals.iter().copied().max());
    }

    #[hegel::test]
    fn reduce_avg_equal_duration_segments_is_arithmetic_mean(tc: TestCase) {
        let vals = tc.draw(
            gs::vecs(gs::integers::<i64>().min_value(-100).max_value(100))
                .min_size(2)
                .max_size(8),
        );
        let mut o = Oracle::new();
        let entries: Vec<(i64, i64, i64)> = vals
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as i64, i as i64 + 1, v))
            .collect();
        o.append("x", &entries);
        let n = vals.len() as i64;
        let avg = o.reduce_avg("x", 0, n).expect("non-empty");
        let expected = vals.iter().map(|&v| v as f64).sum::<f64>() / vals.len() as f64;
        assert!(
            (avg - expected).abs() < 1e-9,
            "avg={avg} expected={expected}"
        );
    }

    #[hegel::test]
    fn reset_clears_lens_data(tc: TestCase) {
        let name = tc.draw(lens_name());
        let v = tc.draw(gs::integers::<i64>());
        let mut o = Oracle::new();
        o.append(&name, &[(0, 10, v)]);
        o.reset(&name);
        assert_eq!(o.at(&name, 5), None);
        assert!(o.range(&name, 0, 10).is_empty());
    }

    #[hegel::test]
    fn sample_midpoints_returns_at_most_n(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(15));
        let mut o = Oracle::new();
        let entries: Vec<(i64, i64, i64)> =
            (0..count as i64).map(|i| (i * 3, i * 3 + 2, i)).collect();
        o.append("x", &entries);
        assert!(o.sample_midpoints("x", n).len() <= n.min(count));
    }

    #[hegel::test]
    fn total_segments_sums_all_lenses(tc: TestCase) {
        let a = tc.draw(gs::integers::<usize>().min_value(0).max_value(5));
        let b = tc.draw(gs::integers::<usize>().min_value(0).max_value(5));
        let mut o = Oracle::new();
        let ea: Vec<(i64, i64, i64)> = (0..a as i64).map(|i| (i, i + 1, i)).collect();
        let eb: Vec<(i64, i64, i64)> = (0..b as i64).map(|i| (i + 100, i + 101, i)).collect();
        o.append("a", &ea);
        o.append("b", &eb);
        assert_eq!(o.total_segments(), a + b);
    }
}
