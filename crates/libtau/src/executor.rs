//! Query-language executor.
//!
//! Wires the [`Stmt`](crate::ql::ast::Stmt) AST to a runtime registry
//! of [`Database<Value>`](crate::database::Database) instances.
//!
//! # Model
//!
//! * The executor owns a `HashMap` of *named databases*.  `CREATE DATABASE`
//!   adds one, `DROP DATABASE` removes one, `USE DATABASE` selects the
//!   active database for subsequent lens statements.
//! * Each database has a single value type - all of its *base* lenses must
//!   share the type declared at `CREATE LENS`.  An `APPEND` whose literal
//!   does not match the declared type is rejected with [`ExecError::TypeMismatch`].
//! * *Derived* lenses are pure expressions over other lenses.  Their result
//!   type is whatever the expression yields, which may differ from the
//!   underlying base type (e.g. a `bool` derived from an `int` lens).  A
//!   cycle in the derivation graph is rejected at `DERIVE` time with
//!   [`ExecError::CycleDetected`].
//! * Every lookup walks the AST live - derived lenses never cache.
//!
//! # Statement semantics
//!
//! DDL (`CREATE`/`DROP`/`USE`/`APPEND`/`COPY`/`DERIVE`) returns [`Output::Empty`].
//! `AT` returns [`Output::Value`] (`None` when no tau covers `t`).
//! `RANGE` returns [`Output::Range`] - a vec of `(start, end, value)` segments.
//! `REDUCE` returns [`Output::Value`] - a single scalar aggregate.
//! `SHOW DATABASES` / `SHOW LENSES` return [`Output::Names`] - a sorted name list.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::database::Database;
use crate::metrics::Metrics;
use crate::metrics::Op;
use crate::model::{Layer, LayerId, Tau, Timestamp};
use crate::ql::ast::{AggFunc, Expr, Stmt, Type};
use crate::query::{
    build_range_segments, collect_range_bounds, eval_agg, eval_lens, ttl_cutoff, would_cycle,
};
use crate::storage::{
    Disk, InMemory, Sstable, Store,
    wal::{Wal, WalEntry},
};
use crate::users::{Perm, User, UserStore};
use crate::value::Value;

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

/// All errors the executor can produce.
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

/// Per-database executor state.
/// Where newly-created databases persist their layer data.
#[derive(Clone)]
pub enum StorageBackend {
    /// In-memory only. WAL (if configured) provides crash recovery; data is
    /// lost on clean shutdown without a WAL.
    Memory,
    /// Every database gets its own `<dir>/<name>.dat` compressed disk file.
    /// Both layer data and schema DDL (CREATE LENS / DERIVE LENS / SET TTL) are
    /// persisted; `CREATE DATABASE <name>` re-opens an existing file and replays
    /// its schema, so lenses survive a restart.
    Disk {
        dir: std::path::PathBuf,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
        /// Applied to each database's per-file WAL (`<dir>/<name>.wal`).
        wal_fsync_each: bool,
        wal_max_bytes: Option<u64>,
    },
    /// Every database gets its own `<dir>/<name>.manifest` + `<dir>/<name>.run.<id>`
    /// SSTable files (see [`crate::storage::Sstable`]): an in-memory memtable
    /// absorbs appends, and checkpoints flush it into a new immutable run
    /// instead of rewriting the whole database, with newest-wins/`AS OF`
    /// resolved at read time (MVCC) across the memtable and runs.
    Sstable {
        dir: std::path::PathBuf,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
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
}

impl DbState {
    /// Wrap an opened database, restoring `next_layer_id` past any replayed
    /// layers so new layers never collide with persisted ones.
    fn from_db(db: Database<Value>) -> Self {
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
        }
    }

    fn new(compact_threshold: usize) -> Self {
        Self::from_db(Database::new(InMemory::<Value>::with_threshold(
            compact_threshold,
        )))
    }

    /// Open (or create) a disk-backed database paired with a per-database WAL,
    /// returning the fresh state plus any schema DDL persisted so the executor
    /// can replay it.
    ///
    /// The WAL (`<dir>/<name>.wal`, alongside the `.dat` file) is the
    /// durability mechanism for every `append`: writes hit the WAL first and
    /// the disk file's full compress+rewrite only happens on
    /// compaction/checkpoint. On restart, any WAL entries written since the
    /// last checkpoint are replayed on top of the loaded disk file.
    fn with_disk(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
        wal_fsync_each: bool,
        wal_max_bytes: Option<u64>,
    ) -> io::Result<(Self, Vec<String>)> {
        let path = path.as_ref();
        let mut store = if path.exists() {
            Disk::open(path, enc_key)?
        } else {
            Disk::create(path, enc_key)?
        };
        store.set_compact_threshold(compact_threshold);
        store.set_compression_level(compression_level);

        let wal_path = path.with_extension("wal");
        let mut wal = Wal::open(&wal_path, enc_key)?;
        wal.replay(&mut store)?;

        // Migrate schema DDL embedded in older disk files (pre-WAL format)
        // into the WAL, which is now the source of truth for schema.
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
        Ok((Self::from_db(db), schema_stmts))
    }

    fn with_sstable(
        base: impl AsRef<Path>,
        compact_threshold: usize,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
        wal_fsync_each: bool,
        wal_max_bytes: Option<u64>,
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
        Ok((Self::from_db(db), schema_stmts))
    }

    fn with_wal(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        key: Option<[u8; 32]>,
    ) -> io::Result<(Self, Vec<String>)> {
        let store = InMemory::<Value>::with_threshold(compact_threshold);
        let db = Database::open(store, path, key)?;
        // Schema stmts are replayed in the executor after construction.
        let schema_stmts = db.schema_stmts()?;
        Ok((Self::from_db(db), schema_stmts))
    }
}

