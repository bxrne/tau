//! Named, seeded TauQL workloads shared by the engine and wire bench layers.
//!
//! Each workload is a `setup` sequence (run once, untimed) followed by a
//! `measure` sequence (the timed operations). Both sequences are plain TauQL
//! text so they can be replayed identically against `libtau::Executor` (the
//! engine layer) or over a TCP socket (the wire layer).

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

pub const DEFAULT_SEED: u64 = 42;
const DB: &str = "bench";
const LENS: &str = "m";
const DERIVED: &str = "derived";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ValueType {
    Int,
    Float,
    Str,
    Bool,
}

impl ValueType {
    pub fn type_kw(&self) -> &'static str {
        match self {
            ValueType::Int => "int",
            ValueType::Float => "float",
            ValueType::Str => "str",
            ValueType::Bool => "bool",
        }
    }

    fn literal(&self, rng: &mut StdRng, idx: usize) -> String {
        match self {
            ValueType::Int => rng.gen_range(-1_000_000..1_000_000).to_string(),
            ValueType::Float => format!("{:.3}", rng.gen_range(-1_000.0..1_000.0)),
            ValueType::Str => format!("\"v{}\"", idx % 1024),
            ValueType::Bool => {
                if rng.gen_bool(0.5) {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WorkloadKind {
    AppendHeavy,
    CorrectionHeavy,
    PointQuery,
    RangeScan,
    ReduceAgg,
    DerivedLens,
    CompactionStress,
}

impl WorkloadKind {
    pub fn all() -> &'static [WorkloadKind] {
        &[
            WorkloadKind::AppendHeavy,
            WorkloadKind::CorrectionHeavy,
            WorkloadKind::PointQuery,
            WorkloadKind::RangeScan,
            WorkloadKind::ReduceAgg,
            WorkloadKind::DerivedLens,
            WorkloadKind::CompactionStress,
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            WorkloadKind::AppendHeavy => "append_heavy",
            WorkloadKind::CorrectionHeavy => "correction_heavy",
            WorkloadKind::PointQuery => "point_query",
            WorkloadKind::RangeScan => "range_scan",
            WorkloadKind::ReduceAgg => "reduce_agg",
            WorkloadKind::DerivedLens => "derived_lens",
            WorkloadKind::CompactionStress => "compaction_stress",
        }
    }

    /// `reduce_agg` and `derived_lens` need an arithmetic value type. Other
    /// workloads honour the requested type; these two always use `Int`.
    pub fn effective_value_type(&self, requested: ValueType) -> ValueType {
        match self {
            WorkloadKind::ReduceAgg | WorkloadKind::DerivedLens => ValueType::Int,
            _ => requested,
        }
    }

    pub fn build(&self, scale: usize, vt: ValueType, seed: u64) -> Workload {
        let vt = self.effective_value_type(vt);
        match self {
            WorkloadKind::AppendHeavy => append_heavy(scale, vt, seed),
            WorkloadKind::CorrectionHeavy => correction_heavy(scale, vt, seed),
            WorkloadKind::PointQuery => point_query(scale, vt, seed),
            WorkloadKind::RangeScan => range_scan(scale, vt, seed),
            WorkloadKind::ReduceAgg => reduce_agg(scale, vt, seed),
            WorkloadKind::DerivedLens => derived_lens(scale, vt, seed),
            WorkloadKind::CompactionStress => compaction_stress(scale, vt, seed),
        }
    }
}

/// A workload's setup (untimed) and measure (timed) TauQL statement sequences.
#[derive(Debug, Clone)]
pub struct Workload {
    pub name: &'static str,
    pub setup: Vec<String>,
    pub measure: Vec<String>,
}

fn base_setup(vt: ValueType) -> Vec<String> {
    vec![
        format!("CREATE DATABASE {DB}"),
        format!("USE DATABASE {DB}"),
        format!("CREATE LENS {LENS} {}", vt.type_kw()),
    ]
}

/// Append `n` non-overlapping taus into the populated lens, used as setup
/// for the query workloads.
fn populate(rng: &mut StdRng, vt: ValueType, n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            let start = i as i64 * 10;
            let end = start + 10;
            format!("APPEND LENS {LENS} {start} {end} {}", vt.literal(rng, i))
        })
        .collect()
}

/// Many fresh, non-overlapping appends into one lens.
fn append_heavy(scale: usize, vt: ValueType, seed: u64) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let setup = base_setup(vt);
    let measure = (0..scale)
        .map(|i| {
            let start = i as i64 * 10;
            let end = start + 10;
            format!(
                "APPEND LENS {LENS} {start} {end} {}",
                vt.literal(&mut rng, i)
            )
        })
        .collect();
    Workload {
        name: "append_heavy",
        setup,
        measure,
    }
}

