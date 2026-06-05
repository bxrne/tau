//! Independent reference oracle for the Tau DST workload.
//!
//! This model shares **no code** with `libtau`. Data is stored as raw `Vec<TauInterval>`
//! per layer; every query is a boundary-decomposition scan. A bug in `libtau`'s sweep-line
//! or compaction will diverge from this oracle and be caught by the simulation.
//!
//! Supported operations mirror the SUT: AT, RANGE, REDUCE, CREATE/DROP LENS, DERIVE, TTL,
//! USE DATABASE, and transactions (buffered-mutation approach).

use std::collections::{BTreeSet, HashMap};

use libtau::{AggFunc, Value};

pub type Ts = i64;

/// Fixed "now" for DST — must stay in sync with [`libtau::wall_clock::set_fixed_now_secs`].
pub const DST_NOW_SECS: i64 = 1_700_000_000;

/// Specification for a derived lens (`DERIVE LENS name AS a + b`).
#[derive(Clone, Debug)]
pub struct DeriveSpec {
    pub a: String,
    pub b: String,
}

/// One temporal fact stored in the oracle.
#[derive(Clone, Debug, PartialEq)]
struct TauInterval {
    start: Ts,
    end: Ts,
    value: Value,
}

/// One append layer (higher `id` = newer, wins on overlap).
#[derive(Clone, Debug)]
struct NaiveLayer {
    #[allow(dead_code)]
    id: u64,
    intervals: Vec<TauInterval>,
}

/// Per-lens data.
enum LensData {
    Base {
        layers: Vec<NaiveLayer>,
        ttl_secs: Option<i64>,
    },
    Derived(DeriveSpec),
}

/// Per-database state.
struct OracleDb {
    lenses: HashMap<String, LensData>,
    next_layer_id: u64,
    compact_threshold: usize,
}

impl OracleDb {
    fn with_threshold(compact_threshold: usize) -> Self {
        Self {
            lenses: HashMap::new(),
            next_layer_id: 1,
            compact_threshold,
        }
    }

    fn ttl_cutoff(ttl_secs: Option<i64>, now_secs: Ts) -> Option<Ts> {
        ttl_secs.map(|s| now_secs - s)
    }

    /// Point lookup: newest layer first, linear scan.
    fn at(&self, name: &str, t: Ts, now_secs: Ts) -> Option<Value> {
        Self::at_in(name, t, now_secs, &self.lenses)
    }

    fn at_in(name: &str, t: Ts, now_secs: Ts, lenses: &HashMap<String, LensData>) -> Option<Value> {
        match lenses.get(name)? {
            LensData::Base { layers, ttl_secs } => {
                let cutoff = Self::ttl_cutoff(*ttl_secs, now_secs);
                if cutoff.is_some_and(|c| t < c) {
                    return None;
                }
                for layer in layers.iter().rev() {
                    for tau in &layer.intervals {
                        if tau.start <= t && t < tau.end {
                            return Some(tau.value.clone());
                        }
                    }
                }
                None
            }
            LensData::Derived(spec) => {
                let a = spec.a.clone();
                let b = spec.b.clone();
                let va = Self::at_in(&a, t, now_secs, lenses)?;
                let vb = Self::at_in(&b, t, now_secs, lenses)?;
                match (va, vb) {
                    (Value::Int(x), Value::Int(y)) => Some(Value::Int(x.wrapping_add(y))),
                    _ => None,
                }
            }
        }
    }

    /// Collect all interval boundary points from lens in [qs, qe].
    fn collect_boundaries(
        name: &str,
        qs: Ts,
        qe: Ts,
        now_secs: Ts,
        lenses: &HashMap<String, LensData>,
        pts: &mut BTreeSet<Ts>,
    ) {
        match lenses.get(name) {
            Some(LensData::Base { layers, ttl_secs }) => {
                let cutoff = Self::ttl_cutoff(*ttl_secs, now_secs);
                let effective_qs = cutoff.map_or(qs, |c| qs.max(c));
                if effective_qs != qs {
                    pts.insert(effective_qs);
                }
                for layer in layers {
                    for tau in &layer.intervals {
                        if tau.start < qe && tau.end > effective_qs {
                            if tau.start > effective_qs {
                                pts.insert(tau.start);
                            }
                            if tau.end < qe {
                                pts.insert(tau.end);
                            }
                        }
                    }
                }
            }
            Some(LensData::Derived(spec)) => {
                let a = spec.a.clone();
                let b = spec.b.clone();
                Self::collect_boundaries(&a, qs, qe, now_secs, lenses, pts);
                Self::collect_boundaries(&b, qs, qe, now_secs, lenses, pts);
            }
            None => {}
        }
    }

