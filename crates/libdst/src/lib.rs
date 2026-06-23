//! # libdst — Deterministic Simulation Testing Framework
//!
//! Generic framework for comparing a **target** (system under test) with an isolated
//! **reference model** under a reproducible workload. Domain-specific ops, targets, and
//! models live in driver crates; libdst supplies:
//!
//! - [`btree`] — closure-based weighted behavior-tree selector (no unsafe, no fn-pointer limits)
//! - [`clock`] — virtual clock for fully deterministic time
//! - [`divergence`] — structured mismatch record
//! - [`faults`] — [`Fault`](faults::Fault) trait + [`truncate_file`](faults::truncate_file) primitive
//! - [`report`] — [`RunResult`] / [`SuiteResult`] with first-divergence tracking
//! - [`runner`] — [`run_sequential`] loop
//! - [`scheduler`] — deterministic cooperative task scheduler (replaces OS threads)
//! - [`shrink`] — delta-debug op-trace shrinker
//! - [`sim`] — [`DualSimulation`] trait
//!
//! ## Quick start
//!
//! 1. Implement [`DualSimulation`] for your op type.
//! 2. Call [`run_sequential`] with a seeded [`rand::rngs::StdRng`].
//!
//! ## Workload selection
//!
//! Build a [`btree::Tree`] with [`btree::Leaf`] entries using closure guards and builders.
//! Pass `excluded_tags` to [`btree::Tree::pick`] to suppress tag-filtered leaves at runtime.
//!
//! ## Concurrency
//!
//! Use [`scheduler::Scheduler`] for a deterministic single-threaded cooperative concurrency
//! model. Every interleaving is reproducible from the seed — no OS thread scheduling.

pub mod btree;
pub mod clock;
pub mod divergence;
pub mod faults;
pub mod report;
pub mod runner;
pub mod scheduler;
pub mod shrink;
pub mod sim;

pub use btree::{Leaf, Tree};
pub use clock::Clock;
pub use divergence::Divergence;
pub use faults::{CorruptFile, Fault, TruncateFile, corrupt_file, truncate_file};
pub use report::{RunResult, SuiteResult};
pub use runner::{SequentialOpts, run_sequential};
pub use scheduler::{Scheduler, Task};
pub use shrink::{shrink, shrink_with_granularity};
pub use sim::{CheckpointAction, DualSimulation};
