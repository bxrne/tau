//! Tau workload behavior tree (built on [`libdst::btree::Tree`]).
//!
//! The static [`TAU_TREE`] uses Arc closures — no function-pointer constraints.
//! [`SimCtx`] holds a raw pointer so it can be `Send + Sync` as a type parameter of the
//! static tree; the pointer is always valid because `pick` is a short-lived stack-local call.

use std::sync::LazyLock;

use libdst::btree::{Leaf, Tree};
use libtau::AggFunc;
use rand::Rng;
use rand::rngs::StdRng;

use crate::op::{self, BOOL, DS, FL, INT, Lens, Op, Payload, SV, XD};
use crate::oracle::{DeriveSpec, Oracle};

/// Tag bits for the Tau behavior tree.
pub mod tags {
    /// Leaves with this tag are excluded when a WAL workload is active.
    pub const WAL_EXCLUDED: u64 = 1 << 0;
}

/// Simulation context passed to behavior-tree guards and builders.
///
/// Holds a raw pointer to the oracle so `SimCtx` satisfies `Send + Sync` as a
/// type-parameter of the `static` tree. The pointer is valid for the duration of
/// each `pick` call — `SimCtx` is only ever a stack-local value.
pub struct SimCtx {
    oracle_ptr: *const Oracle,
    pub in_transaction: bool,
}

// SAFETY: SimCtx is a stack-local value created and destroyed within a single `pick` call.
// The raw pointer is valid for that duration. No concurrent access occurs.
unsafe impl Send for SimCtx {}
unsafe impl Sync for SimCtx {}

impl SimCtx {
    pub fn new(oracle: &Oracle, in_transaction: bool) -> Self {
        Self {
            oracle_ptr: oracle as *const Oracle,
            in_transaction,
        }
    }

    pub fn oracle(&self) -> &Oracle {
        // SAFETY: pointer is valid for the duration of the enclosing `pick` call.
        unsafe { &*self.oracle_ptr }
    }
}

static TAU_TREE: LazyLock<Tree<SimCtx, Op>> = LazyLock::new(build_tree);

