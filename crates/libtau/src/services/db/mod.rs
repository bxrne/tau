//! Database service: the mutation half of statement execution.
//!
//! Owns the shared [`Registry`] of named databases and executes every
//! *mutating* statement: database DDL (`CREATE`/`DROP`/`USE DATABASE`),
//! lens DDL and writes (`CREATE`/`DERIVE`/`XDERIVE`/`DROP LENS`, `APPEND`,
//! `COPY`, TTL), transactions, and `BACKUP`/`RESTORE`.
//!
//! Read-only statements are the [`crate::services::query`] service's job;
//! user management belongs to [`crate::services::auth`].  The kernel routes
//! between the three — nothing calls this service directly.
//!
//! # Model
//!
//! * The registry maps *named databases* to per-database state.  `CREATE
//!   DATABASE` adds one, `DROP DATABASE` removes one, `USE DATABASE` selects
//!   the active database for subsequent lens statements.
//! * Each database has a single value type - all of its *base* lenses must
//!   share the type declared at `CREATE LENS`.  An `APPEND` whose literal
//!   does not match the declared type is rejected with [`ExecError::TypeMismatch`].
//! * *Derived* lenses are pure expressions over other lenses.  A cycle in the
//!   derivation graph is rejected at `DERIVE` time with [`ExecError::CycleDetected`].

pub(crate) mod database;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use crate::clock::Clock;
use crate::kernel::{Service, SyscallCtx, SyscallError};
use crate::model::{Layer, LayerId, Tau, Timestamp};
use crate::ql::ast::{Expr, Stmt, Type};
use crate::services::auth::Perm;
use crate::services::metrics::Metrics;
use crate::services::query::eval::{materialise_expr, would_cycle};
use crate::services::store::{
    FaultInjector, InMemory, Sstable, Store,
    wal::{Wal, WalEntry},
};
use crate::value::Value;

pub use database::Database;

/// Metadata about a single layer returned by `HISTORY LENS`.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerInfo {
    /// Monotonic layer identifier assigned at write time.
    pub id: LayerId,
    /// Wall-clock write time (milliseconds since Unix epoch).
    pub written_at: i64,
    /// Earliest tau start in this layer.
    pub min_start: Timestamp,
    /// Latest tau end in this layer.
    pub max_end: Timestamp,
    /// Number of taus in this layer.
    pub tau_count: usize,
}

/// Output of a single executed statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Statement produced no value (DDL / writes).
    Empty,
    /// Point lookup result.  `None` means the lens has no value at that time.
    Value(Option<Value>),
    /// Sequence of `(start, end, value)` segments covering the queried range.
    Range(Vec<(Timestamp, Timestamp, Value)>),
    /// List of names returned by `SHOW DATABASES` / `SHOW LENSES` / `SHOW USERS`.
    Names(Vec<String>),
    /// `SHOW GRANTS` result: lines of `<user> <db>:<perms> <db>:<perms> …`.
    Grants(Vec<(String, Vec<(String, Perm)>)>),
    /// `HISTORY LENS` result: metadata for each layer covering the queried range.
    LayerHistory(Vec<LayerInfo>),
    /// `SHOW STATUS` result: flat list of `(key, value)` server-state pairs.
    Status(Vec<(String, String)>),
}

/// All errors statement execution can produce.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecError {
    /// No `USE DATABASE` (or initial `CREATE DATABASE`) has set an active DB.
    NoActiveDatabase,
    /// `USE DATABASE` / `DROP DATABASE` targeted a database that doesn't exist.
    UnknownDatabase(String),
    /// `CREATE DATABASE` for a name already present.
    DuplicateDatabase(String),
    /// Reference to a lens that is neither base nor derived in the active DB.
    UnknownLens(String),
    /// `CREATE LENS` / `DERIVE LENS` for a name already in use.
    DuplicateLens(String),
    /// Append value's runtime type does not match the lens's declared type.
    TypeMismatch {
        lens: String,
        expected: Type,
        got: String,
    },
    /// Expression evaluation produced an illegal combination of types.
    InvalidExpr(String),
    /// `APPEND` / `RANGE` with `start >= end`.
    InvalidRange,
    /// A storage I/O error (WAL write failure, disk error, etc.).
    Io(String),
    /// `DERIVE LENS` would introduce a reference cycle.
    CycleDetected(String),
    /// The user attempted an operation they're not authorised to perform.
    PermissionDenied(String),
    /// `CREATE USER` for a name already present.
    DuplicateUser(String),
    /// `DROP USER` / `GRANT` / `REVOKE` targeted a non-existent user.
    UnknownUser(String),
    /// `START TRANSACTION` issued while a transaction is already active.
    TransactionAlreadyActive,
    /// `COMMIT` or `ROLLBACK` issued without a preceding `START TRANSACTION`.
    NoActiveTransaction,
    /// A direct write (`APPEND` / `COPY`) targeted a materialised (`XDERIVE`)
    /// lens.  These are maintained by the engine; write the source lenses.
    MaterialisedLens(String),
}

impl From<io::Error> for ExecError {
    fn from(e: io::Error) -> Self {
        ExecError::Io(e.to_string())
    }
}

