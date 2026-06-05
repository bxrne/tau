//! Deterministic cooperative task scheduler for concurrent DST phases.
//!
//! All "concurrency" is simulated on a single thread. The seeded RNG decides which
//! task runs next, making every interleaving fully reproducible from a seed.

use rand::Rng;
use rand::rngs::StdRng;

/// One logical task tracked by the scheduler.
#[derive(Debug, Clone)]
pub struct Task {
    /// Caller-assigned task ID (opaque to the scheduler).
    pub id: usize,
    /// Remaining steps for this task; task is done when this reaches zero.
    pub ops_remaining: usize,
}

impl Task {
    pub fn new(id: usize, ops: usize) -> Self {
        Self {
            id,
            ops_remaining: ops,
        }
    }
}

/// Deterministic single-threaded cooperative task scheduler.
///
/// On each call to [`Scheduler::next`] the scheduler uniformly picks one runnable
/// task (weighted 1 each, not by remaining ops) and decrements its counter.
pub struct Scheduler {
    tasks: Vec<Task>,
}

impl Scheduler {
    pub fn new(tasks: Vec<Task>) -> Self {
        Self { tasks }
    }

    /// Pick the next task to run. Returns `None` when all tasks are done.
    pub fn next(&mut self, rng: &mut StdRng) -> Option<usize> {
        let active: Vec<usize> = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.ops_remaining > 0)
            .map(|(i, _)| i)
            .collect();
        if active.is_empty() {
            return None;
        }
        let chosen = active[rng.gen_range(0..active.len())];
        self.tasks[chosen].ops_remaining -= 1;
        Some(self.tasks[chosen].id)
    }

    pub fn is_done(&self) -> bool {
        self.tasks.iter().all(|t| t.ops_remaining == 0)
    }

    pub fn total_ops_remaining(&self) -> usize {
        self.tasks.iter().map(|t| t.ops_remaining).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn all_tasks_run_to_completion() {
        let mut sched = Scheduler::new(vec![Task::new(0, 5), Task::new(1, 3)]);
        let mut rng = StdRng::seed_from_u64(42);
        let mut counts = [0usize; 2];
        while let Some(id) = sched.next(&mut rng) {
            counts[id] += 1;
        }
        assert_eq!(counts[0], 5);
        assert_eq!(counts[1], 3);
        assert!(sched.is_done());
    }

    #[test]
    fn empty_scheduler_returns_none_immediately() {
        let mut sched = Scheduler::new(vec![]);
        let mut rng = StdRng::seed_from_u64(1);
        assert!(sched.next(&mut rng).is_none());
    }

    #[test]
    fn deterministic_from_same_seed() {
        let make = || Scheduler::new(vec![Task::new(0, 4), Task::new(1, 4)]);
        let run = |seed: u64| {
            let mut sched = make();
            let mut rng = StdRng::seed_from_u64(seed);
            let mut seq = Vec::new();
            while let Some(id) = sched.next(&mut rng) {
                seq.push(id);
            }
            seq
        };
        assert_eq!(run(1), run(1));
        assert_eq!(run(99), run(99));
        // different seeds produce different orderings (with very high probability)
        // we just check both seeds are deterministic
    }

    #[hegel::test]
    fn pbt_each_task_runs_exactly_its_ops(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let n0 = tc.draw(gs::integers::<usize>().min_value(0).max_value(10));
        let n1 = tc.draw(gs::integers::<usize>().min_value(0).max_value(10));
        let seed = tc.draw(gs::integers::<u64>());
        let mut sched = Scheduler::new(vec![Task::new(0, n0), Task::new(1, n1)]);
        let mut rng = StdRng::seed_from_u64(seed);
        let mut counts = [0usize; 2];
        while let Some(id) = sched.next(&mut rng) {
            counts[id] += 1;
        }
        assert_eq!(counts[0], n0);
        assert_eq!(counts[1], n1);
    }
}