/// Repeated overlapping appends over the same range, building a deep
/// newest-layer-wins stack.
fn correction_heavy(scale: usize, vt: ValueType, seed: u64) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let span = (scale as i64 * 10).max(10);
    let mut setup = base_setup(vt);
    setup.push(format!(
        "APPEND LENS {LENS} 0 {span} {}",
        vt.literal(&mut rng, 0)
    ));
    let measure = (0..scale)
        .map(|i| {
            format!(
                "APPEND LENS {LENS} 0 {span} {}",
                vt.literal(&mut rng, i + 1)
            )
        })
        .collect();
    Workload {
        name: "correction_heavy",
        setup,
        measure,
    }
}

/// `AT` point lookups fanned out across a populated lens.
fn point_query(scale: usize, vt: ValueType, seed: u64) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut setup = base_setup(vt);
    setup.extend(populate(&mut rng, vt, scale));
    let domain = (scale as i64 * 10).max(1);
    let measure = (0..scale)
        .map(|_| format!("AT LENS {LENS} {}", rng.gen_range(0..domain)))
        .collect();
    Workload {
        name: "point_query",
        setup,
        measure,
    }
}

/// Wide `RANGE` scans over a populated lens.
fn range_scan(scale: usize, vt: ValueType, seed: u64) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut setup = base_setup(vt);
    setup.extend(populate(&mut rng, vt, scale));
    let domain = (scale as i64 * 10).max(1);
    let window = (domain / 2).max(1);
    let n = (scale / 10).max(1);
    let measure = (0..n)
        .map(|_| {
            let start = rng.gen_range(0..=(domain - window).max(1));
            format!("RANGE LENS {LENS} {start} {}", start + window)
        })
        .collect();
    Workload {
        name: "range_scan",
        setup,
        measure,
    }
}

/// `REDUCE` aggregates over a populated lens, cycling through every
/// aggregation function. Always uses `int`.
fn reduce_agg(scale: usize, vt: ValueType, seed: u64) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut setup = base_setup(vt);
    setup.extend(populate(&mut rng, vt, scale));
    let domain = (scale as i64 * 10).max(1);
    let window = (domain / 2).max(1);
    let n = (scale / 10).max(1);
    let funcs = ["min", "max", "avg", "sum", "count"];
    let measure = (0..n)
        .map(|i| {
            let start = rng.gen_range(0..=(domain - window).max(1));
            let func = funcs[i % funcs.len()];
            format!("REDUCE LENS {LENS} {start} {} USING {func}", start + window)
        })
        .collect();
    Workload {
        name: "reduce_agg",
        setup,
        measure,
    }
}

/// Queries against a derived lens (lazy expression evaluation, no caching).
/// Always uses `int`.
fn derived_lens(scale: usize, vt: ValueType, seed: u64) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut setup = base_setup(vt);
    setup.extend(populate(&mut rng, vt, scale));
    setup.push(format!("DERIVE LENS {DERIVED} AS ({LENS} * 2)"));
    let domain = (scale as i64 * 10).max(1);
    let measure = (0..scale)
        .map(|_| format!("AT LENS {DERIVED} {}", rng.gen_range(0..domain)))
        .collect();
    Workload {
        name: "derived_lens",
        setup,
        measure,
    }
}

/// Many small appends into a narrow rotating set of ranges, intended to be
/// run with a low `compact_threshold` so compaction's sweep-line runs
/// repeatedly during `measure`.
fn compaction_stress(scale: usize, vt: ValueType, seed: u64) -> Workload {
    let mut rng = StdRng::seed_from_u64(seed);
    let setup = base_setup(vt);
    let buckets = 16i64;
    let measure = (0..scale)
        .map(|i| {
            let bucket = i as i64 % buckets;
            let start = bucket * 10;
            format!(
                "APPEND LENS {LENS} {start} {} {}",
                start + 10,
                vt.literal(&mut rng, i)
            )
        })
        .collect();
    Workload {
        name: "compaction_stress",
        setup,
        measure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_scale_is_deterministic() {
        for kind in WorkloadKind::all() {
            let a = kind.build(20, ValueType::Int, DEFAULT_SEED);
            let b = kind.build(20, ValueType::Int, DEFAULT_SEED);
            assert_eq!(a.setup, b.setup, "{} setup diverged", kind.name());
            assert_eq!(a.measure, b.measure, "{} measure diverged", kind.name());
        }
    }

    #[test]
    fn different_seed_changes_measure() {
        let a = WorkloadKind::AppendHeavy.build(20, ValueType::Int, 1);
        let b = WorkloadKind::AppendHeavy.build(20, ValueType::Int, 2);
        assert_ne!(a.measure, b.measure);
    }

    #[test]
    fn every_workload_produces_nonempty_setup_and_measure() {
        for kind in WorkloadKind::all() {
            for vt in [
                ValueType::Int,
                ValueType::Float,
                ValueType::Str,
                ValueType::Bool,
            ] {
                let w = kind.build(10, vt, DEFAULT_SEED);
                assert!(!w.setup.is_empty(), "{} setup empty", kind.name());
                assert!(!w.measure.is_empty(), "{} measure empty", kind.name());
            }
        }
    }
}