#[allow(clippy::too_many_lines)]
fn build_tree() -> Tree<SimCtx, Op> {
    Tree::new()
        // Append int — default DB
        .leaf(Leaf::new(
            30,
            0,
            |c: &SimCtx| c.oracle().active_db() == "default",
            |rng, _| Op::Append {
                lens: op::rand_int_lens(rng),
                data: {
                    let n = rng.gen_range(1..=6);
                    Payload::Int(op::gen_int_taus(rng, n))
                },
            },
        ))
        // Append float
        .leaf(Leaf::new(
            5,
            0,
            |c: &SimCtx| c.oracle().active_db() == "default" && c.oracle().has_lens(FL),
            |rng, _| Op::Append {
                lens: FL.to_string(),
                data: {
                    let n = rng.gen_range(1..=4);
                    Payload::Float(op::gen_float_taus(rng, n))
                },
            },
        ))
        // Append bool
        .leaf(Leaf::new(
            3,
            0,
            |c: &SimCtx| c.oracle().active_db() == "default" && c.oracle().has_lens(BOOL),
            |rng, _| Op::Append {
                lens: BOOL.to_string(),
                data: {
                    let n = rng.gen_range(1..=4);
                    Payload::Bool(op::gen_bool_taus(rng, n))
                },
            },
        ))
        // Append str
        .leaf(Leaf::new(
            3,
            0,
            |c: &SimCtx| c.oracle().active_db() == "default" && c.oracle().has_lens(SV),
            |rng, _| Op::Append {
                lens: SV.to_string(),
                data: {
                    let n = rng.gen_range(1..=4);
                    Payload::Str(op::gen_str_taus(rng, n))
                },
            },
        ))
        // Append int — aux DB (WAL-excluded: aux is multi-DB, WAL profiles are single-DB)
        .leaf(Leaf::new(
            15,
            tags::WAL_EXCLUDED,
            |c: &SimCtx| c.oracle().active_db() != "default",
            |rng, _| Op::Append {
                lens: Lens::Aux.as_str().to_string(),
                data: {
                    let n = rng.gen_range(1..=6);
                    Payload::Int(op::gen_int_taus(rng, n))
                },
            },
        ))
        .leaf(Leaf::new(
            2,
            tags::WAL_EXCLUDED,
            |c: &SimCtx| !c.in_transaction && c.oracle().active_db() == "default",
            |_, _| Op::StartTransaction,
        ))
        // Append inside transaction
        .leaf(Leaf::new(
            2,
            tags::WAL_EXCLUDED,
            |c: &SimCtx| c.in_transaction,
            |rng, _| {
                let n = rng.gen_range(1..=4);
                Op::Append {
                    lens: op::rand_int_lens(rng),
                    data: Payload::Int(op::gen_int_taus(rng, n)),
                }
            },
        ))
        .leaf(Leaf::new(
            2,
            tags::WAL_EXCLUDED,
            |c: &SimCtx| c.in_transaction,
            |_, _| Op::Commit,
        ))
        .leaf(Leaf::new(
            1,
            tags::WAL_EXCLUDED,
            |c: &SimCtx| c.in_transaction,
            |_, _| Op::Rollback,
        ))
        .leaf(Leaf::new(
            9,
            0,
            |c: &SimCtx| !c.in_transaction,
            |rng, c| Op::At {
                lens: op::int_lens_for_db(c.oracle(), rng),
                t: rng.gen_range(-50..3000),
            },
        ))
        .leaf(Leaf::new(
            3,
            0,
            |c: &SimCtx| {
                !c.in_transaction && c.oracle().active_db() == "default" && c.oracle().has_lens(DS)
            },
            |rng, _| Op::At {
                lens: DS.to_string(),
                t: rng.gen_range(-50..3000),
            },
        ))
        .leaf(Leaf::new(
            3,
            0,
            |c: &SimCtx| !c.in_transaction && c.oracle().active_db() == "default",
            |rng, c| {
                let o = c.oracle();
                let lens = if o.has_lens(FL) && rng.gen_bool(0.5) {
                    FL
                } else if o.has_lens(BOOL) && rng.gen_bool(0.5) {
                    BOOL
                } else if o.has_lens(SV) {
                    SV
                } else {
                    INT[rng.gen_range(0..INT.len())]
                };
                Op::At {
                    lens: lens.to_string(),
                    t: rng.gen_range(-50..3000),
                }
            },
        ))
        .leaf(Leaf::new(
            15,
            0,
            |c: &SimCtx| !c.in_transaction,
            |rng, c| {
                let s = rng.gen_range(-50..2500);
                Op::Range {
                    lens: op::int_lens_for_db(c.oracle(), rng),
                    start: s,
                    end: s + rng.gen_range(1..500),
                }
            },
        ))
        .leaf(Leaf::new(
            3,
            0,
            |c: &SimCtx| {
                !c.in_transaction && c.oracle().active_db() == "default" && c.oracle().has_lens(DS)
            },
            |rng, _| {
                let s = rng.gen_range(-50..2500);
                Op::Range {
                    lens: DS.to_string(),
                    start: s,
                    end: s + rng.gen_range(1..500),
                }
            },
        ))
        .leaf(Leaf::new(
            5,
            0,
            |c: &SimCtx| !c.in_transaction && c.oracle().active_db() == "default",
            |rng, _| {
                let s = rng.gen_range(-50..2000);
                let func = match rng.gen_range(0..4u8) {
                    0 => AggFunc::Count,
                    1 => AggFunc::Sum,
                    2 => AggFunc::Min,
                    _ => AggFunc::Max,
                };
                Op::Reduce {
                    lens: op::rand_int_lens(rng),
                    start: s,
                    end: s + rng.gen_range(1..500),
                    func,
                }
            },
        ))
        .leaf(Leaf::new(
            3,
            0,
            |c: &SimCtx| !c.in_transaction && c.oracle().active_db() == "default",
            |rng, c| {
                let (lens, ty) = Lens::DYN[rng.gen_range(0..Lens::DYN.len())];
                let name = lens.as_str();
                if c.oracle().has_lens(name) {
                    Op::DropLens {
                        name: name.to_string(),
                    }
                } else {
                    Op::CreateLens {
                        name: name.to_string(),
                        ty,
                    }
                }
            },
        ))
        .leaf(Leaf::new(
            2,
            0,
            |c: &SimCtx| {
                !c.in_transaction
                    && c.oracle().active_db() == "default"
                    && !c.oracle().has_lens(DS)
                    && c.oracle().has_lens("a")
                    && c.oracle().has_lens("b")
            },
            |_, _| Op::Derive {
                name: DS.to_string(),
                spec: DeriveSpec {
                    a: "a".into(),
                    b: "b".into(),
                },
            },
        ))
        .leaf(Leaf::new(
            1,
            0,
            |c: &SimCtx| {
                !c.in_transaction && c.oracle().active_db() == "default" && c.oracle().has_lens(DS)
            },
            |_, _| Op::DropLens {
                name: DS.to_string(),
            },
        ))
        // XDERIVE: materialise xd = a + b (optionally bounded by OVER).
        .leaf(Leaf::new(
            2,
            0,
            |c: &SimCtx| {
                !c.in_transaction
                    && c.oracle().active_db() == "default"
                    && !c.oracle().has_lens(XD)
                    && c.oracle().has_lens("a")
                    && c.oracle().has_lens("b")
            },
            |rng, _| Op::Xderive {
                name: XD.to_string(),
                spec: DeriveSpec {
                    a: "a".into(),
                    b: "b".into(),
                },
                range: if rng.gen_bool(0.5) {
                    let s = rng.gen_range(-50..1500);
                    Some((s, s + rng.gen_range(1..1500)))
                } else {
                    None
                },
            },
        ))
        .leaf(Leaf::new(
            1,
            0,
            |c: &SimCtx| {
                !c.in_transaction && c.oracle().active_db() == "default" && c.oracle().has_lens(XD)
            },
            |_, _| Op::DropLens {
                name: XD.to_string(),
            },
        ))
        // AT / RANGE against the materialised lens.
        .leaf(Leaf::new(
            3,
            0,
            |c: &SimCtx| {
                !c.in_transaction && c.oracle().active_db() == "default" && c.oracle().has_lens(XD)
            },
            |rng, _| Op::At {
                lens: XD.to_string(),
                t: rng.gen_range(-50..3000),
            },
        ))
        .leaf(Leaf::new(
            3,
            0,
            |c: &SimCtx| {
                !c.in_transaction && c.oracle().active_db() == "default" && c.oracle().has_lens(XD)
            },
            |rng, _| {
                let s = rng.gen_range(-50..2500);
                Op::Range {
                    lens: XD.to_string(),
                    start: s,
                    end: s + rng.gen_range(1..500),
                }
            },
        ))
        // USE aux
        .leaf(Leaf::new(
            2,
            tags::WAL_EXCLUDED,
            |c: &SimCtx| !c.in_transaction && c.oracle().active_db() == "default",
            |_, _| Op::UseDb("aux"),
        ))
        // USE default
        .leaf(Leaf::new(
            5,
            tags::WAL_EXCLUDED,
            |c: &SimCtx| c.oracle().active_db() != "default",
            |_, _| Op::UseDb("default"),
        ))
        // AT extreme timestamps
        .leaf(Leaf::new(
            5,
            0,
            |c: &SimCtx| !c.in_transaction,
            |rng, c| Op::At {
                lens: op::int_lens_for_db(c.oracle(), rng),
                t: op::extreme_ts(rng),
            },
        ))
        // RANGE extreme timestamps
        .leaf(Leaf::new(
            7,
            0,
            |c: &SimCtx| !c.in_transaction,
            |rng, c| {
                let s = op::extreme_ts(rng);
                Op::Range {
                    lens: op::int_lens_for_db(c.oracle(), rng),
                    start: s,
                    end: s.saturating_add(rng.gen_range(1..1000)),
                }
            },
        ))
}