/// Runtime container for executing parsed [`Stmt`]s.
pub struct Executor {
    databases: HashMap<String, Arc<RwLock<DbState>>>,
    /// Name of the currently active database (set by the first
    /// `CREATE DATABASE` and by `USE DATABASE`).  Cleared if the active
    /// database is dropped.
    active: Option<String>,
    compact_threshold: usize,
    /// When `true`, `create_lens` and `derive_lens` skip writing to the WAL.
    /// Set during schema replay on startup to avoid re-writing entries that
    /// were just read from the WAL.
    in_replay: bool,
    users: UserStore,
    metrics: Arc<Metrics>,
    /// Buffered mutations for the active transaction.  `None` means no
    /// transaction is open.  Each entry pairs the active database name (at the
    /// time the statement was buffered) with the statement itself so COMMIT can
    /// replay each mutation against the correct database regardless of which
    /// database is active when COMMIT runs.
    pending: Option<Vec<(Option<String>, Stmt)>>,
    /// Storage backend used when `CREATE DATABASE` allocates a new store.
    backend: StorageBackend,
    /// Instant the executor was created — used to report `uptime_secs` in
    /// `SHOW STATUS`.
    started_at: std::time::Instant,
}

fn apply_offset_limit<T>(v: Vec<T>, offset: Option<usize>, limit: Option<usize>) -> Vec<T> {
    if offset.is_none() && limit.is_none() {
        return v;
    }
    v.into_iter()
        .skip(offset.unwrap_or(0))
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

/// Convert parsed literal taus to runtime values (Arc-bump for strings).
fn literal_taus(
    taus: &[(i64, i64, crate::ql::ast::Literal)],
) -> Vec<(Timestamp, Timestamp, Value)> {
    taus.iter().map(|(s, e, l)| (*s, *e, l.into())).collect()
}

/// The lens must exist as either a base or a derived lens.
fn ensure_lens_exists(state: &DbState, name: &str) -> Result<(), ExecError> {
    if state.base_types.contains_key(name) || state.derived.contains_key(name) {
        Ok(())
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

/// The lens must be a base lens; derived lenses get a targeted error.
fn ensure_base_lens(state: &DbState, name: &str, stmt_kind: &str) -> Result<(), ExecError> {
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
fn ensure_single_axis(state: &DbState, name: &str, stmt_kind: &str) -> Result<(), ExecError> {
    match state.axes.get(name) {
        Some(axes) => Err(ExecError::InvalidExpr(format!(
            "{stmt_kind}: lens '{name}' has {} axes; supply one coordinate per axis",
            axes.len()
        ))),
        None => Ok(()),
    }
}

fn record_metrics(metrics: &Metrics, active: Option<&str>, stmt: &Stmt, ns: u64) {
    let op = stmt_to_op(stmt);
    metrics.record_op(op, ns);
    if let Some(db) = active {
        metrics.record_db_op(db, op);
    }
}

fn stmt_to_op(stmt: &Stmt) -> Op {
    match stmt {
        Stmt::Append { .. } | Stmt::BatchAppend { .. } | Stmt::AppendNd { .. } => Op::Append,
        Stmt::Copy { .. } => Op::Copy,
        Stmt::At { .. } | Stmt::AtNd { .. } | Stmt::AtAsOf { .. } | Stmt::AtLayer { .. } => Op::At,
        Stmt::Range { .. } | Stmt::RangeNd { .. } => Op::Range,
        Stmt::Reduce { .. } => Op::Reduce,
        Stmt::HistoryLens { .. } => Op::History,
        Stmt::Create { .. } | Stmt::Derive { .. } | Stmt::Xderive { .. } => Op::CreateLens,
        Stmt::Drop { .. } => Op::DropLens,
        Stmt::ShowDatabases
        | Stmt::ShowLenses
        | Stmt::ShowStatus
        | Stmt::ShowUsers
        | Stmt::ShowGrants { .. } => Op::Show,
        Stmt::CreateDatabase { .. } | Stmt::DropDatabase { .. } | Stmt::UseDatabase { .. } => {
            Op::Database
        }
        Stmt::SetTtl { .. } | Stmt::UnsetTtl { .. } => Op::CreateLens,
        Stmt::CreateUser { .. }
        | Stmt::DropUser { .. }
        | Stmt::Grant { .. }
        | Stmt::Revoke { .. } => Op::User,
        Stmt::BackupDatabase { .. } | Stmt::RestoreDatabase { .. } => Op::Backup,
        Stmt::StartTransaction | Stmt::Commit | Stmt::Rollback => Op::Transaction,
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::with_threshold(crate::storage::COMPACT_THRESHOLD)
    }
}

impl Executor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an executor with a custom layer compaction threshold.
    pub fn with_threshold(compact_threshold: usize) -> Self {
        Self {
            databases: HashMap::default(),
            active: None,
            compact_threshold,
            in_replay: false,
            users: UserStore::new(),
            metrics: Metrics::arc(),
            pending: None,
            backend: StorageBackend::Memory,
            started_at: std::time::Instant::now(),
        }
    }

    /// Create a disk-backed executor. Each `CREATE DATABASE` allocates a
    /// `<dir>/<name>.dat` compressed disk file paired with a `<dir>/<name>.wal`
    /// write-ahead log, which is the durability mechanism for every append.
    /// `wal_fsync_each` and `wal_max_bytes` (from `[wal]` config) are applied
    /// to each database's WAL.
    pub fn with_disk_backend(
        dir: impl AsRef<Path>,
        compact_threshold: usize,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
        wal_fsync_each: bool,
        wal_max_bytes: Option<u64>,
    ) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let mut executor = Self::with_threshold(compact_threshold);
        executor.backend = StorageBackend::Disk {
            dir,
            compression_level,
            enc_key,
            wal_fsync_each,
            wal_max_bytes,
        };
        Ok(executor)
    }

    /// Create an SSTable-backed executor (see [`crate::storage::Sstable`]).
    /// Each `CREATE DATABASE` allocates `<dir>/<name>.manifest` +
    /// `<dir>/<name>.run.<id>` files paired with a `<dir>/<name>.wal` WAL.
    pub fn with_sstable_backend(
        dir: impl AsRef<Path>,
        compact_threshold: usize,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
        wal_fsync_each: bool,
        wal_max_bytes: Option<u64>,
    ) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let mut executor = Self::with_threshold(compact_threshold);
        executor.backend = StorageBackend::Sstable {
            dir,
            compression_level,
            enc_key,
            wal_fsync_each,
            wal_max_bytes,
        };
        Ok(executor)
    }

    /// Open a WAL-backed executor with the default compaction threshold.
    pub fn with_wal(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        Self::with_wal_threshold(path, crate::storage::COMPACT_THRESHOLD, key)
    }

    /// Open a WAL-backed executor with a custom compaction threshold.
    pub fn with_wal_threshold(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        key: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        let mut executor = Self::with_threshold(compact_threshold);
        let (db_state, schema_stmts) = DbState::with_wal(path, compact_threshold, key)?;
        executor
            .databases
            .insert("default".to_string(), Arc::new(RwLock::new(db_state)));
        // Replay schema DDL (CREATE LENS / DERIVE LENS / SET TTL); in_replay
        // suppresses writing these back to the WAL.
        executor.replay_schema_stmts("default", &schema_stmts);
        executor.active = Some("default".to_string());
        Ok(executor)
    }

    /// Name of the active database, if any.
    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Disable per-record WAL fsync across all databases.  Caller is
    /// responsible for periodic `flush_wal()` calls to enforce durability
    /// boundaries.  Intended for bulk-load paths.
    pub fn set_wal_fsync_each(&mut self, on: bool) {
        for arc in self.databases.values() {
            arc.write()
                .expect("db lock poisoned")
                .db
                .set_wal_fsync_each(on);
        }
    }

    /// Set a soft WAL file-size cap (bytes) across all databases.  Once a WAL
    /// file reaches `bytes`, the next `APPEND` triggers a checkpoint rewrite
    /// that compacts it to only live layers.
    pub fn set_wal_max_bytes(&mut self, bytes: u64) {
        for arc in self.databases.values() {
            arc.write()
                .expect("db lock poisoned")
                .db
                .set_wal_max_bytes(bytes);
        }
    }

    /// Flush the WAL for all databases.  Used with group-commit mode
    /// (`set_wal_fsync_each(false)`) to enforce periodic durability.
    pub fn flush_wal(&self) -> io::Result<()> {
        for arc in self.databases.values() {
            arc.read().expect("db lock poisoned").db.wal_flush()?;
        }
        Ok(())
    }

    /// Execute a read-only statement (`AT` or `RANGE`) without taking an
    /// exclusive borrow.  Returns [`ExecError::InvalidExpr`] for any
    /// mutating statement.
    ///
    /// The TCP server uses this entry point under a shared read lock so
    /// concurrent lookups don't serialise on each other.
    pub fn exec_read(&self, stmt: &Stmt) -> Result<Output, ExecError> {
        let t0 = Instant::now();
        let result = match stmt {
            Stmt::At { name, t } => self.at_lens(name, *t),
            Stmt::AtNd { name, ts, as_of } => self.at_nd_lens(name, ts, *as_of),
            Stmt::AtAsOf { name, t, as_of } => self.at_as_of_lens(name, *t, *as_of),
            Stmt::AtLayer { name, t, layer_id } => self.at_layer_lens(name, *t, *layer_id),
            Stmt::HistoryLens { name, range } => self.history_lens(name, *range),
            Stmt::Range {
                name,
                start,
                end,
                filter,
                limit,
                offset,
            } => self.range_lens(name, *start, *end, filter.as_ref(), *limit, *offset),
            Stmt::RangeNd {
                name,
                start,
                end,
                fixed,
            } => self.range_nd_lens(name, *start, *end, fixed),
            Stmt::Reduce {
                name,
                start,
                end,
                func,
            } => self.reduce_lens(name, *start, *end, *func),
            Stmt::ShowDatabases => self.show_databases(),
            Stmt::ShowLenses => self.show_lenses(),
            Stmt::ShowUsers => self.show_users(),
            Stmt::ShowGrants { user } => self.show_grants(user.as_deref()),
            Stmt::ShowStatus => self.show_status(),
            Stmt::BackupDatabase { name, path } => self.backup_database(name, path),
            _ => Err(ExecError::InvalidExpr(
                "exec_read called on a mutating statement".into(),
            )),
        };
        record_metrics(
            &self.metrics,
            self.active.as_deref(),
            stmt,
            t0.elapsed().as_nanos() as u64,
        );
        result
    }

    /// Execute a single parsed statement.
    pub fn exec(&mut self, stmt: &Stmt) -> Result<Output, ExecError> {
        let t0 = Instant::now();
        // While inside a transaction, buffer mutable lens statements.
        // Each entry captures the active database at buffer time so COMMIT can
        // replay the statement against the right database.
        if let Some(pending) = &mut self.pending
            && is_transactable(stmt)
        {
            pending.push((self.active.clone(), stmt.clone()));
            return Ok(Output::Empty);
        }
        let result = match stmt {
            Stmt::StartTransaction => self.start_transaction(),
            Stmt::Commit => self.commit(),
            Stmt::Rollback => self.rollback(),
            Stmt::CreateDatabase { name } => self.create_database(name),
            Stmt::DropDatabase { name } => self.drop_database(name),
            Stmt::UseDatabase { name } => self.use_database(name),
            Stmt::Create { name, ty, axes } => self.create_lens(name, ty.clone(), axes),
            Stmt::Append { name, taus } => self.write_layer(name, literal_taus(taus)),
            Stmt::AppendNd { name, taus } => self.append_nd_lens(name, taus),
            Stmt::Copy { name, path } => self.copy_lens(name, path),
            Stmt::Derive { name, expr, range } => self.derive_lens(name, expr.clone(), *range),
            Stmt::Xderive { name, expr, range } => self.xderive_lens(name, expr.clone(), *range),
            Stmt::At { name, t } => self.at_lens(name, *t),
            Stmt::AtNd { name, ts, as_of } => self.at_nd_lens(name, ts, *as_of),
            Stmt::Range {
                name,
                start,
                end,
                filter,
                limit,
                offset,
            } => self.range_lens(name, *start, *end, filter.as_ref(), *limit, *offset),
            Stmt::RangeNd {
                name,
                start,
                end,
                fixed,
            } => self.range_nd_lens(name, *start, *end, fixed),
            Stmt::Drop { name } => self.drop_lens(name),
            Stmt::ShowDatabases => self.show_databases(),
            Stmt::ShowLenses => self.show_lenses(),
            Stmt::ShowStatus => self.show_status(),
            Stmt::Reduce {
                name,
                start,
                end,
                func,
            } => self.reduce_lens(name, *start, *end, *func),
            Stmt::SetTtl { name, secs } => self.update_ttl(name, Some(*secs)),
            Stmt::UnsetTtl { name } => self.update_ttl(name, None),
            Stmt::CreateUser { name, password } => self.create_user(name, password),
            Stmt::DropUser { name } => self.drop_user(name),
            Stmt::Grant {
                perms,
                database,
                user,
            } => self.grant(*perms, database, user),
            Stmt::Revoke {
                perms,
                database,
                user,
            } => self.revoke(*perms, database, user),
            Stmt::ShowUsers => self.show_users(),
            Stmt::ShowGrants { user } => self.show_grants(user.as_deref()),
            Stmt::BatchAppend { name, taus } => self.write_layer(name, literal_taus(taus)),
            Stmt::AtAsOf { name, t, as_of } => self.at_as_of_lens(name, *t, *as_of),
            Stmt::AtLayer { name, t, layer_id } => self.at_layer_lens(name, *t, *layer_id),
            Stmt::HistoryLens { name, range } => self.history_lens(name, *range),
            Stmt::BackupDatabase { name, path } => self.backup_database(name, path),
            Stmt::RestoreDatabase { name, path } => self.restore_database(name, path),
        };
        record_metrics(
            &self.metrics,
            self.active.as_deref(),
            stmt,
            t0.elapsed().as_nanos() as u64,
        );
        result
    }

    /// Execute a statement on behalf of `caller`, applying permission checks
    /// before delegating to [`Executor::exec`].
    ///
    /// `SHOW DATABASES` is filtered to databases the caller has any grant on.
    /// Read-only statements still route through the standard path; the locking
    /// router in the TCP server picks the right lock variant.
    pub fn exec_as(&mut self, stmt: &Stmt, caller: &str) -> Result<Output, ExecError> {
        // Check permission under an immutable borrow, then drop it before the
        // mutable exec call.  We only clone when SHOW DATABASES needs the grants
        // for post-filtering, avoiding a full User clone on every statement.
        {
            let user = self
                .users
                .get(caller)
                .ok_or_else(|| ExecError::UnknownUser(caller.into()))?;
            self.check_permission(stmt, user)?;
        }
        let out = self.exec(stmt)?;
        let user = self
            .users
            .get(caller)
            .ok_or_else(|| ExecError::UnknownUser(caller.into()))?;
        Ok(filter_show_databases(out, stmt, user))
    }

    /// Read-only counterpart of [`Executor::exec_as`].  Same permission rules.
    pub fn exec_read_as(&self, stmt: &Stmt, caller: &str) -> Result<Output, ExecError> {
        let user = self
            .users
            .get(caller)
            .ok_or_else(|| ExecError::UnknownUser(caller.into()))?;
        self.check_permission(stmt, user)?;
        let out = self.exec_read(stmt)?;
        Ok(filter_show_databases(out, stmt, user))
    }

    /// Execute a non-registry data-write statement under a per-database write
    /// lock, holding only the shared executor lock.  This allows concurrent
    /// reads (and writes to *other* databases) while this write is in flight.
    ///
    /// Only call this when [`Executor::is_in_transaction`] is `false`; if a
    /// transaction is active the caller must use [`Executor::exec`] instead so
    /// mutations are buffered.
    ///
    /// Returns `Err(InvalidExpr)` for registry statements — those require
    /// `exec`.
    pub fn exec_db_write(&self, stmt: &Stmt) -> Result<Output, ExecError> {
        let t0 = Instant::now();
        let result = match stmt {
            Stmt::Create { name, ty, axes } => self.create_lens(name, ty.clone(), axes),
            Stmt::Append { name, taus } | Stmt::BatchAppend { name, taus } => {
                self.write_layer(name, literal_taus(taus))
            }
            Stmt::AppendNd { name, taus } => self.append_nd_lens(name, taus),
            Stmt::Copy { name, path } => self.copy_lens(name, path),
            Stmt::Derive { name, expr, range } => self.derive_lens(name, expr.clone(), *range),
            Stmt::Xderive { name, expr, range } => self.xderive_lens(name, expr.clone(), *range),
            Stmt::Drop { name } => self.drop_lens(name),
            Stmt::BackupDatabase { name, path } => self.backup_database(name, path),
            _ => Err(ExecError::InvalidExpr(
                "exec_db_write: not a data-write statement".into(),
            )),
        };
        record_metrics(
            &self.metrics,
            self.active.as_deref(),
            stmt,
            t0.elapsed().as_nanos() as u64,
        );
        result
    }

    /// Permission-checking wrapper around [`Executor::exec_db_write`].
    pub fn exec_db_write_as(&self, stmt: &Stmt, caller: &str) -> Result<Output, ExecError> {
        {
            let user = self
                .users
                .get(caller)
                .ok_or_else(|| ExecError::UnknownUser(caller.into()))?;
            self.check_permission(stmt, user)?;
        }
        self.exec_db_write(stmt)
    }

    /// Per-statement permission check.  Returns `Err(PermissionDenied)` when
    /// the caller does not have the right grants.
    fn check_permission(&self, stmt: &Stmt, user: &User) -> Result<(), ExecError> {
        let active = self.active.as_deref();
        let require = |db: &str, bit: Perm| {
            if user.effective(db).contains(bit) {
                Ok(())
            } else {
                Err(ExecError::PermissionDenied(format!(
                    "user '{}' lacks {} on {}",
                    user.name, bit, db
                )))
            }
        };
        let require_global_admin = || {
            if user.is_global_admin() {
                Ok(())
            } else {
                Err(ExecError::PermissionDenied(format!(
                    "user '{}' is not a global admin",
                    user.name
                )))
            }
        };
        let require_active = || active.ok_or(ExecError::NoActiveDatabase);
        let require_admin_or_a_on = |db: &str| {
            if user.is_global_admin() || user.effective(db).contains(Perm::A) {
                Ok(())
            } else {
                Err(ExecError::PermissionDenied(format!(
                    "user '{}' lacks A on {}",
                    user.name, db
                )))
            }
        };

        let require_any_grant = |db: &str| {
            if user.effective(db).is_empty() {
                Err(ExecError::PermissionDenied(format!(
                    "user '{}' has no grants on {}",
                    user.name, db
                )))
            } else {
                Ok(())
            }
        };
        match stmt {
            Stmt::CreateDatabase { .. } => require_global_admin(),
            Stmt::DropDatabase { name } => require_admin_or_a_on(name),
            Stmt::UseDatabase { name } => require_any_grant(name),
            Stmt::Create { .. } => require(require_active()?, Perm::C),
            Stmt::Append { .. } | Stmt::AppendNd { .. } | Stmt::Copy { .. } => {
                require(require_active()?, Perm::U)
            }
            Stmt::Derive { .. } | Stmt::Xderive { .. } => require(require_active()?, Perm::C),
            Stmt::At { .. }
            | Stmt::AtNd { .. }
            | Stmt::Range { .. }
            | Stmt::RangeNd { .. }
            | Stmt::Reduce { .. } => require(require_active()?, Perm::R),
            Stmt::Drop { .. } => require(require_active()?, Perm::D),
            Stmt::ShowDatabases => Ok(()),
            Stmt::ShowLenses => require(require_active()?, Perm::R),
            Stmt::CreateUser { .. } | Stmt::DropUser { .. } | Stmt::ShowUsers => {
                require_global_admin()
            }
            Stmt::Grant { database, .. } | Stmt::Revoke { database, .. } => {
                require_admin_or_a_on(database)
            }
            Stmt::ShowGrants { user: target } => {
                if target.as_deref().is_some_and(|t| t == user.name) {
                    return Ok(());
                }
                require_global_admin()
            }
            Stmt::StartTransaction | Stmt::Commit | Stmt::Rollback => Ok(()),
            Stmt::BatchAppend { .. } => require(require_active()?, Perm::U),
            Stmt::AtAsOf { .. } | Stmt::AtLayer { .. } | Stmt::HistoryLens { .. } => {
                require(require_active()?, Perm::R)
            }
            Stmt::SetTtl { .. } | Stmt::UnsetTtl { .. } => require(require_active()?, Perm::C),
            Stmt::ShowStatus => Ok(()),
            Stmt::BackupDatabase { name, .. } => require(name, Perm::R),
            Stmt::RestoreDatabase { .. } => require_global_admin(),
        }
    }

    fn create_user(&mut self, name: &str, password: &str) -> Result<Output, ExecError> {
        if self.users.get(name).is_some() {
            return Err(ExecError::DuplicateUser(name.into()));
        }
        self.users
            .add(User::new(name, password, Default::default()))
            .map_err(ExecError::Io)?;
        Ok(Output::Empty)
    }

    fn drop_user(&mut self, name: &str) -> Result<Output, ExecError> {
        self.users
            .remove(name)
            .map_err(|_| ExecError::UnknownUser(name.into()))?;
        Ok(Output::Empty)
    }

    fn grant(&mut self, perms: Perm, database: &str, user: &str) -> Result<Output, ExecError> {
        if self.users.get(user).is_none() {
            return Err(ExecError::UnknownUser(user.into()));
        }
        self.users
            .grant(user, database, perms)
            .map_err(ExecError::Io)?;
        Ok(Output::Empty)
    }

    fn revoke(&mut self, perms: Perm, database: &str, user: &str) -> Result<Output, ExecError> {
        if self.users.get(user).is_none() {
            return Err(ExecError::UnknownUser(user.into()));
        }
        self.users
            .revoke(user, database, perms)
            .map_err(ExecError::Io)?;
        Ok(Output::Empty)
    }

    fn show_users(&self) -> Result<Output, ExecError> {
        Ok(Output::Names(self.users.names()))
    }

    fn show_grants(&self, target: Option<&str>) -> Result<Output, ExecError> {
        let mut out = Vec::new();
        match target {
            Some(name) => {
                let grants = self
                    .users
                    .grants_for(name)
                    .ok_or_else(|| ExecError::UnknownUser(name.into()))?;
                out.push((name.to_string(), grants));
            }
            None => {
                for name in self.users.names() {
                    if let Some(g) = self.users.grants_for(&name) {
                        out.push((name, g));
                    }
                }
            }
        }
        Ok(Output::Grants(out))
    }

    fn start_transaction(&mut self) -> Result<Output, ExecError> {
        if self.pending.is_some() {
            return Err(ExecError::TransactionAlreadyActive);
        }
        self.pending = Some(Vec::new());
        Ok(Output::Empty)
    }

    fn commit(&mut self) -> Result<Output, ExecError> {
        let entries = self.pending.take().ok_or(ExecError::NoActiveTransaction)?;
        // pending is now None so the buffering intercept in exec is inactive
        // and the replayed statements go straight to storage.
        let active_before = self.active.clone();
        for (db, stmt) in entries {
            // Restore the DB context that was active when this stmt was buffered.
            self.active = db;
            self.exec(&stmt)?;
        }
        self.active = active_before;
        Ok(Output::Empty)
    }

    fn rollback(&mut self) -> Result<Output, ExecError> {
        self.pending.take().ok_or(ExecError::NoActiveTransaction)?;
        Ok(Output::Empty)
    }

    fn create_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if self.databases.contains_key(name) {
            return Err(ExecError::DuplicateDatabase(name.into()));
        }
        let metrics = self.metrics.clone();
        let (mut db_state, schema_stmts) = match &self.backend {
            StorageBackend::Memory => (DbState::new(self.compact_threshold), Vec::new()),
            StorageBackend::Disk {
                dir,
                compression_level,
                enc_key,
                wal_fsync_each,
                wal_max_bytes,
            } => {
                let path = dir.join(format!("{name}.dat"));
                DbState::with_disk(
                    &path,
                    self.compact_threshold,
                    *compression_level,
                    *enc_key,
                    *wal_fsync_each,
                    *wal_max_bytes,
                )?
            }
            StorageBackend::Sstable {
                dir,
                compression_level,
                enc_key,
                wal_fsync_each,
                wal_max_bytes,
            } => {
                let base = dir.join(name);
                DbState::with_sstable(
                    &base,
                    self.compact_threshold,
                    *compression_level,
                    *enc_key,
                    *wal_fsync_each,
                    *wal_max_bytes,
                )?
            }
        };
        db_state.db.set_metrics(metrics);
        self.databases
            .insert(name.into(), Arc::new(RwLock::new(db_state)));
        // Rebuild lens schema (CREATE/DERIVE/SET TTL) for a re-opened disk file.
        self.replay_schema_stmts(name, &schema_stmts);
        if self.active.is_none() {
            self.active = Some(name.into());
        }
        Ok(Output::Empty)
    }

    /// Re-execute persisted schema DDL against `db_name` with WAL/disk writes
    /// suppressed (`in_replay`), restoring the previously active database
    /// afterwards.  Shared by the WAL-open, disk-open, and RESTORE paths.
    fn replay_schema_stmts(&mut self, db_name: &str, stmts: &[String]) {
        if stmts.is_empty() {
            return;
        }
        let prev_active = self.active.take();
        self.active = Some(db_name.to_string());
        self.in_replay = true;
        for stmt_text in stmts {
            match crate::ql::parser::parse(stmt_text) {
                Ok((_, stmt)) => {
                    if let Err(e) = self.exec(&stmt) {
                        tracing::warn!(stmt = %stmt_text, error = ?e, "schema replay: statement failed, skipping");
                    }
                }
                Err(e) => {
                    tracing::warn!(stmt = %stmt_text, error = %e, "schema replay: parse error, skipping");
                }
            }
        }
        self.in_replay = false;
        self.active = prev_active;
    }

    fn drop_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if self.databases.remove(name).is_none() {
            return Err(ExecError::UnknownDatabase(name.into()));
        }
        if self.active.as_deref() == Some(name) {
            self.active = None;
        }
        Ok(Output::Empty)
    }

    fn use_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if !self.databases.contains_key(name) {
            return Err(ExecError::UnknownDatabase(name.into()));
        }
        self.active = Some(name.into());
        Ok(Output::Empty)
    }

    fn create_lens(&self, name: &str, ty: Type, axes: &[String]) -> Result<Output, ExecError> {
        let in_replay = self.in_replay;
        let db_arc = self.active_db_arc()?;
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

    /// Arity-mismatch error shared by the N-D read/write paths.
    fn arity_error(name: &str, declared: usize, supplied: usize) -> ExecError {
        ExecError::InvalidExpr(format!(
            "lens '{name}' has {declared} axes but the statement supplies {supplied}"
        ))
    }

    /// Shared write path for `APPEND`, `BATCH APPEND`, and `COPY`: type-check
    /// every tau against the lens's declared type, sort, reject overlaps and
    /// empty intervals with [`ExecError::InvalidRange`], then append one layer.
    fn write_layer(
        &self,
        name: &str,
        taus: Vec<(Timestamp, Timestamp, Value)>,
    ) -> Result<Output, ExecError> {
        if taus.is_empty() {
            return Ok(Output::Empty);
        }
        let db_arc = self.active_db_arc()?;
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
            return Err(Self::arity_error(name, axes.len(), 1));
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
        let layer = Layer::new_sorted_unchecked(id, tau_vec, crate::model::now_ms());
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
    /// [`Executor::write_layer`] so it shares the 1-D invariants and the
    /// materialised-view refresh.
    fn append_nd_lens(
        &self,
        name: &str,
        taus: &[(Vec<(i64, i64)>, crate::ql::ast::Literal)],
    ) -> Result<Output, ExecError> {
        if taus.is_empty() {
            return Ok(Output::Empty);
        }
        let db_arc = self.active_db_arc()?;
        let arity = {
            let state = db_arc.read().expect("db lock poisoned");
            state.axes.get(name).map_or(1, Vec::len)
        };
        if arity == 1 {
            let flat: Result<Vec<_>, ExecError> = taus
                .iter()
                .map(|(coords, lit)| match coords.as_slice() {
                    [(s, e)] => Ok((*s, *e, Value::from(lit))),
                    _ => Err(Self::arity_error(name, 1, coords.len())),
                })
                .collect();
            return self.write_layer(name, flat?);
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
                return Err(Self::arity_error(name, arity, coords.len()));
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
        let layer = Layer::try_new_nd_at(id, tau_vec, crate::model::now_ms())
            .ok_or(ExecError::InvalidRange)?;
        state.db.append(name, layer)?;
        Ok(Output::Empty)
    }

    /// N-dimensional point lookup: newest layer wins, optionally scoped to
    /// layers written at or before `as_of`.
    fn at_nd_lens(&self, name: &str, ts: &[i64], as_of: Option<i64>) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_base_lens(&state, name, "AT")?;
        let arity = state.axes.get(name).map_or(1, Vec::len);
        if ts.len() != arity {
            return Err(Self::arity_error(name, arity, ts.len()));
        }
        Ok(Output::Value(state.db.get(name, ts, as_of)))
    }

    /// N-dimensional range scan: sweep valid time over `[start, end)` with the
    /// remaining axes fixed at `fixed`. Taus whose non-valid axes contain the
    /// fixed points form a 1-D non-overlapping view per layer (two taus that
    /// both cover the fixed points and overlap on valid time would be a full
    /// orthotope overlap, which the append path rejects), so the standard
    /// newest-wins sweep applies unchanged.
    fn range_nd_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        fixed: &[i64],
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_base_lens(&state, name, "RANGE")?;
        let arity = state.axes.get(name).map_or(1, Vec::len);
        if fixed.len() + 1 != arity {
            return Err(Self::arity_error(name, arity, fixed.len() + 1));
        }
        Ok(Output::Range(state.db.scan(name, start, end, fixed, None)))
    }

    fn copy_lens(&self, name: &str, path: &str) -> Result<Output, ExecError> {
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
        self.write_layer(name, taus)
    }

    fn derive_lens(
        &self,
        name: &str,
        expr: Expr,
        range: Option<(Timestamp, Timestamp)>,
    ) -> Result<Output, ExecError> {
        let in_replay = self.in_replay;
        let db_arc = self.active_db_arc()?;
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
        name: &str,
        expr: Expr,
        range: Option<(Timestamp, Timestamp)>,
    ) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let mut state = db_arc.write().expect("db lock poisoned");

        if self.in_replay {
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

        let taus = crate::query::materialise_expr(&state, &expr, range)?;
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

    fn at_lens(&self, name: &str, t: Timestamp) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_single_axis(&state, name, "AT")?;
        if ttl_cutoff(&state, name).is_some_and(|c| t < c) {
            return Ok(Output::Value(None));
        }
        Ok(Output::Value(eval_lens(&state, name, t)?))
    }

    fn range_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        filter: Option<&Expr>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_lens_exists(&state, name)?;
        ensure_single_axis(&state, name, "RANGE")?;
        let effective_start = ttl_cutoff(&state, name).map_or(start, |c| start.max(c));
        if effective_start >= end {
            return Ok(Output::Range(vec![]));
        }
        // Fast path for unfiltered base-lens queries: pushed to the store as a
        // single valid-axis scan (no non-valid axes to fix on a 1-D lens).
        if filter.is_none() && state.base_types.contains_key(name) {
            let out = state.db.scan(name, effective_start, end, &[], None);
            return Ok(Output::Range(apply_offset_limit(out, offset, limit)));
        }
        let (bounds, layers_snap) =
            collect_range_bounds(&state, name, effective_start, end, filter)?;
        let out = build_range_segments(&state, name, &bounds, layers_snap.as_deref(), filter)?;
        Ok(Output::Range(apply_offset_limit(out, offset, limit)))
    }

    fn reduce_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        func: AggFunc,
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_lens_exists(&state, name)?;
        ensure_single_axis(&state, name, "REDUCE")?;
        let effective_start = ttl_cutoff(&state, name).map_or(start, |c| start.max(c));
        if effective_start >= end {
            return Ok(Output::Value(None));
        }
        eval_agg(&state, name, func, effective_start, end).map(Output::Value)
    }

    /// `SET TTL` (`Some(secs)`) / `UNSET TTL` (`None`), persisted to the
    /// schema log unless replaying.
    fn update_ttl(&self, name: &str, secs: Option<i64>) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
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
        if !self.in_replay {
            state.db.append_schema(&stmt_text)?;
        }
        Ok(Output::Empty)
    }

    fn show_status(&self) -> Result<Output, ExecError> {
        let uptime = self.started_at.elapsed().as_secs();
        let db_count = self.databases.len();
        let mut lens_count = 0usize;
        let mut wal_bytes = 0u64;
        for db_arc in self.databases.values() {
            let state = db_arc.read().expect("db lock poisoned");
            lens_count += state.base_types.len() + state.derived.len();
            wal_bytes += state.db.wal_size_bytes();
        }
        Ok(Output::Status(vec![
            ("uptime_secs".into(), uptime.to_string()),
            ("databases".into(), db_count.to_string()),
            ("lenses".into(), lens_count.to_string()),
            ("wal_bytes".into(), wal_bytes.to_string()),
        ]))
    }

    fn show_databases(&self) -> Result<Output, ExecError> {
        let mut names: Vec<String> = self.databases.keys().cloned().collect();
        names.sort();
        Ok(Output::Names(names))
    }

    fn show_lenses(&self) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        let mut names: Vec<String> = state
            .base_types
            .keys()
            .chain(state.derived.keys())
            .cloned()
            .collect();
        names.sort();
        Ok(Output::Names(names))
    }

    fn drop_lens(&self, name: &str) -> Result<Output, ExecError> {
        let in_replay = self.in_replay;
        let db_arc = self.active_db_arc()?;
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

    fn at_as_of_lens(&self, name: &str, t: Timestamp, as_of: i64) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_base_lens(&state, name, "AT AS OF")?;
        ensure_single_axis(&state, name, "AT AS OF")?;
        Ok(Output::Value(state.db.get(name, &[t], Some(as_of))))
    }

    fn at_layer_lens(&self, name: &str, t: Timestamp, layer_id: u64) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_base_lens(&state, name, "AT LAYER")?;
        ensure_single_axis(&state, name, "AT LAYER")?;
        let result = state
            .db
            .layers(name)
            .as_deref()
            .and_then(|ls| ls.iter().find(|l| l.id == layer_id))
            .and_then(|l| l.at(t))
            .cloned();
        Ok(Output::Value(result))
    }

    fn history_lens(&self, name: &str, range: Option<(i64, i64)>) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        ensure_lens_exists(&state, name)?;
        let infos = state
            .db
            .layer_infos(name)
            .into_iter()
            .filter(|(_, _, min_start, max_end, _)| match range {
                Some((start, end)) => *max_end > start && *min_start < end,
                None => true,
            })
            .map(
                |(id, written_at, min_start, max_end, tau_count)| LayerInfo {
                    id,
                    written_at,
                    min_start,
                    max_end,
                    tau_count,
                },
            )
            .collect();
        Ok(Output::LayerHistory(infos))
    }

    fn backup_database(&self, name: &str, path: &str) -> Result<Output, ExecError> {
        let db_arc = self
            .databases
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownDatabase(name.into()))?;
        let state = db_arc.read().expect("db lock poisoned");

        // Build schema DDL from executor in-memory state so backup works even
        // when no WAL is attached to the source database.
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

    fn restore_database(&mut self, name: &str, path: &str) -> Result<Output, ExecError> {
        if self.databases.contains_key(name) {
            return Err(ExecError::DuplicateDatabase(name.into()));
        }
        if !Path::new(path).exists() {
            return Err(ExecError::Io(format!("backup file not found: {path}")));
        }

        let wal = Wal::open(path, None)?;
        let mut store = InMemory::<Value>::with_threshold(self.compact_threshold);
        wal.replay(&mut store)?;
        let schema_stmts = wal.replay_schemas()?;

        self.databases.insert(
            name.into(),
            Arc::new(RwLock::new(DbState::from_db(Database::new(store)))),
        );

        self.replay_schema_stmts(name, &schema_stmts);
        Ok(Output::Empty)
    }

    /// Returns the `Arc<RwLock<DbState>>` for the active database.
    /// Clone the Arc to hold it across re-borrows of `self`.
    fn active_db_arc(&self) -> Result<Arc<RwLock<DbState>>, ExecError> {
        let name = self.active.as_deref().ok_or(ExecError::NoActiveDatabase)?;
        self.databases
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownDatabase(name.into()))
    }

    /// Whether a transaction is currently buffering mutations on this executor.
    /// The server uses this to route data writes through the cheaper
    /// `exec.read()` path when no transaction is active.
    pub fn is_in_transaction(&self) -> bool {
        self.pending.is_some()
    }

    pub fn users(&self) -> &UserStore {
        &self.users
    }

    pub fn users_mut(&mut self) -> &mut UserStore {
        &mut self.users
    }

    pub fn set_users(&mut self, store: UserStore) {
        self.users = store;
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
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
    let layer = Layer::new_sorted_unchecked(id, tau_vec, crate::model::now_ms());
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
        let taus = crate::query::materialise_expr(state, &def.expr, def.range)?;
        append_materialised_layer(state, &m, taus)?;
        rematerialise_dependents(state, &m, visited)?;
    }
    Ok(())
}

/// Returns `true` for the statement kinds that are deferred when a transaction
/// is active.  Only lens-scoped mutations are buffered; database management,
/// user management, and DDL that operates outside lens storage are not.
fn is_transactable(stmt: &Stmt) -> bool {
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

/// For `SHOW DATABASES` returned to a non-global-admin caller, drop entries
/// they have no grants on.  Pass-through for every other statement / caller.
fn filter_show_databases(out: Output, stmt: &Stmt, user: &User) -> Output {
    if matches!(stmt, Stmt::ShowDatabases)
        && !user.is_global_admin()
        && let Output::Names(names) = out
    {
        return Output::Names(
            names
                .into_iter()
                .filter(|n| !user.effective(n).is_empty())
                .collect(),
        );
    }
    out
}

#[cfg(test)]
#[path = "executor_tests.rs"]
mod tests;
