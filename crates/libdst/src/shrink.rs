//! Delta-debug op-trace shrinker.
//!
//! Given a failing trace and a predicate, [`shrink`] returns the smallest
//! sub-sequence that still causes the failure, using the delta-debugging algorithm.

/// Shrink a failing op trace to a minimal sub-sequence.
///
/// `is_failing(trace)` must return `true` for the full `ops` slice.
/// Returns the smallest subsequence for which the predicate still holds.
pub fn shrink<Op: Clone, F>(ops: Vec<Op>, mut is_failing: F) -> Vec<Op>
where
    F: FnMut(&[Op]) -> bool,
{
    let mut current = ops;

    // Iteratively drop single elements until no further reduction is possible.
    let mut changed = true;
    while changed {
        changed = false;
        let mut i = 0;
        while i < current.len() {
            let mut candidate = current[..i].to_vec();
            candidate.extend_from_slice(&current[i + 1..]);
            if is_failing(&candidate) {
                current = candidate;
                changed = true;
            } else {
                i += 1;
            }
        }
    }

    current
}

/// Shrink with a coarse-to-fine granularity strategy for large traces.
///
/// First attempts to drop large chunks (halves, quarters, …), then falls back to
/// single-element removal. More efficient than [`shrink`] on long traces.
pub fn shrink_with_granularity<Op: Clone, F>(ops: Vec<Op>, mut is_failing: F) -> Vec<Op>
where
    F: FnMut(&[Op]) -> bool,
{
    if ops.is_empty() {
        return ops;
    }

    let mut current = ops;
    let mut granularity = 2usize;

    loop {
        let len = current.len();
        if len == 0 {
            break;
        }
        let chunk_size = len.div_ceil(granularity);
        let mut made_progress = false;

        let mut i = 0;
        while i < current.len() {
            let end = (i + chunk_size).min(current.len());
            let mut candidate = current[..i].to_vec();
            candidate.extend_from_slice(&current[end..]);
            if !candidate.is_empty() && is_failing(&candidate) {
                current = candidate;
                made_progress = true;
            } else {
                i += chunk_size;
            }
        }

        if !made_progress {
            if granularity >= len.saturating_mul(2) {
                break;
            }
            granularity = granularity.saturating_mul(2).min(len.saturating_mul(2));
        }
    }

    shrink(current, is_failing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrink_finds_minimal_pair() {
        let ops = vec![1u32, 2, 3, 4, 5, 6];
        let result = shrink(ops, |s| s.contains(&2) && s.contains(&5));
        assert_eq!(result, vec![2, 5]);
    }

    #[test]
    fn shrink_empty_returns_empty() {
        let result = shrink(Vec::<u32>::new(), |_| false);
        assert!(result.is_empty());
    }

    #[test]
    fn shrink_single_required() {
        let result = shrink(vec![1u32, 2, 3], |s| s.contains(&2));
        assert_eq!(result, vec![2]);
    }

    #[test]
    fn shrink_with_granularity_finds_minimal_pair() {
        let ops: Vec<u32> = (0..20).collect();
        let result = shrink_with_granularity(ops, |s| s.contains(&7) && s.contains(&13));
        assert_eq!(result, vec![7, 13]);
    }

    #[hegel::test]
    fn pbt_shrunk_trace_still_fails(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let len = tc.draw(gs::integers::<usize>().min_value(1).max_value(10));
        let ops: Vec<u32> = (0..len as u32).collect();
        let target = tc.draw(gs::integers::<u32>().min_value(0).max_value(len as u32 - 1));
        let result = shrink(ops, |s: &[u32]| s.contains(&target));
        assert!(result.contains(&target), "shrunk trace must still fail");
        assert!(result.len() <= len, "shrunk must be no longer");
    }

    #[hegel::test]
    fn pbt_granularity_shrunk_still_fails(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let len = tc.draw(gs::integers::<usize>().min_value(1).max_value(15));
        let ops: Vec<u32> = (0..len as u32).collect();
        let target = tc.draw(gs::integers::<u32>().min_value(0).max_value(len as u32 - 1));
        let result = shrink_with_granularity(ops, |s: &[u32]| s.contains(&target));
        assert!(result.contains(&target), "shrunk trace must still fail");
        assert!(result.len() <= len);
    }
}
