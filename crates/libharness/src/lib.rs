//! Shared simulation + benchmark harness for tau.
//!
//! This crate holds everything that the deterministic simulation tester
//! (`dst`), the 1BRC load test, and the Criterion micro-benchmark suite need
//! in common: backend abstractions over the engine, the reference oracle,
//! deterministic data generators, and result reporting. Keeping it out of
//! `libtau` keeps the engine's public surface and dependency set clean.
//!
//! The modules are filled in across the DST/benchmark phases; for now the
//! crate provides the deterministic seed-derivation primitive that every
//! other piece builds on.

pub mod seed;

pub use seed::SeedTree;
