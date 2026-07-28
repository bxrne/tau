//! libtau: a temporal database built as a syscall microkernel.
//!
//! The [`Kernel`] owns four built-in services and routes every statement
//! between them:
//! - [`services::db`] — mutations (DDL, appends, transactions, backup/restore)
//! - [`services::query`] — read-only evaluation (`AT`, `RANGE`, `REDUCE`, `SHOW …`)
//! - [`services::auth`] — users, grants, permission primitives
//! - [`services::metrics`] — counters and histograms
//!
//! [`services::store`] provides the pluggable persistence drivers
//! ([`InMemory`], [`Sstable`], [`Wal`]) the db service composes per database.
//! The query language ([`ql`]) is a plain library: parsing stays outside the
//! kernel.

pub mod clock;
pub mod crypto;
pub(crate) mod func;
pub mod kernel;
pub(crate) mod model;
pub(crate) mod ql;
pub mod services;
pub(crate) mod value;
pub(crate) mod wire;

pub use clock::Clock;
pub use kernel::*;
pub use model::{Layer, LayerId, Tau, Timestamp};
pub use ql::{AggFunc, Stmt, format_parse_error, needs_registry_lock, parse, parse_literal};
pub use services::auth::{AuthService, Perm, User, UserStore};
pub use services::db::{Database, DbService, ExecError, LayerInfo, Output, StorageBackend};
pub use services::metrics::{Metrics, MetricsService, Op};
pub use services::query::QueryService;
pub use services::query::eval::{at_layers, collect_bounds_from_layers};
pub use services::store::{
    COMPACT_THRESHOLD, Codec, DEFAULT_ZSTD_LEVEL, FaultInjector, InMemory, Sstable, Wal,
};
pub use value::Value;
pub use wire::{Response, WireError};

/// Async facade over a hosted kernel: syscalls are serialized through the
/// tokio host loop.  For synchronous embedding (the TCP server ) use
/// [`Kernel`] directly.
pub struct Libtau {
    host: HostHandle,
}

impl Libtau {
    pub fn new() -> Self {
        let host = start(Kernel::new());
        Self { host }
    }

    pub async fn exec(&mut self, stmt: &Stmt) -> Result<Output, ExecError> {
        let stmt = stmt.clone();
        self.host
            .syscall(move |ctx| Ok(ctx.exec(&stmt)))
            .await
            .map_err(|e| ExecError::Io(e.to_string()))?
    }
}

impl Default for Libtau {
    fn default() -> Self {
        Self::new()
    }
}
