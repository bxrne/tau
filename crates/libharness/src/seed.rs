//! Deterministic, hierarchical seed derivation.
//!
//! A whole simulation run is reproducible from a single root seed. Sub-streams
//! (per config cell, per reader thread, per fault schedule) derive their own
//! seeds from the root by hashing a stable string tag. Because each tag hashes
//! independently, adding a new sub-stream never perturbs the seeds of existing
//! ones - the property that makes "reproduce from seed N" stable across changes
//! to the harness.

use std::hash::{Hash, Hasher};

use rand::{SeedableRng, rngs::StdRng};

/// A reproducible tree of RNG seeds rooted at a single `u64`.
#[derive(Clone, Copy, Debug)]
pub struct SeedTree {
    root: u64,
}

impl SeedTree {
    /// Create a seed tree from a root seed (typically the run's `--seed`).
    pub fn new(root: u64) -> Self {
        Self { root }
    }

    /// The root seed, for printing on every run so failures can be reproduced.
    pub fn root(&self) -> u64 {
        self.root
    }

    /// Derive a child seed for a named sub-stream. Stable across harness
    /// changes: the same `(root, tag)` always yields the same value, and
    /// distinct tags are independent.
    pub fn seed_for(&self, tag: &str) -> u64 {
        // FNV-style mix of the root and the tag. Not cryptographic - it only
        // needs to spread inputs so unrelated tags do not collide or correlate.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.root.hash(&mut h);
        tag.hash(&mut h);
        h.finish()
    }

    /// Derive a fresh, seeded RNG for a named sub-stream.
    pub fn rng(&self, tag: &str) -> StdRng {
        StdRng::seed_from_u64(self.seed_for(tag))
    }

    /// Derive a child `SeedTree` whose root is this tree's `tag` sub-stream,
    /// for nesting (e.g. a per-cell tree that itself spawns per-reader streams).
    pub fn child(&self, tag: &str) -> SeedTree {
        SeedTree::new(self.seed_for(tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    #[hegel::test]
    fn same_tag_is_stable(tc: TestCase) {
        let root = tc.draw(gs::integers::<u64>());
        let tag = tc.draw(gs::text().max_size(32));
        let t = SeedTree::new(root);
        assert_eq!(t.seed_for(&tag), t.seed_for(&tag));
    }

    #[hegel::test]
    fn distinct_tags_differ(tc: TestCase) {
        let root = tc.draw(gs::integers::<u64>());
        let a = tc.draw(gs::text().max_size(16));
        let b = tc.draw(gs::text().max_size(16).filter({
            let a = a.clone();
            move |s| *s != a
        }));
        let t = SeedTree::new(root);
        assert_ne!(t.seed_for(&a), t.seed_for(&b));
    }

    #[hegel::test]
    fn distinct_roots_differ(tc: TestCase) {
        let r1 = tc.draw(gs::integers::<u64>());
        let r2 = tc.draw(gs::integers::<u64>().filter(move |&x| x != r1));
        let tag = tc.draw(gs::text().max_size(16));
        assert_ne!(
            SeedTree::new(r1).seed_for(&tag),
            SeedTree::new(r2).seed_for(&tag)
        );
    }

    #[hegel::test]
    fn child_nesting_is_deterministic(tc: TestCase) {
        let root = tc.draw(gs::integers::<u64>());
        let child_tag = tc.draw(gs::text().max_size(16));
        let leaf_tag = tc.draw(gs::text().max_size(16));
        let a = SeedTree::new(root).child(&child_tag).seed_for(&leaf_tag);
        let b = SeedTree::new(root).child(&child_tag).seed_for(&leaf_tag);
        assert_eq!(a, b);
    }

    #[hegel::test]
    fn rng_is_deterministic_from_same_seed(tc: TestCase) {
        use rand::RngCore;
        let root = tc.draw(gs::integers::<u64>());
        let tag = tc.draw(gs::text().max_size(16));
        let t = SeedTree::new(root);
        let mut r1 = t.rng(&tag);
        let mut r2 = t.rng(&tag);
        let a: u64 = r1.next_u64();
        let b: u64 = r2.next_u64();
        assert_eq!(a, b);
    }

    #[hegel::test]
    fn root_accessor_returns_root(tc: TestCase) {
        let root = tc.draw(gs::integers::<u64>());
        assert_eq!(SeedTree::new(root).root(), root);
    }

    #[hegel::test]
    fn child_differs_from_parent_seed(tc: TestCase) {
        let root = tc.draw(gs::integers::<u64>());
        let tag = tc.draw(gs::text().min_size(1).max_size(16));
        let parent = SeedTree::new(root).seed_for(&tag);
        let child = SeedTree::new(root).child(&tag).seed_for(&tag);
        assert_ne!(parent, child);
    }
}