/// Where newly-created databases persist their layer data.
#[derive(Clone)]
pub enum StorageBackend {
    /// In-memory only. WAL (if configured) provides crash recovery; data is
    /// lost on clean shutdown without a WAL.
    Memory,
    /// Every database gets its own `<dir>/<name>.manifest` + `<dir>/<name>.run.<id>`
    /// SSTable files (see [`crate::services::store::Sstable`]): an in-memory
    /// memtable absorbs appends, and checkpoints flush it into a new immutable
    /// run instead of rewriting the whole database, with newest-wins/`AS OF`
    /// resolved at read time (MVCC) across the memtable and runs.
    /// `CREATE DATABASE <name>` re-opens an existing manifest and replays its
    /// schema, so lenses survive a restart.
    Disk {
        dir: std::path::PathBuf,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
        /// Applied to each database's per-file WAL (`<dir>/<name>.wal`).
        wal_fsync_each: bool,
        wal_max_bytes: Option<u64>,
    },
}

/// Definition of a materialised (`XDERIVE`) lens: the expression to compute,
/// an optional domain bound, and the set of lens names whose writes should
/// trigger a re-materialisation.  `deps` is resolved transitively through lazy
/// derived lenses so a write to a leaf base lens still refreshes the view.
#[derive(Clone)]
pub(crate) struct XderiveDef {
    pub(crate) expr: Expr,
    pub(crate) range: Option<(Timestamp, Timestamp)>,
    pub(crate) deps: Vec<String>,
}

/// Per-database state: the store plus the lens schema.
pub(crate) struct DbState {
    pub(crate) db: Database<Value>,
    /// Declared type of every base lens in this database.  Absence of an
    /// entry means the lens is either derived or unknown.  Materialised
    /// (`XDERIVE`) lenses also get an entry here so they share the base-lens
    /// query, range, and history paths.
    pub(crate) base_types: HashMap<String, Type>,
    /// Monotonic layer-id source so `APPEND` doesn't need the caller to
    /// supply one.
    pub(crate) next_layer_id: u64,
    /// Derived lens definitions, stored by name.  Lookups recursively
    /// re-evaluate these - no caching.
    pub(crate) derived: HashMap<String, Expr>,
    /// Optional `OVER` domain bound for a derived lens.  Outside the bound the
    /// lens reads as NIL.  Only lazy derived lenses use this; materialised
    /// lenses bake the bound into their stored layers.
    pub(crate) derived_ranges: HashMap<String, (Timestamp, Timestamp)>,
    /// Materialised lens definitions, keyed by name.  These names are also
    /// present in `base_types`; this map only drives re-materialisation.
    pub(crate) xderived: HashMap<String, XderiveDef>,
    /// Per-lens TTL in seconds.  A lens with an entry here hides data whose
    /// temporal interval ends before `(now - ttl_secs)` seconds ago.
    pub(crate) ttl_secs: HashMap<String, i64>,
    /// Axis names for multi-dimensional lenses (axis 0 is valid time).  A lens
    /// absent from this map has the default single valid-time axis.
    pub(crate) axes: HashMap<String, Vec<String>>,
    /// The kernel's virtual clock: transaction stamps and TTL cutoffs read it.
    pub(crate) clock: Arc<Clock>,
}

impl DbState {
    /// Wrap an opened database, restoring `next_layer_id` past any replayed
    /// layers so new layers never collide with persisted ones.
    fn from_db(db: Database<Value>, clock: Arc<Clock>) -> Self {
        let next_layer_id = db.max_layer_id() + 1;
        Self {
            db,
            base_types: HashMap::default(),
            next_layer_id,
            derived: HashMap::default(),
            derived_ranges: HashMap::default(),
            xderived: HashMap::default(),
            ttl_secs: HashMap::default(),
            axes: HashMap::default(),
            clock,
        }
    }

    fn new(compact_threshold: usize, clock: Arc<Clock>) -> Self {
        Self::from_db(
            Database::new(InMemory::<Value>::with_threshold(compact_threshold)),
            clock,
        )
    }

    /// Open (or create) a disk-backed database — an [`Sstable`] store paired
    /// with a per-database WAL — returning the fresh state plus any schema
    /// DDL persisted so the caller can replay it.
    fn with_disk(
        base: impl AsRef<Path>,
        compact_threshold: usize,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
        wal_fsync_each: bool,
        wal_max_bytes: Option<u64>,
        clock: Arc<Clock>,
    ) -> io::Result<(Self, Vec<String>)> {
        let base = base.as_ref();
        let mut store = Sstable::open(base, enc_key)?;
        store.set_compact_threshold(compact_threshold);
        store.set_compression_level(compression_level);

        let wal_path = base.with_extension("wal");
        let mut wal = Wal::open(&wal_path, enc_key)?;
        wal.replay(&mut store)?;

        if wal.replay_schemas()?.is_empty() {
            for stmt in store.schema_stmts() {
                wal.append_schema(&stmt)?;
            }
        }

        wal.set_fsync_each(wal_fsync_each);
        if let Some(bytes) = wal_max_bytes {
            wal.set_max_bytes(bytes);
        }

        let db = Database::with_wal(store, wal);
        let schema_stmts = db.schema_stmts()?;
        Ok((Self::from_db(db, clock), schema_stmts))
    }

    fn with_wal(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        key: Option<[u8; 32]>,
        clock: Arc<Clock>,
    ) -> io::Result<(Self, Vec<String>)> {
        let store = InMemory::<Value>::with_threshold(compact_threshold);
        let db = Database::open(store, path, key)?;
        // Schema stmts are replayed by the caller after construction.
        let schema_stmts = db.schema_stmts()?;
        Ok((Self::from_db(db, clock), schema_stmts))
    }
}