    /// Range scan: boundary-decomposition with same-value merging.
    fn range(&self, name: &str, qs: Ts, qe: Ts, now_secs: Ts) -> Vec<(Ts, Ts, Value)> {
        if qs >= qe {
            return vec![];
        }
        // Determine effective start after TTL clip
        let effective_qs = match self.lenses.get(name) {
            Some(LensData::Base { ttl_secs, .. }) => {
                Self::ttl_cutoff(*ttl_secs, now_secs).map_or(qs, |c| qs.max(c))
            }
            _ => qs,
        };
        if effective_qs >= qe {
            return vec![];
        }

        let mut pts = BTreeSet::new();
        pts.insert(effective_qs);
        pts.insert(qe);
        Self::collect_boundaries(name, effective_qs, qe, now_secs, &self.lenses, &mut pts);
        let pts: Vec<Ts> = pts.into_iter().collect();

        let mut out: Vec<(Ts, Ts, Value)> = Vec::new();
        for w in pts.windows(2) {
            let (s, e) = (w[0], w[1]);
            let Some(v) = self.at(name, s, now_secs) else {
                continue;
            };
            match out.last_mut() {
                Some(last) if last.2 == v && last.1 == s => last.1 = e,
                _ => out.push((s, e, v)),
            }
        }
        out
    }

    /// Aggregation windows: boundary-decomposition WITHOUT same-value merging.
    /// Returns `(duration, value)` pairs, matching the original oracle's `agg_segments`.
    fn agg_windows(&self, name: &str, qs: Ts, qe: Ts, now_secs: Ts) -> Vec<(Ts, Value)> {
        if qs >= qe {
            return vec![];
        }
        let effective_qs = match self.lenses.get(name) {
            Some(LensData::Base { ttl_secs, .. }) => {
                Self::ttl_cutoff(*ttl_secs, now_secs).map_or(qs, |c| qs.max(c))
            }
            _ => qs,
        };
        if effective_qs >= qe {
            return vec![];
        }

        let mut pts = BTreeSet::new();
        pts.insert(effective_qs);
        pts.insert(qe);
        Self::collect_boundaries(name, effective_qs, qe, now_secs, &self.lenses, &mut pts);
        let pts: Vec<Ts> = pts.into_iter().collect();

        let mut out: Vec<(Ts, Value)> = Vec::new();
        for w in pts.windows(2) {
            let (s, e) = (w[0], w[1]);
            if let Some(v) = self.at(name, s, now_secs) {
                out.push((e - s, v));
            }
        }
        out
    }

