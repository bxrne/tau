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

    #[test]
    fn same_tag_is_stable() {
        let t = SeedTree::new(0xdead_beef);
        assert_eq!(t.seed_for("reader-0"), t.seed_for("reader-0"));
    }

    #[test]
    fn distinct_tags_differ() {
        let t = SeedTree::new(0xdead_beef);
        assert_ne!(t.seed_for("reader-0"), t.seed_for("reader-1"));
    }

    #[test]
    fn distinct_roots_differ() {
        assert_ne!(
            SeedTree::new(1).seed_for("cell"),
            SeedTree::new(2).seed_for("cell"),
        );
    }

    #[test]
    fn child_nesting_is_deterministic() {
        let a = SeedTree::new(7).child("cell-3").seed_for("reader-2");
        let b = SeedTree::new(7).child("cell-3").seed_for("reader-2");
        assert_eq!(a, b);
    }
}
