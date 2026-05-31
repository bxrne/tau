//! Shared simulation + benchmark harness for tau.
//!
//! Holds the oracle, data generators, seed-derivation, and the Backend
//! abstraction used by both the DST (`crates/dst`) and the Criterion
//! micro-benchmark suite (`benches/`).

pub mod datagen;
pub mod oracle;
pub mod seed;

pub use datagen::{OneBrcGen, Reading, Tier};
pub use oracle::Oracle;
pub use seed::SeedTree;
