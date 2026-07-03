//! Storage service: pluggable persistence drivers for the kernel.
//!
//! Drivers ([`InMemory`], [`Sstable`], [`Wal`]) implement or feed the
//! [`Store`] trait; the db service composes them per database according to
//! its configured backend.

pub mod codec;
pub mod faults;
pub mod layers;
pub mod memory;
pub mod sstable;
#[allow(clippy::module_inception)]
pub mod store;
pub mod wal;

pub use codec::DEFAULT_ZSTD_LEVEL;
pub use faults::{FaultInjector, INJECTED_WAL_WRITE_ERROR};
pub use layers::{compact_layers, sweep_range};
pub use memory::InMemory;
pub use sstable::Sstable;
pub use store::{COMPACT_THRESHOLD, Store};
pub use wal::{Codec, Wal, WalEntry};
