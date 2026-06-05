//! Closure-based weighted behavior-tree selector for DST workloads.
//!
//! Each [`Leaf`] combines a guard, a weight, exclusion tags, and a builder.
//! The root [`Tree`] picks uniformly among eligible leaves proportional to weight.
//! Guards and builders are heap-allocated closures — no function-pointer constraints,
//! no domain-specific field names, no `unsafe`.

use std::sync::Arc;

use rand::Rng;
use rand::rngs::StdRng;

/// One weighted, tagged operation generator.
pub struct Leaf<Ctx, Op> {
    /// Relative selection weight (higher = more likely).
    pub weight: u32,
    /// Driver-defined exclusion bitmask. A leaf is ineligible when `tags & excluded_tags != 0`.
    pub tags: u64,
    #[allow(clippy::type_complexity)]
    pub(crate) guard: Arc<dyn Fn(&Ctx) -> bool + Send + Sync>,
    #[allow(clippy::type_complexity)]
    pub(crate) build: Arc<dyn Fn(&mut StdRng, &Ctx) -> Op + Send + Sync>,
}

impl<Ctx, Op> Leaf<Ctx, Op> {
    pub fn new(
        weight: u32,
        tags: u64,
        guard: impl Fn(&Ctx) -> bool + Send + Sync + 'static,
        build: impl Fn(&mut StdRng, &Ctx) -> Op + Send + Sync + 'static,
    ) -> Self {
        Self {
            weight,
            tags,
            guard: Arc::new(guard),
            build: Arc::new(build),
        }
    }
}

/// Runtime-built weighted selector over a [`Vec`] of [`Leaf`] entries.
///
/// Unlike the previous `Selector`, the tree is heap-allocated and built via
/// the [`Tree::leaf`] builder API. It is typically constructed once and stored
/// in a [`std::sync::LazyLock`].
pub struct Tree<Ctx, Op> {
    leaves: Vec<Leaf<Ctx, Op>>,
}

impl<Ctx, Op> Tree<Ctx, Op> {
    pub fn new() -> Self {
        Self { leaves: Vec::new() }
    }

    /// Append a leaf and return `self` for chaining.
    pub fn leaf(mut self, leaf: Leaf<Ctx, Op>) -> Self {
        self.leaves.push(leaf);
        self
    }

    /// Pick the next operation.
    ///
    /// `excluded_tags`: any leaf with `leaf.tags & excluded_tags != 0` is skipped
    /// regardless of its guard.  Pass `0` to exclude nothing.
    ///
    /// Panics if no leaf is eligible.
    pub fn pick(&self, rng: &mut StdRng, ctx: &Ctx, excluded_tags: u64) -> Op {
        let total: u32 = self
            .leaves
            .iter()
            .filter(|l| l.tags & excluded_tags == 0 && (l.guard)(ctx))
            .map(|l| l.weight)
            .sum();
        assert!(
            total > 0,
            "libdst selector: no eligible leaves (excluded_tags={excluded_tags:#x})"
        );
        let mut rem = rng.gen_range(0..total);
        for leaf in &self.leaves {
            if leaf.tags & excluded_tags != 0 || !(leaf.guard)(ctx) {
                continue;
            }
            if rem < leaf.weight {
                return (leaf.build)(rng, ctx);
            }
            rem -= leaf.weight;
        }
        unreachable!()
    }
}

impl<Ctx, Op> Default for Tree<Ctx, Op> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use rand::SeedableRng;

    struct Ctx {
        flag: bool,
    }

    #[derive(Debug, PartialEq)]
    enum Op {
        A,
        B,
    }

    fn make_tree() -> Tree<Ctx, Op> {
        Tree::new()
            .leaf(Leaf::new(1, 0, |c: &Ctx| c.flag, |_, _| Op::A))
            .leaf(Leaf::new(1, 0, |_| true, |_, _| Op::B))
    }

    #[test]
    fn pick_respects_guard() {
        let tree = make_tree();
        let ctx = Ctx { flag: false };
        let mut rng = StdRng::seed_from_u64(0);
        for _ in 0..20 {
            assert_eq!(tree.pick(&mut rng, &ctx, 0), Op::B);
        }
    }

    #[hegel::test]
    fn pbt_flagged_leaf_only_when_set(tc: TestCase) {
        use hegel::generators as gs;
        let flag = tc.draw(gs::booleans());
        let tree = make_tree();
        let ctx = Ctx { flag };
        let mut rng = StdRng::seed_from_u64(tc.draw(gs::integers::<u64>()));
        for _ in 0..30 {
            let op = tree.pick(&mut rng, &ctx, 0);
            if !flag {
                assert_eq!(op, Op::B);
            }
        }
    }

    #[test]
    fn excluded_tag_skips_leaf() {
        const TAG: u64 = 1;
        let tree: Tree<(), ()> = Tree::new().leaf(Leaf::new(1, TAG, |_| true, |_, _| ()));
        let mut rng = StdRng::seed_from_u64(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tree.pick(&mut rng, &(), TAG);
        }));
        assert!(result.is_err(), "should panic when all leaves are excluded");
    }

    #[test]
    fn two_pass_pick_no_alloc_path() {
        let tree = make_tree();
        let ctx = Ctx { flag: true };
        let mut rng = StdRng::seed_from_u64(42);
        let mut saw_a = false;
        let mut saw_b = false;
        for _ in 0..100 {
            match tree.pick(&mut rng, &ctx, 0) {
                Op::A => saw_a = true,
                Op::B => saw_b = true,
            }
        }
        assert!(saw_a && saw_b);
    }
}