/// Shared registry of named databases plus the active-database cursor.
/// Handed to both the db service (mutations) and the query service (reads).
pub(crate) struct Registry {
    pub(crate) databases: HashMap<String, Arc<RwLock<DbState>>>,
    /// Name of the currently active database (set by the first
    /// `CREATE DATABASE` and by `USE DATABASE`).  Cleared if the active
    /// database is dropped.
    pub(crate) active: Option<String>,
}

impl Registry {
    fn new() -> Self {
        Self {
            databases: HashMap::default(),
            active: None,
        }
    }

    /// The `Arc<RwLock<DbState>>` for the active database.
    pub(crate) fn active_db_arc(&self) -> Result<Arc<RwLock<DbState>>, ExecError> {
        let name = self.active.as_deref().ok_or(ExecError::NoActiveDatabase)?;
        self.databases
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownDatabase(name.into()))
    }
}

/// Convert parsed literal taus to runtime values (Arc-bump for strings).
fn literal_taus(
    taus: &[(i64, i64, crate::ql::ast::Literal)],
) -> Vec<(Timestamp, Timestamp, Value)> {
    taus.iter().map(|(s, e, l)| (*s, *e, l.into())).collect()
}

/// The lens must exist as either a base or a derived lens.
pub(crate) fn ensure_lens_exists(state: &DbState, name: &str) -> Result<(), ExecError> {
    if state.base_types.contains_key(name) || state.derived.contains_key(name) {
        Ok(())
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

/// The lens must be a base lens; derived lenses get a targeted error.
pub(crate) fn ensure_base_lens(
    state: &DbState,
    name: &str,
    stmt_kind: &str,
) -> Result<(), ExecError> {
    if state.base_types.contains_key(name) {
        Ok(())
    } else if state.derived.contains_key(name) {
        Err(ExecError::InvalidExpr(format!(
            "{stmt_kind} is only supported for base lenses"
        )))
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

/// Single-axis statements (`AT t`, `RANGE`, `REDUCE`, TTL, derivation) cannot
/// address a multi-axis lens — its taus need one coordinate per axis.
pub(crate) fn ensure_single_axis(
    state: &DbState,
    name: &str,
    stmt_kind: &str,
) -> Result<(), ExecError> {
    match state.axes.get(name) {
        Some(axes) => Err(ExecError::InvalidExpr(format!(
            "{stmt_kind}: lens '{name}' has {} axes; supply one coordinate per axis",
            axes.len()
        ))),
        None => Ok(()),
    }
}

/// Arity-mismatch error shared by the N-D read/write paths.
pub(crate) fn arity_error(name: &str, declared: usize, supplied: usize) -> ExecError {
    ExecError::InvalidExpr(format!(
        "lens '{name}' has {declared} axes but the statement supplies {supplied}"
    ))
}

/// One buffered transaction entry: the statement plus the database that was
/// active when it was buffered.
type PendingStmt = (Option<String>, Stmt);

/// The mutation service.  All methods take `&self`: registry-level mutations
/// serialize on the registry's write lock, lens-level mutations on the
/// per-database write lock, exactly mirroring the old server-side routing.
pub struct DbService {
    registry: Arc<RwLock<Registry>>,
    /// Buffered mutations for the active transaction.  `None` means no
    /// transaction is open.  Each entry pairs the active database name (at the
    /// time the statement was buffered) with the statement itself so COMMIT can
    /// replay each mutation against the correct database regardless of which
    /// database is active when COMMIT runs.
    pending: Mutex<Option<Vec<PendingStmt>>>,
    compact_threshold: usize,
    /// Storage backend used when `CREATE DATABASE` allocates a new store.
    backend: StorageBackend,
    metrics: Arc<Metrics>,
    /// The kernel's virtual clock, shared into every database's state.
    clock: Arc<Clock>,
    /// Kernel-owned fault injector, attached to every database's WAL.
    faults: Arc<FaultInjector>,
}

impl Service for DbService {
    fn boot(&mut self, _ctx: &mut SyscallCtx<'_>) -> Result<(), SyscallError> {
        Ok(())
    }
}

impl DbService {
    pub(crate) fn new(
        backend: StorageBackend,
        compact_threshold: usize,
        metrics: Arc<Metrics>,
        clock: Arc<Clock>,
        faults: Arc<FaultInjector>,
    ) -> Self {
        Self {
            registry: Arc::new(RwLock::new(Registry::new())),
            pending: Mutex::new(None),
            compact_threshold,
            backend,
            metrics,
            clock,
            faults,
        }
    }

    /// Open a WAL-backed `default` database and replay its schema.  Used by
    /// the WAL (memory + write-ahead-log) kernel backend.
    pub(crate) fn open_wal_default(
        &self,
        path: impl AsRef<Path>,
        key: Option<[u8; 32]>,
    ) -> io::Result<()> {
        let (mut db_state, schema_stmts) =
            DbState::with_wal(path, self.compact_threshold, key, self.clock.clone())?;
        db_state.db.set_metrics(self.metrics.clone());
        db_state.db.set_wal_fault_injector(self.faults.clone());
        let db_arc = Arc::new(RwLock::new(db_state));
        {
            let mut reg = self.registry.write().expect("registry lock poisoned");
            reg.databases.insert("default".to_string(), db_arc.clone());
            reg.active = Some("default".to_string());
        }
        self.replay_schema_stmts(&db_arc, &schema_stmts);
        Ok(())
    }

    /// Shared handle to the registry, for the query service.
    pub(crate) fn registry(&self) -> Arc<RwLock<Registry>> {
        self.registry.clone()
    }

    /// Name of the active database, if any.
    pub fn active(&self) -> Option<String> {
        self.registry
            .read()
            .expect("registry lock poisoned")
            .active
            .clone()
    }

    /// Whether a transaction is currently buffering mutations.
    pub fn is_in_transaction(&self) -> bool {
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .is_some()
    }

    /// Disable per-record WAL fsync across all databases.  Caller is
    /// responsible for periodic `flush_wal()` calls to enforce durability
    /// boundaries.  Intended for bulk-load paths.
    pub fn set_wal_fsync_each(&self, on: bool) {
        let reg = self.registry.read().expect("registry lock poisoned");
        for arc in reg.databases.values() {
            arc.write()
                .expect("db lock poisoned")
                .db
                .set_wal_fsync_each(on);
        }
    }

    /// Set a soft WAL file-size cap (bytes) across all databases.  Once a WAL
    /// file reaches `bytes`, the next `APPEND` triggers a checkpoint rewrite
    /// that compacts it to only live layers.
    pub fn set_wal_max_bytes(&self, bytes: u64) {
        let reg = self.registry.read().expect("registry lock poisoned");
        for arc in reg.databases.values() {
            arc.write()
                .expect("db lock poisoned")
                .db
                .set_wal_max_bytes(bytes);
        }
    }

    /// Flush the WAL for all databases.  Used with group-commit mode
    /// (`set_wal_fsync_each(false)`) to enforce periodic durability.
    pub fn flush_wal(&self) -> io::Result<()> {
        let reg = self.registry.read().expect("registry lock poisoned");
        for arc in reg.databases.values() {
            arc.read().expect("db lock poisoned").db.wal_flush()?;
        }
        Ok(())
    }

    /// Execute a mutating statement.  Read-only statements belong to the
    /// query service; user statements to the auth service — both are routed
    /// by the kernel and rejected here.
    pub fn exec(&self, stmt: &Stmt) -> Result<Output, ExecError> {
        // While inside a transaction, buffer mutable lens statements.  Each
        // entry captures the active database at buffer time so COMMIT can
        // replay the statement against the right database.
        {
            let mut pending = self.pending.lock().expect("pending lock poisoned");
            if let Some(buffer) = pending.as_mut()
                && is_transactable(stmt)
            {
                let active = self
                    .registry
                    .read()
                    .expect("registry lock poisoned")
                    .active
                    .clone();
                buffer.push((active, stmt.clone()));
                return Ok(Output::Empty);
            }
        }
        match stmt {
            Stmt::StartTransaction => self.start_transaction(),
            Stmt::Commit => self.commit(),
            Stmt::Rollback => self.rollback(),
            Stmt::CreateDatabase { name } => self.create_database(name),
            Stmt::DropDatabase { name } => self.drop_database(name),
            Stmt::UseDatabase { name } => self.use_database(name),
            Stmt::BackupDatabase { name, path } => self.backup_database(name, path),
            Stmt::RestoreDatabase { name, path } => self.restore_database(name, path),
            _ if is_transactable(stmt) => {
                let db_arc = self
                    .registry
                    .read()
                    .expect("registry lock poisoned")
                    .active_db_arc()?;
                self.exec_lens(&db_arc, stmt, false)
            }
            _ => Err(ExecError::InvalidExpr(
                "db service: not a mutating statement".into(),
            )),
        }
    }

    /// Execute a lens-scoped mutation against an explicit database.  `in_replay`
    /// suppresses schema-log writes when re-executing persisted DDL on startup.
    fn exec_lens(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        stmt: &Stmt,
        in_replay: bool,
    ) -> Result<Output, ExecError> {
        match stmt {
            Stmt::Create { name, ty, axes } => {
                self.create_lens(db_arc, name, ty.clone(), axes, in_replay)
            }
            Stmt::Append { name, taus } | Stmt::BatchAppend { name, taus } => {
                self.write_layer(db_arc, name, literal_taus(taus))
            }
            Stmt::AppendNd { name, taus } => self.append_nd_lens(db_arc, name, taus),
            Stmt::Copy { name, path } => self.copy_lens(db_arc, name, path),
            Stmt::Derive { name, expr, range } => {
                self.derive_lens(db_arc, name, expr.clone(), *range, in_replay)
            }
            Stmt::Xderive { name, expr, range } => {
                self.xderive_lens(db_arc, name, expr.clone(), *range, in_replay)
            }
            Stmt::Drop { name } => self.drop_lens(db_arc, name, in_replay),
            Stmt::SetTtl { name, secs } => self.update_ttl(db_arc, name, Some(*secs), in_replay),
            Stmt::UnsetTtl { name } => self.update_ttl(db_arc, name, None, in_replay),
            _ => Err(ExecError::InvalidExpr(
                "db service: not a lens statement".into(),
            )),
        }
    }

    fn start_transaction(&self) -> Result<Output, ExecError> {
        let mut pending = self.pending.lock().expect("pending lock poisoned");
        if pending.is_some() {
            return Err(ExecError::TransactionAlreadyActive);
        }
        *pending = Some(Vec::new());
        Ok(Output::Empty)
    }

    fn commit(&self) -> Result<Output, ExecError> {
        let entries = self
            .pending
            .lock()
            .expect("pending lock poisoned")
            .take()
            .ok_or(ExecError::NoActiveTransaction)?;
        // Each buffered statement replays against the database that was
        // active when it was buffered, resolved explicitly — the shared
        // active cursor is never touched.
        for (db_name, stmt) in entries {
            let db_arc = {
                let reg = self.registry.read().expect("registry lock poisoned");
                let name = db_name.as_deref().ok_or(ExecError::NoActiveDatabase)?;
                reg.databases
                    .get(name)
                    .cloned()
                    .ok_or_else(|| ExecError::UnknownDatabase(name.into()))?
            };
            self.exec_lens(&db_arc, &stmt, false)?;
        }
        Ok(Output::Empty)
    }

    fn rollback(&self) -> Result<Output, ExecError> {
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .take()
            .ok_or(ExecError::NoActiveTransaction)?;
        Ok(Output::Empty)
    }

    fn create_database(&self, name: &str) -> Result<Output, ExecError> {
        let (mut db_state, schema_stmts) = {
            let reg = self.registry.read().expect("registry lock poisoned");
            if reg.databases.contains_key(name) {
                return Err(ExecError::DuplicateDatabase(name.into()));
            }
            drop(reg);
            match &self.backend {
                StorageBackend::Memory => (
                    DbState::new(self.compact_threshold, self.clock.clone()),
                    Vec::new(),
                ),
                StorageBackend::Disk {
                    dir,
                    compression_level,
                    enc_key,
                    wal_fsync_each,
                    wal_max_bytes,
                } => {
                    let base = dir.join(name);
                    DbState::with_disk(
                        &base,
                        self.compact_threshold,
                        *compression_level,
                        *enc_key,
                        *wal_fsync_each,
                        *wal_max_bytes,
                        self.clock.clone(),
                    )?
                }
            }
        };
        db_state.db.set_metrics(self.metrics.clone());
        db_state.db.set_wal_fault_injector(self.faults.clone());
        let db_arc = Arc::new(RwLock::new(db_state));
        {
            let mut reg = self.registry.write().expect("registry lock poisoned");
            if reg.databases.contains_key(name) {
                return Err(ExecError::DuplicateDatabase(name.into()));
            }
            reg.databases.insert(name.into(), db_arc.clone());
            if reg.active.is_none() {
                reg.active = Some(name.into());
            }
        }
        // Rebuild lens schema (CREATE/DERIVE/SET TTL) for a re-opened disk file.
        self.replay_schema_stmts(&db_arc, &schema_stmts);
        Ok(Output::Empty)
    }

    /// Re-execute persisted schema DDL against `db_arc` with WAL/disk writes
    /// suppressed.  Shared by the WAL-open, disk-open, and RESTORE paths.
    fn replay_schema_stmts(&self, db_arc: &Arc<RwLock<DbState>>, stmts: &[String]) {
        for stmt_text in stmts {
            match crate::ql::parser::parse(stmt_text) {
                Ok((_, stmt)) => {
                    if let Err(e) = self.exec_lens(db_arc, &stmt, true) {
                        tracing::warn!(stmt = %stmt_text, error = ?e, "schema replay: statement failed, skipping");
                    }
                }
                Err(e) => {
                    tracing::warn!(stmt = %stmt_text, error = %e, "schema replay: parse error, skipping");
                }
            }
        }
    }

    fn drop_database(&self, name: &str) -> Result<Output, ExecError> {
        let mut reg = self.registry.write().expect("registry lock poisoned");
        if reg.databases.remove(name).is_none() {
            return Err(ExecError::UnknownDatabase(name.into()));
        }
        if reg.active.as_deref() == Some(name) {
            reg.active = None;
        }
        Ok(Output::Empty)
    }

    fn use_database(&self, name: &str) -> Result<Output, ExecError> {
        let mut reg = self.registry.write().expect("registry lock poisoned");
        if !reg.databases.contains_key(name) {
            return Err(ExecError::UnknownDatabase(name.into()));
        }
        reg.active = Some(name.into());
        Ok(Output::Empty)
    }

    fn create_lens(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        name: &str,
        ty: Type,
        axes: &[String],
        in_replay: bool,
    ) -> Result<Output, ExecError> {
        let mut state = db_arc.write().expect("db lock poisoned");
        if state.base_types.contains_key(name) || state.derived.contains_key(name) {
            return Err(ExecError::DuplicateLens(name.into()));
        }
        {
            let mut seen = HashSet::default();
            if axes.iter().any(|a| !seen.insert(a.as_str())) {
                return Err(ExecError::InvalidExpr("AXES names must be distinct".into()));
            }
        }
        // WAL-first: persist before updating in-memory state. The Stmt Display
        // includes the AXES clause, so arity survives schema replay.
        if !in_replay {
            let ddl = Stmt::Create {
                name: name.into(),
                ty: ty.clone(),
                axes: axes.to_vec(),
            };
            state.db.append_schema(&ddl.to_string())?;
        }
        state.base_types.insert(name.into(), ty);
        // A single named axis is the default valid-time axis; only true
        // multi-axis lenses need an entry.
        if axes.len() > 1 {
            state.axes.insert(name.into(), axes.to_vec());
        }
        Ok(Output::Empty)
    }

    /// Shared write path for `APPEND`, `BATCH APPEND`, and `COPY`: type-check
    /// every tau against the lens's declared type, sort, reject overlaps and
    /// empty intervals with [`ExecError::InvalidRange`], then append one layer.
    fn write_layer(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        name: &str,
        taus: Vec<(Timestamp, Timestamp, Value)>,
    ) -> Result<Output, ExecError> {
        if taus.is_empty() {
            return Ok(Output::Empty);
        }
        let mut state = db_arc.write().expect("db lock poisoned");
        // Materialised lenses are engine-maintained — reject direct writes
        // (their name is also in `base_types`, so this check must come first).
        if state.xderived.contains_key(name) {
            return Err(ExecError::MaterialisedLens(name.into()));
        }
        let ty = state
            .base_types
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownLens(name.into()))?;
        if let Some(axes) = state.axes.get(name) {
            return Err(arity_error(name, axes.len(), 1));
        }
        let mut tau_vec: Vec<Tau<Value>> = Vec::with_capacity(taus.len());
        for (start, end, value) in taus {
            if start >= end {
                return Err(ExecError::InvalidRange);
            }
            if let Some(got) = value.ty()
                && got != ty
            {
                return Err(ExecError::TypeMismatch {
                    lens: name.into(),
                    expected: ty.clone(),
                    got: value.type_name().into(),
                });
            }
            tau_vec.push(Tau::new(start, end, value));
        }
        tau_vec.sort_by_key(|t| t.start());
        if tau_vec.windows(2).any(|w| w[0].end() > w[1].start()) {
            return Err(ExecError::InvalidRange);
        }
        let id = state.next_layer_id;
        state.next_layer_id += 1;
        let layer = Layer::new_sorted_unchecked(id, tau_vec, state.clock.now_ms());
        state.db.append(name, layer)?;
        // Refresh any materialised views that read from this lens so a
        // correction here propagates as a new (newest-wins) layer.
        if !state.xderived.is_empty() {
            let mut visited = HashSet::default();
            rematerialise_dependents(&mut state, name, &mut visited)?;
        }
        Ok(Output::Empty)
    }

    /// N-dimensional write path: one orthotope per tau, arity checked against
    /// the lens's declared axes. A single-axis lens routes through
    /// [`DbService::write_layer`] so it shares the 1-D invariants and the
    /// materialised-view refresh.
    fn append_nd_lens(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        name: &str,
        taus: &[(Vec<(i64, i64)>, crate::ql::ast::Literal)],
    ) -> Result<Output, ExecError> {
        if taus.is_empty() {
            return Ok(Output::Empty);
        }
        let arity = {
            let state = db_arc.read().expect("db lock poisoned");
            state.axes.get(name).map_or(1, Vec::len)
        };
        if arity == 1 {
            let flat: Result<Vec<_>, ExecError> = taus
                .iter()
                .map(|(coords, lit)| match coords.as_slice() {
                    [(s, e)] => Ok((*s, *e, Value::from(lit))),
                    _ => Err(arity_error(name, 1, coords.len())),
                })
                .collect();
            return self.write_layer(db_arc, name, flat?);
        }
        let mut state = db_arc.write().expect("db lock poisoned");
        if state.xderived.contains_key(name) {
            return Err(ExecError::MaterialisedLens(name.into()));
        }
        let ty = state
            .base_types
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownLens(name.into()))?;
        let mut tau_vec: Vec<Tau<Value>> = Vec::with_capacity(taus.len());
        for (coords, lit) in taus {
            if coords.len() != arity {
                return Err(arity_error(name, arity, coords.len()));
            }
            let value = Value::from(lit);
            if let Some(got) = value.ty()
                && got != ty
            {
                return Err(ExecError::TypeMismatch {
                    lens: name.into(),
                    expected: ty.clone(),
                    got: value.type_name().into(),
                });
            }
            tau_vec.push(Tau::try_new_nd(coords, value).ok_or(ExecError::InvalidRange)?);
        }
        let id = state.next_layer_id;
        state.next_layer_id += 1;
        let layer = Layer::try_new_nd_at(id, tau_vec, state.clock.now_ms())
            .ok_or(ExecError::InvalidRange)?;
        state.db.append(name, layer)?;
        Ok(Output::Empty)
    }

    fn copy_lens(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        name: &str,
        path: &str,
    ) -> Result<Output, ExecError> {
        let content = fs::read_to_string(path)?;
        let mut taus: Vec<(Timestamp, Timestamp, Value)> = Vec::new();
        for (lineno, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = |what: &str| ExecError::Io(format!("line {}: {what}", lineno + 1));
            let mut parts = line.splitn(3, ',').map(str::trim);
            let (Some(s), Some(e), Some(v)) = (parts.next(), parts.next(), parts.next()) else {
                return Err(bad("expected start,end,value"));
            };
            let start: Timestamp = s
                .parse()
                .map_err(|_| bad(&format!("invalid start {s:?}")))?;
            let end: Timestamp = e.parse().map_err(|_| bad(&format!("invalid end {e:?}")))?;
            let value = crate::ql::parse_literal(v)
                .map(|lit| Value::from(&lit))
                .ok_or_else(|| bad(&format!("cannot parse value {v:?}")))?;
            taus.push((start, end, value));
        }
        self.write_layer(db_arc, name, taus)
    }

    fn derive_lens(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        name: &str,
        expr: Expr,
        range: Option<(Timestamp, Timestamp)>,
        in_replay: bool,
    ) -> Result<Output, ExecError> {
        let mut state = db_arc.write().expect("db lock poisoned");
        if state.base_types.contains_key(name) || state.derived.contains_key(name) {
            return Err(ExecError::DuplicateLens(name.into()));
        }
        // Reject self-referential or transitively cyclic expressions.
        let mut visited = HashSet::default();
        if would_cycle(&state.derived, name, &expr, &mut visited) {
            return Err(ExecError::CycleDetected(name.into()));
        }
        for dep in collect_deps(&state, &expr) {
            ensure_single_axis(&state, &dep, "DERIVE")?;
        }
        if !in_replay {
            let stmt = Stmt::Derive {
                name: name.into(),
                expr: expr.clone(),
                range,
            };
            state.db.append_schema(&stmt.to_string())?;
        }
        state.derived.insert(name.into(), expr);
        if let Some(r) = range {
            state.derived_ranges.insert(name.into(), r);
        }
        Ok(Output::Empty)
    }

    /// `XDERIVE LENS` - create a materialised lens: compute the expression now,
    /// store the result as concrete layers, and register the definition so any
    /// later write to a referenced lens re-materialises it (newest wins).
    fn xderive_lens(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        name: &str,
        expr: Expr,
        range: Option<(Timestamp, Timestamp)>,
        in_replay: bool,
    ) -> Result<Output, ExecError> {
        let mut state = db_arc.write().expect("db lock poisoned");

        if in_replay {
            // The materialised layers and the `base_types` entry are already
            // restored (data via WAL replay, type via the persisted CREATE
            // LENS). Only re-register the definition so future writes keep the
            // view current — do not re-materialise or re-persist.
            let deps = collect_deps(&state, &expr);
            state
                .xderived
                .insert(name.into(), XderiveDef { expr, range, deps });
            return Ok(Output::Empty);
        }

        if state.base_types.contains_key(name)
            || state.derived.contains_key(name)
            || state.xderived.contains_key(name)
        {
            return Err(ExecError::DuplicateLens(name.into()));
        }
        let mut visited = HashSet::default();
        if would_cycle(&state.derived, name, &expr, &mut visited) {
            return Err(ExecError::CycleDetected(name.into()));
        }
        for dep in collect_deps(&state, &expr) {
            ensure_single_axis(&state, &dep, "XDERIVE")?;
        }

        let taus = materialise_expr(&state, &expr, range)?;
        let ty = infer_type(&taus).unwrap_or(Type::Int);
        let deps = collect_deps(&state, &expr);

        // Persist as a base lens (type + routing) plus the XDERIVE definition
        // so a restart restores both the stored data and the auto-update rule.
        state
            .db
            .append_schema(&format!("CREATE LENS {name} {ty}"))?;
        let stmt = Stmt::Xderive {
            name: name.into(),
            expr: expr.clone(),
            range,
        };
        state.db.append_schema(&stmt.to_string())?;

        state.base_types.insert(name.into(), ty);
        state
            .xderived
            .insert(name.into(), XderiveDef { expr, range, deps });
        append_materialised_layer(&mut state, name, taus)?;
        Ok(Output::Empty)
    }

    /// `SET TTL` (`Some(secs)`) / `UNSET TTL` (`None`), persisted to the
    /// schema log unless replaying.
    fn update_ttl(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        name: &str,
        secs: Option<i64>,
        in_replay: bool,
    ) -> Result<Output, ExecError> {
        let mut state = db_arc.write().expect("db lock poisoned");
        ensure_lens_exists(&state, name)?;
        ensure_single_axis(&state, name, "TTL")?;
        let stmt_text = match secs {
            Some(s) => {
                state.ttl_secs.insert(name.into(), s);
                format!("SET TTL LENS {name} {s}")
            }
            None => {
                state.ttl_secs.remove(name);
                format!("UNSET TTL LENS {name}")
            }
        };
        if !in_replay {
            state.db.append_schema(&stmt_text)?;
        }
        Ok(Output::Empty)
    }

    fn drop_lens(
        &self,
        db_arc: &Arc<RwLock<DbState>>,
        name: &str,
        in_replay: bool,
    ) -> Result<Output, ExecError> {
        let mut state = db_arc.write().expect("db lock poisoned");
        let in_types = state.base_types.remove(name).is_some();
        let in_derived = state.derived.remove(name).is_some();
        state.derived_ranges.remove(name);
        state.axes.remove(name);
        let in_xderived = state.xderived.remove(name).is_some();
        if in_types || in_derived || in_xderived {
            state.db.drop_lens(name);
            if !in_replay {
                state.db.append_schema(&format!("DROP LENS {name}"))?;
            }
            Ok(Output::Empty)
        } else {
            Err(ExecError::UnknownLens(name.into()))
        }
    }

    fn backup_database(&self, name: &str, path: &str) -> Result<Output, ExecError> {
        let db_arc = {
            let reg = self.registry.read().expect("registry lock poisoned");
            reg.databases
                .get(name)
                .cloned()
                .ok_or_else(|| ExecError::UnknownDatabase(name.into()))?
        };
        let state = db_arc.read().expect("db lock poisoned");

        // Build schema DDL from in-memory state so backup works even when no
        // WAL is attached to the source database.
        let mut schema_stmts: Vec<String> = Vec::new();
        for (lens_name, lens_type) in &state.base_types {
            schema_stmts.push(format!("CREATE LENS {lens_name} {lens_type}"));
        }
        for (lens_name, expr) in &state.derived {
            let stmt = Stmt::Derive {
                name: lens_name.clone(),
                expr: expr.clone(),
                range: state.derived_ranges.get(lens_name).copied(),
            };
            schema_stmts.push(stmt.to_string());
        }
        // Materialised lenses are already emitted as `CREATE LENS` + data above;
        // emit the XDERIVE definition so the auto-update rule survives restore.
        for (lens_name, def) in &state.xderived {
            let stmt = Stmt::Xderive {
                name: lens_name.clone(),
                expr: def.expr.clone(),
                range: def.range,
            };
            schema_stmts.push(stmt.to_string());
        }

        let bk_path = Path::new(path);
        if bk_path.exists() {
            fs::remove_file(bk_path)?;
        }

        let mut wal = Wal::open(path, None)?;
        for stmt in &schema_stmts {
            wal.append_schema(stmt)?;
        }
        for (lens_name, layers) in &state.db.export_layers() {
            for layer in layers {
                wal.append(&WalEntry::from_layer(lens_name, layer))?;
            }
        }
        wal.sync()?;
        Ok(Output::Empty)
    }

    fn restore_database(&self, name: &str, path: &str) -> Result<Output, ExecError> {
        {
            let reg = self.registry.read().expect("registry lock poisoned");
            if reg.databases.contains_key(name) {
                return Err(ExecError::DuplicateDatabase(name.into()));
            }
        }
        if !Path::new(path).exists() {
            return Err(ExecError::Io(format!("backup file not found: {path}")));
        }

        let wal = Wal::open(path, None)?;
        let mut store = InMemory::<Value>::with_threshold(self.compact_threshold);
        wal.replay(&mut store)?;
        let schema_stmts = wal.replay_schemas()?;

        let db_arc = Arc::new(RwLock::new(DbState::from_db(
            Database::new(store),
            self.clock.clone(),
        )));
        {
            let mut reg = self.registry.write().expect("registry lock poisoned");
            if reg.databases.contains_key(name) {
                return Err(ExecError::DuplicateDatabase(name.into()));
            }
            reg.databases.insert(name.into(), db_arc.clone());
        }
        self.replay_schema_stmts(&db_arc, &schema_stmts);
        Ok(Output::Empty)
    }
}

/// Infer a declared type for a materialised lens from its computed taus: the
/// type of the first non-null value, or `None` when every value is null/empty.
fn infer_type(taus: &[(Timestamp, Timestamp, Value)]) -> Option<Type> {
    taus.iter().find_map(|(_, _, v)| v.ty())
}

/// Resolve the set of lens names whose writes should refresh a materialised
/// view defined by `expr`.  Lazy derived lenses are inlined (their own deps are
/// followed) so a leaf base-lens write still triggers the view; base and
/// materialised lenses are recorded as opaque leaves.
fn collect_deps(state: &DbState, expr: &Expr) -> Vec<String> {
    let mut out = Vec::new();
    collect_deps_into(state, expr, &mut out);
    out.sort();
    out.dedup();
    out
}

fn collect_deps_into(state: &DbState, expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Lit(_) => {}
        Expr::Ident(name) | Expr::Agg { lens: name, .. } => {
            if state.base_types.contains_key(name) {
                out.push(name.clone());
            } else if let Some(inner) = state.derived.get(name) {
                let inner = inner.clone();
                collect_deps_into(state, &inner, out);
            } else {
                // Unknown lens — record it so a later definition still wires up.
                out.push(name.clone());
            }
        }
        Expr::Unary { expr, .. } => collect_deps_into(state, expr, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_deps_into(state, lhs, out);
            collect_deps_into(state, rhs, out);
        }
    }
}

/// Append a freshly computed materialised result as a new layer.  Engine-
/// generated, so no per-tau type check — the value types come from the
/// expression, not user input.  Newest layer wins at query time.
fn append_materialised_layer(
    state: &mut DbState,
    name: &str,
    taus: Vec<(Timestamp, Timestamp, Value)>,
) -> Result<(), ExecError> {
    if taus.is_empty() {
        return Ok(());
    }
    let mut tau_vec: Vec<Tau<Value>> = taus
        .into_iter()
        .map(|(s, e, v)| Tau::new(s, e, v))
        .collect();
    tau_vec.sort_by_key(|t| t.start());
    let id = state.next_layer_id;
    state.next_layer_id += 1;
    let layer = Layer::new_sorted_unchecked(id, tau_vec, state.clock.now_ms());
    state.db.append(name, layer)?;
    Ok(())
}

/// Re-materialise every view that depends on `changed` and, recursively, any
/// views that depend on those (chained XDERIVEs).  `visited` makes the walk
/// terminate; definition cycles are already rejected at `XDERIVE` time.
fn rematerialise_dependents(
    state: &mut DbState,
    changed: &str,
    visited: &mut HashSet<String>,
) -> Result<(), ExecError> {
    let targets: Vec<String> = state
        .xderived
        .iter()
        .filter(|(mname, def)| def.deps.iter().any(|d| d == changed) && !visited.contains(*mname))
        .map(|(mname, _)| mname.clone())
        .collect();
    for m in targets {
        if !visited.insert(m.clone()) {
            continue;
        }
        let def = state.xderived.get(&m).expect("present above").clone();
        let taus = materialise_expr(state, &def.expr, def.range)?;
        append_materialised_layer(state, &m, taus)?;
        rematerialise_dependents(state, &m, visited)?;
    }
    Ok(())
}

/// Returns `true` for the statement kinds that are deferred when a transaction
/// is active.  Only lens-scoped mutations are buffered; database management,
/// user management, and DDL that operates outside lens storage are not.
pub(crate) fn is_transactable(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Create { .. }
            | Stmt::Append { .. }
            | Stmt::AppendNd { .. }
            | Stmt::BatchAppend { .. }
            | Stmt::Copy { .. }
            | Stmt::Derive { .. }
            | Stmt::Xderive { .. }
            | Stmt::Drop { .. }
            | Stmt::SetTtl { .. }
            | Stmt::UnsetTtl { .. }
    )
}