/// Pick the next Tau operation using the weighted behavior tree.
pub fn pick(rng: &mut StdRng, oracle: &Oracle, wal: bool, in_transaction: bool) -> Op {
    let ctx = SimCtx::new(oracle, in_transaction);
    let excluded = if wal { tags::WAL_EXCLUDED } else { 0 };
    TAU_TREE.pick(rng, &ctx, excluded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::MEMORY_MULTI;
    use hegel::TestCase;
    use rand::SeedableRng;

    #[test]
    fn pick_always_returns_when_default_bootstrapped() {
        let paths = crate::profile::ProfileWorkspace::new(MEMORY_MULTI).paths;
        let o = MEMORY_MULTI.bootstrap_oracle(&paths);
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..50 {
            let _ = pick(&mut rng, &o, false, false);
        }
    }

    #[hegel::test]
    fn pbt_pick_respects_derive_guard(tc: TestCase) {
        use hegel::generators as gs;
        let paths = crate::profile::ProfileWorkspace::new(MEMORY_MULTI).paths;
        let mut o = MEMORY_MULTI.bootstrap_oracle(&paths);
        let has_ds = tc.draw(gs::booleans());
        if has_ds {
            o.derive_lens(
                DS,
                DeriveSpec {
                    a: "a".into(),
                    b: "b".into(),
                },
            );
        }
        let mut rng = StdRng::seed_from_u64(tc.draw(gs::integers::<u64>()));
        for _ in 0..20 {
            let op = pick(&mut rng, &o, false, false);
            match (&op, has_ds) {
                (Op::Derive { name, .. }, false) => assert_eq!(name, DS),
                (Op::DropLens { name }, true) if name == DS => {}
                _ => {}
            }
        }
    }

    #[test]
    fn xderive_is_reachable_when_sources_exist() {
        // With source lenses a and b present and no XD lens yet, the behavior
        // tree must be able to emit an XDERIVE op — otherwise materialised
        // lenses would silently never be exercised by the simulation.
        let paths = crate::profile::ProfileWorkspace::new(MEMORY_MULTI).paths;
        let o = MEMORY_MULTI.bootstrap_oracle(&paths);
        let mut rng = StdRng::seed_from_u64(7);
        let saw_xderive = (0..3000).any(
            |_| matches!(pick(&mut rng, &o, false, false), Op::Xderive { name, .. } if name == XD),
        );
        assert!(saw_xderive, "behavior tree never generated an XDERIVE op");
    }

    #[test]
    fn derive_not_picked_inside_transaction() {
        let mut o = Oracle::new();
        o.create_lens("a");
        o.create_lens("b");
        o.start_transaction();
        let mut rng = StdRng::seed_from_u64(99);
        for _ in 0..200 {
            let op = pick(&mut rng, &o, false, true);
            assert!(
                !matches!(
                    op,
                    Op::Derive { .. }
                        | Op::Xderive { .. }
                        | Op::CreateLens { .. }
                        | Op::DropLens { .. }
                ),
                "registry ops must not be picked in transaction: {op:?}"
            );
        }
    }

    #[test]
    fn wal_mode_excludes_tagged_ops() {
        let paths = crate::profile::ProfileWorkspace::new(MEMORY_MULTI).paths;
        let mut o = MEMORY_MULTI.bootstrap_oracle(&paths);
        o.use_db("aux");
        let mut rng = StdRng::seed_from_u64(99);
        for _ in 0..100 {
            match pick(&mut rng, &o, true, false) {
                Op::UseDb(db) => assert_ne!(db, "aux"),
                Op::Append { lens, .. } => assert_ne!(lens, Lens::Aux.as_str()),
                _ => {}
            }
        }
    }
}
