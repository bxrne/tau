use std::collections::{BTreeMap, HashMap};

// Reference implementation: O(log n) BTreeMap per lens.
// Keys are start timestamps; each entry maps to (end, value).
// Newest-wins reduces to last-write-wins on unique start keys since
// time_cursor advances monotonically so intervals never overlap.
pub(crate) struct Oracle {
    lenses: HashMap<String, BTreeMap<i64, (i64, i64)>>,
}

impl Oracle {
    pub(crate) fn new() -> Self {
        Self {
            lenses: HashMap::new(),
        }
    }

    pub(crate) fn append(&mut self, lens: &str, entries: &[(i64, i64, i64)]) {
        let map = self.lenses.entry(lens.to_string()).or_default();
        for &(s, e, v) in entries {
            map.insert(s, (e, v));
        }
    }

    pub(crate) fn at(&self, lens: &str, t: i64) -> Option<i64> {
        let map = self.lenses.get(lens)?;
        let (_, (end, val)) = map.range(..=t).next_back()?;
        if t < *end { Some(*val) } else { None }
    }

    pub(crate) fn reset(&mut self, lens: &str) {
        self.lenses.remove(lens);
    }

    pub(crate) fn total_segments(&self) -> usize {
        self.lenses.values().map(|m| m.len()).sum()
    }
}