    fn reduce(&self, name: &str, qs: Ts, qe: Ts, func: AggFunc, now_secs: Ts) -> Option<Value> {
        let segs = self.agg_windows(name, qs, qe, now_secs);
        if segs.is_empty() {
            return None;
        }
        Some(match func {
            AggFunc::Count => Value::Int(segs.len() as i64),
            AggFunc::Sum => {
                let s: i64 = segs
                    .iter()
                    .filter_map(|(_, v)| {
                        if let Value::Int(i) = v {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                    .fold(0i64, i64::wrapping_add);
                Value::Int(s)
            }
            AggFunc::Min => segs
                .into_iter()
                .map(|(_, v)| v)
                .reduce(|a, b| match (&a, &b) {
                    (Value::Int(x), Value::Int(y)) => {
                        if x <= y {
                            a
                        } else {
                            b
                        }
                    }
                    _ => a,
                })
                .expect("non-empty"),
            AggFunc::Max => segs
                .into_iter()
                .map(|(_, v)| v)
                .reduce(|a, b| match (&a, &b) {
                    (Value::Int(x), Value::Int(y)) => {
                        if x >= y {
                            a
                        } else {
                            b
                        }
                    }
                    _ => a,
                })
                .expect("non-empty"),
            AggFunc::Avg => {
                let total: i64 = segs.iter().map(|(d, _)| *d).sum();
                if total == 0 {
                    return None;
                }
                let w: f64 = segs
                    .iter()
                    .filter_map(|(d, v)| {
                        if let Value::Int(i) = v {
                            Some(*i as f64 * *d as f64)
                        } else {
                            None
                        }
                    })
                    .sum();
                Value::Float(w / total as f64)
            }
        })
    }

    fn append(&mut self, name: &str, taus: Vec<(Ts, Ts, Value)>) {
        if let Some(LensData::Base { layers, .. }) = self.lenses.get_mut(name) {
            let valid: Vec<TauInterval> = taus
                .into_iter()
                .filter(|(s, e, _)| s < e)
                .map(|(start, end, value)| TauInterval { start, end, value })
                .collect();
            if valid.is_empty() {
                return;
            }
            let id = self.next_layer_id;
            self.next_layer_id += 1;
            layers.push(NaiveLayer {
                id,
                intervals: valid,
            });
            if layers.len() > self.compact_threshold {
                Self::compact_layers(layers, &mut self.next_layer_id);
            }
        }
    }

    /// Sweep-line compaction: merge all NaiveLayers into one, newest-wins, adjacent same-value merged.
    fn compact_layers(layers: &mut Vec<NaiveLayer>, id_counter: &mut u64) {
        if layers.len() <= 1 {
            return;
        }
        let mut pts: BTreeSet<Ts> = BTreeSet::new();
        for layer in layers.iter() {
            for tau in &layer.intervals {
                pts.insert(tau.start);
                pts.insert(tau.end);
            }
        }
        let pts: Vec<Ts> = pts.into_iter().collect();

        let mut merged: Vec<TauInterval> = Vec::new();
        for w in pts.windows(2) {
            let (s, e) = (w[0], w[1]);
            let mut found: Option<Value> = None;
            for layer in layers.iter().rev() {
                for tau in &layer.intervals {
                    if tau.start <= s && s < tau.end {
                        found = Some(tau.value.clone());
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            if let Some(v) = found {
                match merged.last_mut() {
                    Some(last) if last.end == s && last.value == v => last.end = e,
                    _ => merged.push(TauInterval {
                        start: s,
                        end: e,
                        value: v,
                    }),
                }
            }
        }

        let new_id = *id_counter;
        *id_counter += 1;
        *layers = if merged.is_empty() {
            vec![]
        } else {
            vec![NaiveLayer {
                id: new_id,
                intervals: merged,
            }]
        };
    }
}

#[derive(Clone)]
enum PendingMutation {
    Append {
        lens: String,
        taus: Vec<(Ts, Ts, Value)>,
    },
    CreateLens {
        name: String,
    },
    DropLens {
        name: String,
    },
    Derive {
        name: String,
        spec: DeriveSpec,
    },
    SetTtl {
        lens: String,
        secs: i64,
    },
    UnsetTtl {
        lens: String,
    },
}

/// Independent reference model for Tau DST.
///
/// Uses a naive `Vec<TauInterval>` per layer with threshold-triggered sweep-line compaction,
/// mirroring libtau's layer management so queries are semantically equivalent.
pub struct Oracle {
    dbs: HashMap<String, OracleDb>,
    active: String,
    pending: Option<Vec<(String, PendingMutation)>>,
    compact_threshold: usize,
}

impl Oracle {
    pub fn new() -> Self {
        Self::with_threshold(libtau::storage::COMPACT_THRESHOLD)
    }

    /// Match the SUT's compact threshold so boundary counts agree.
    pub fn with_threshold(threshold: usize) -> Self {
        let mut dbs = HashMap::new();
        dbs.insert("default".to_string(), OracleDb::with_threshold(threshold));
        Self {
            dbs,
            active: "default".to_string(),
            pending: None,
            compact_threshold: threshold,
        }
    }

    fn now_secs() -> Ts {
        libtau::wall_clock::now_secs()
    }

    fn db(&self) -> &OracleDb {
        self.dbs.get(&self.active).expect("oracle: no active db")
    }

    fn db_mut(&mut self) -> &mut OracleDb {
        self.dbs
            .get_mut(&self.active)
            .expect("oracle: no active db")
    }

    pub fn create_db(&mut self, name: &str) {
        let threshold = self.compact_threshold;
        self.dbs
            .entry(name.to_string())
            .or_insert_with(|| OracleDb::with_threshold(threshold));
    }

    pub fn use_db(&mut self, name: &str) {
        assert!(self.dbs.contains_key(name), "oracle: unknown db '{name}'");
        self.active = name.to_string();
    }

    pub fn create_lens(&mut self, name: &str) {
        self.db_mut()
            .lenses
            .entry(name.to_string())
            .or_insert_with(|| LensData::Base {
                layers: vec![],
                ttl_secs: None,
            });
    }

    pub fn drop_lens(&mut self, name: &str) {
        self.db_mut().lenses.remove(name);
    }

    pub fn derive_lens(&mut self, name: &str, spec: DeriveSpec) {
        self.db_mut()
            .lenses
            .insert(name.to_string(), LensData::Derived(spec));
    }

    pub fn set_ttl(&mut self, lens: &str, secs: i64) {
        if let Some(LensData::Base { ttl_secs, .. }) = self.db_mut().lenses.get_mut(lens) {
            *ttl_secs = Some(secs);
        }
    }

    pub fn unset_ttl(&mut self, lens: &str) {
        if let Some(LensData::Base { ttl_secs, .. }) = self.db_mut().lenses.get_mut(lens) {
            *ttl_secs = None;
        }
    }

    pub fn in_transaction(&self) -> bool {
        self.pending.is_some()
    }

    pub fn start_transaction(&mut self) {
        if self.pending.is_none() {
            self.pending = Some(Vec::new());
        }
    }

    pub fn commit(&mut self) {
        let Some(entries) = self.pending.take() else {
            return;
        };
        let active_before = self.active.clone();
        for (db, mutation) in entries {
            self.active = db;
            self.apply_mutation_now(mutation);
        }
        self.active = active_before;
    }

    pub fn rollback(&mut self) {
        self.pending = None;
    }

    fn buffer_mutation(&mut self, mutation: PendingMutation) {
        if let Some(pending) = &mut self.pending {
            pending.push((self.active.clone(), mutation));
        } else {
            self.apply_mutation_now(mutation);
        }
    }

    fn apply_mutation_now(&mut self, mutation: PendingMutation) {
        match mutation {
            PendingMutation::Append { lens, taus } => {
                self.db_mut().append(&lens, taus);
            }
            PendingMutation::CreateLens { name } => self.create_lens(&name),
            PendingMutation::DropLens { name } => self.drop_lens(&name),
            PendingMutation::Derive { name, spec } => self.derive_lens(&name, spec),
            PendingMutation::SetTtl { lens, secs } => self.set_ttl(&lens, secs),
            PendingMutation::UnsetTtl { lens } => self.unset_ttl(&lens),
        }
    }

    #[allow(dead_code)]
    pub fn append(&mut self, lens: &str, taus: Vec<(Ts, Ts, Value)>) {
        self.buffer_mutation(PendingMutation::Append {
            lens: lens.to_string(),
            taus,
        });
    }

    pub fn at(&self, lens: &str, t: Ts) -> Option<Value> {
        self.db().at(lens, t, Self::now_secs())
    }

    pub fn range(&self, lens: &str, qs: Ts, qe: Ts) -> Vec<(Ts, Ts, Value)> {
        self.db().range(lens, qs, qe, Self::now_secs())
    }

    pub fn reduce(&self, lens: &str, qs: Ts, qe: Ts, func: AggFunc) -> Option<Value> {
        self.db().reduce(lens, qs, qe, func, Self::now_secs())
    }

    pub fn has_lens(&self, name: &str) -> bool {
        self.db().lenses.contains_key(name)
    }

    pub fn active_db(&self) -> &str {
        &self.active
    }

    /// Replay one workload op (including transaction boundaries).
    pub fn apply_op(&mut self, op: &crate::op::Op) {
        use crate::op::Op;
        match op {
            Op::Append { lens, data } => {
                self.buffer_mutation(PendingMutation::Append {
                    lens: lens.clone(),
                    taus: data.to_values(),
                });
            }
            Op::At { lens, t } => {
                let _ = self.at(lens, *t);
            }
            Op::Range { lens, start, end } => {
                if start < end {
                    let _ = self.range(lens, *start, *end);
                }
            }
            Op::Reduce {
                lens,
                start,
                end,
                func,
            } => {
                let _ = self.reduce(lens, *start, *end, *func);
            }
            Op::CreateLens { name, .. } => {
                self.buffer_mutation(PendingMutation::CreateLens { name: name.clone() });
            }
            Op::DropLens { name } => {
                self.buffer_mutation(PendingMutation::DropLens { name: name.clone() });
            }
            Op::Derive { name, spec } => {
                self.buffer_mutation(PendingMutation::Derive {
                    name: name.clone(),
                    spec: spec.clone(),
                });
            }
            Op::Ttl {
                lens,
                secs: Some(s),
            } => {
                self.buffer_mutation(PendingMutation::SetTtl {
                    lens: lens.clone(),
                    secs: *s,
                });
            }
            Op::Ttl { lens, secs: None } => {
                self.buffer_mutation(PendingMutation::UnsetTtl { lens: lens.clone() });
            }
            Op::UseDb(db) => self.use_db(db),
            Op::StartTransaction => self.start_transaction(),
            Op::Commit => self.commit(),
            Op::Rollback => self.rollback(),
        }
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
    use pretty_assertions::assert_eq;

    fn seed_lens(name: &str, taus: Vec<(Ts, Ts, Value)>) -> Oracle {
        let mut o = Oracle::new();
        o.create_lens(name);
        o.append(name, taus);
        o
    }

    #[test]
    fn at_returns_appended_value() {
        let o = seed_lens("x", vec![(0, 100, Value::Int(42))]);
        assert_eq!(o.at("x", 50), Some(Value::Int(42)));
        assert_eq!(o.at("x", 0), Some(Value::Int(42)));
        assert_eq!(o.at("x", 99), Some(Value::Int(42)));
    }

    #[test]
    fn at_misses_outside_interval() {
        let o = seed_lens("x", vec![(10, 50, Value::Int(1))]);
        assert_eq!(o.at("x", 9), None);
        assert_eq!(o.at("x", 50), None);
        assert_eq!(o.at("x", 100), None);
    }

    #[test]
    fn newest_layer_wins() {
        let mut o = Oracle::new();
        o.create_lens("x");
        o.append("x", vec![(0, 100, Value::Int(1))]);
        o.append("x", vec![(25, 75, Value::Int(2))]);
        assert_eq!(o.at("x", 25), Some(Value::Int(2)));
        assert_eq!(o.at("x", 74), Some(Value::Int(2)));
        assert_eq!(o.at("x", 0), Some(Value::Int(1)));
        assert_eq!(o.at("x", 75), Some(Value::Int(1)));
    }

    #[test]
    fn range_returns_non_overlapping_sorted_segments() {
        let o = seed_lens("x", vec![(0, 50, Value::Int(1)), (100, 150, Value::Int(2))]);
        let segs = o.range("x", -10, 200);
        assert_eq!(
            segs,
            vec![(0, 50, Value::Int(1)), (100, 150, Value::Int(2))]
        );
        for w in segs.windows(2) {
            assert!(w[0].1 <= w[1].0);
        }
    }

    #[test]
    fn range_merges_adjacent_same_value() {
        let mut o = Oracle::new();
        o.create_lens("x");
        o.append("x", vec![(0, 100, Value::Int(1))]);
        o.append("x", vec![(25, 50, Value::Int(1))]);
        let segs = o.range("x", 0, 100);
        // After merging adjacent same-value windows the whole range collapses
        assert_eq!(segs, vec![(0, 100, Value::Int(1))]);
    }

    #[test]
    fn derived_lens_visible() {
        let mut o = Oracle::new();
        o.create_lens("a");
        o.create_lens("b");
        o.append("a", vec![(0, 5000, Value::Int(1))]);
        o.append("b", vec![(0, 5000, Value::Int(2))]);
        o.derive_lens(
            "ds",
            DeriveSpec {
                a: "a".into(),
                b: "b".into(),
            },
        );
        assert_eq!(o.at("ds", 1000), Some(Value::Int(3)));
        let segs = o.range("ds", 0, 100);
        assert!(!segs.is_empty());
    }

    #[test]
    fn reduce_count() {
        let mut o = Oracle::new();
        o.create_lens("x");
        for i in 0..4i64 {
            o.append("x", vec![(i * 10, i * 10 + 10, Value::Int(i))]);
        }
        assert_eq!(o.reduce("x", 0, 40, AggFunc::Count), Some(Value::Int(4)));
    }

    #[test]
    fn reduce_sum() {
        let o = seed_lens("x", vec![(0, 1, Value::Int(3)), (1, 2, Value::Int(7))]);
        assert_eq!(o.reduce("x", 0, 2, AggFunc::Sum), Some(Value::Int(10)));
    }

    #[test]
    fn reduce_min_max() {
        let o = seed_lens(
            "x",
            vec![
                (0, 10, Value::Int(5)),
                (10, 20, Value::Int(-3)),
                (20, 30, Value::Int(9)),
            ],
        );
        assert_eq!(o.reduce("x", 0, 30, AggFunc::Min), Some(Value::Int(-3)));
        assert_eq!(o.reduce("x", 0, 30, AggFunc::Max), Some(Value::Int(9)));
    }

    #[test]
    fn reduce_empty_range_returns_none() {
        let o = seed_lens("x", vec![(100, 200, Value::Int(1))]);
        assert_eq!(o.reduce("x", 0, 50, AggFunc::Count), None);
    }

    #[test]
    fn ttl_hides_old_entries() {
        libtau::wall_clock::set_fixed_now_secs(DST_NOW_SECS);
        let mut o = Oracle::new();
        o.create_lens("x");
        o.append("x", vec![(0, 100, Value::Int(99))]);
        o.set_ttl("x", 1);
        assert_eq!(o.at("x", 50), None);
    }

    #[test]
    fn transaction_rollback() {
        let mut o = Oracle::new();
        o.create_lens("x");
        o.append("x", vec![(0, 10, Value::Int(1))]);
        o.start_transaction();
        o.append("x", vec![(10, 20, Value::Int(2))]);
        o.rollback();
        assert_eq!(o.at("x", 15), None);
    }

    #[test]
    fn transaction_commit() {
        let mut o = Oracle::new();
        o.create_lens("x");
        o.start_transaction();
        o.append("x", vec![(0, 10, Value::Int(42))]);
        o.commit();
        assert_eq!(o.at("x", 5), Some(Value::Int(42)));
    }

    #[hegel::test]
    fn pbt_at_appended_value_visible(tc: TestCase) {
        let s = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
        let span = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let e = s + span;
        let v = tc.draw(gs::integers::<i64>());
        let o = seed_lens("x", vec![(s, e, Value::Int(v))]);
        let t = tc.draw(gs::integers::<i64>().min_value(s).max_value(e - 1));
        assert_eq!(o.at("x", t), Some(Value::Int(v)));
        assert_eq!(o.at("x", s - 1), None);
        assert_eq!(o.at("x", e), None);
    }

    #[hegel::test]
    fn pbt_range_segments_non_overlapping_sorted(tc: TestCase) {
        let mut o = Oracle::new();
        o.create_lens("x");
        let mut cur = 0i64;
        for _ in 0..8 {
            let span = tc.draw(gs::integers::<i64>().min_value(1).max_value(100));
            let v = tc.draw(gs::integers::<i64>());
            o.append("x", vec![(cur, cur + span, Value::Int(v))]);
            cur += span;
        }
        let segs = o.range("x", -10, cur + 10);
        for w in segs.windows(2) {
            assert!(w[0].1 <= w[1].0, "segments must not overlap");
            assert!(w[0].0 < w[1].0, "segments must be ordered by start");
        }
    }

    #[hegel::test]
    fn pbt_newest_layer_wins_at_overlap(tc: TestCase) {
        let s = tc.draw(gs::integers::<i64>().min_value(0).max_value(500));
        let e = s + tc.draw(gs::integers::<i64>().min_value(10).max_value(100));
        let v1 = tc.draw(gs::integers::<i64>());
        let v2 = tc.draw(gs::integers::<i64>());
        let mut o = Oracle::new();
        o.create_lens("x");
        o.append("x", vec![(s, e, Value::Int(v1))]);
        o.append("x", vec![(s, e, Value::Int(v2))]);
        let t = tc.draw(gs::integers::<i64>().min_value(s).max_value(e - 1));
        assert_eq!(o.at("x", t), Some(Value::Int(v2)));
    }
}
