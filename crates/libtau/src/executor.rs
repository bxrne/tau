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
    at_layers, build_range_segments, collect_range_bounds, eval_agg, eval_lens, ttl_cutoff,
    would_cycle,
};
use crate::storage::{
    Disk, InMemory, sweep_range,
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
    /// `0` for layers replayed from WAL files that predate the timestamp field.
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
    },
}

pub(crate) struct DbState {
    pub(crate) db: Database<Value>,
    /// Declared type of every base lens in this database.  Absence of an
    /// entry means the lens is either derived or unknown.
    pub(crate) base_types: HashMap<String, Type>,
    /// Monotonic layer-id source so `APPEND` doesn't need the caller to
    /// supply one.
    pub(crate) next_layer_id: u64,
    /// Derived lens definitions, stored by name.  Lookups recursively
    /// re-evaluate these - no caching.
    pub(crate) derived: HashMap<String, Expr>,
    /// Per-lens TTL in seconds.  A lens with an entry here hides data whose
    /// temporal interval ends before `(now - ttl_secs)` seconds ago.
    pub(crate) ttl_secs: HashMap<String, i64>,
}

impl DbState {
    fn new(compact_threshold: usize) -> Self {
        Self {
            db: Database::new(InMemory::<Value>::with_threshold(compact_threshold)),
            base_types: HashMap::default(),
            next_layer_id: 1,
            derived: HashMap::default(),
            ttl_secs: HashMap::default(),
        }
    }

    /// Open (or create) a disk-backed database, returning the fresh state plus
    /// any schema DDL persisted in the file so the executor can replay it.
    fn with_disk(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
    ) -> io::Result<(Self, Vec<String>)> {
        let path = path.as_ref();
        let mut store = if path.exists() {
            Disk::open(path, enc_key)?
        } else {
            Disk::create(path, enc_key)?
        };
        store.set_compact_threshold(compact_threshold);
        store.set_compression_level(compression_level);
        let db = Database::new(store);
        let next_layer_id = db.max_layer_id() + 1;
        let schema_stmts = db.schema_stmts().map_err(io::Error::other)?;
        Ok((
            Self {
                db,
                base_types: HashMap::default(),
                next_layer_id,
                derived: HashMap::default(),
                ttl_secs: HashMap::default(),
            },
            schema_stmts,
        ))
    }

    fn with_wal(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        key: Option<[u8; 32]>,
    ) -> io::Result<(Self, Vec<String>)> {
        let store = InMemory::<Value>::with_threshold(compact_threshold);
        let db = Database::open(store, path, key).map_err(io::Error::other)?;
        // Restore next_layer_id so new layers never collide with replayed ones.
        let next_layer_id = db.max_layer_id() + 1;
        // Fetch schema stmts to replay in the executor after construction.
        let schema_stmts = db.schema_stmts().map_err(io::Error::other)?;
        Ok((
            Self {
                db,
                base_types: HashMap::default(),
                next_layer_id,
                derived: HashMap::default(),
                ttl_secs: HashMap::default(),
            },
            schema_stmts,
        ))
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
    let iter = v.into_iter().skip(offset.unwrap_or(0));
    match limit {
        Some(n) => iter.take(n).collect(),
        None => iter.collect(),
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
        Stmt::Append { .. } | Stmt::BatchAppend { .. } => Op::Append,
        Stmt::Copy { .. } => Op::Copy,
        Stmt::At { .. } | Stmt::AtAsOf { .. } | Stmt::AtLayer { .. } => Op::At,
        Stmt::Range { .. } => Op::Range,
        Stmt::Reduce { .. } => Op::Reduce,
        Stmt::HistoryLens { .. } => Op::History,
        Stmt::Create { .. } | Stmt::Derive { .. } => Op::CreateLens,
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
    /// `<dir>/<name>.dat` compressed disk file. WAL config is ignored when
    /// this backend is active.
    pub fn with_disk_backend(
        dir: impl AsRef<Path>,
        compact_threshold: usize,
        compression_level: i32,
        enc_key: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            databases: HashMap::default(),
            active: None,
            compact_threshold,
            in_replay: false,
            users: UserStore::new(),
            metrics: Metrics::arc(),
            pending: None,
            backend: StorageBackend::Disk {
                dir,
                compression_level,
                enc_key,
            },
            started_at: std::time::Instant::now(),
        })
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
            Stmt::Create { name, ty } => self.create_lens(name, ty.clone()),
            Stmt::Append { name, taus } => {
                let taus: Vec<(Timestamp, Timestamp, Value)> =
                    taus.iter().map(|(s, e, v)| (*s, *e, v.into())).collect();
                self.append_lens(name, taus)
            }
            Stmt::Copy { name, path } => self.copy_lens(name, path),
            Stmt::Derive { name, expr } => self.derive_lens(name, expr.clone()),
            Stmt::At { name, t } => self.at_lens(name, *t),
            Stmt::Range {
                name,
                start,
                end,
                filter,
                limit,
                offset,
            } => self.range_lens(name, *start, *end, filter.as_ref(), *limit, *offset),
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
            Stmt::SetTtl { name, secs } => self.set_ttl(name, *secs),
            Stmt::UnsetTtl { name } => self.unset_ttl(name),
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
            Stmt::BatchAppend { name, taus } => self.batch_append_lens(name, taus),
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
            Stmt::Create { name, ty } => self.create_lens(name, ty.clone()),
            Stmt::Append { name, taus } => {
                let parsed: Vec<(Timestamp, Timestamp, Value)> =
                    taus.iter().map(|(s, e, l)| (*s, *e, l.into())).collect();
                self.append_lens(name, parsed)
            }
            Stmt::BatchAppend { name, taus } => self.batch_append_lens(name, taus),
            Stmt::Copy { name, path } => self.copy_lens(name, path),
            Stmt::Derive { name, expr } => self.derive_lens(name, expr.clone()),
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
            Stmt::Append { .. } | Stmt::Copy { .. } => require(require_active()?, Perm::U),
            Stmt::Derive { .. } => require(require_active()?, Perm::C),
            Stmt::At { .. } | Stmt::Range { .. } | Stmt::Reduce { .. } => {
                require(require_active()?, Perm::R)
            }
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
            } => {
                let path = dir.join(format!("{name}.dat"));
                DbState::with_disk(&path, self.compact_threshold, *compression_level, *enc_key)
                    .map_err(|e| ExecError::Io(e.to_string()))?
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

    fn create_lens(&self, name: &str, ty: Type) -> Result<Output, ExecError> {
        let in_replay = self.in_replay;
        let db_arc = self.active_db_arc()?;
        let mut state = db_arc.write().expect("db lock poisoned");
        if state.base_types.contains_key(name) || state.derived.contains_key(name) {
            return Err(ExecError::DuplicateLens(name.into()));
        }
        // WAL-first: persist before updating in-memory state.
        if !in_replay {
            let stmt_text = format!("CREATE LENS {name} {ty}");
            state
                .db
                .append_schema(&stmt_text)
                .map_err(|e| ExecError::Io(e.to_string()))?;
        }
        state.base_types.insert(name.into(), ty);
        Ok(Output::Empty)
    }

    fn append_lens(
        &self,
        name: &str,
        taus: Vec<(Timestamp, Timestamp, Value)>,
    ) -> Result<Output, ExecError> {
        if taus.is_empty() {
            return Ok(Output::Empty);
        }
        let db_arc = self.active_db_arc()?;
        let mut state = db_arc.write().expect("db lock poisoned");
        let ty = state
            .base_types
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownLens(name.into()))?;
        for (start, end, value) in &taus {
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
        }
        let id = state.next_layer_id;
        state.next_layer_id += 1;
        let layer = Layer::new(
            id,
            taus.into_iter()
                .map(|(s, e, v)| Tau::new(s, e, v))
                .collect(),
        );
        state
            .db
            .append(name, layer)
            .map_err(|e| ExecError::Io(e.to_string()))?;
        Ok(Output::Empty)
    }

    fn copy_lens(&self, name: &str, path: &str) -> Result<Output, ExecError> {
        use crate::ql::parse_literal;
        let content = fs::read_to_string(path).map_err(|e| ExecError::Io(e.to_string()))?;
        // Validate the lens exists and get its type before parsing all rows.
        {
            let db_arc = self.active_db_arc()?;
            let state = db_arc.read().expect("db lock poisoned");
            if !state.base_types.contains_key(name) {
                return Err(ExecError::UnknownLens(name.into()));
            }
        }
        let mut taus: Vec<(Timestamp, Timestamp, Value)> = Vec::new();
        for (lineno, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(3, ',');
            let start_str = parts
                .next()
                .ok_or_else(|| ExecError::Io(format!("line {}: missing start", lineno + 1)))?
                .trim();
            let end_str = parts
                .next()
                .ok_or_else(|| ExecError::Io(format!("line {}: missing end", lineno + 1)))?
                .trim();
            let val_str = parts
                .next()
                .ok_or_else(|| ExecError::Io(format!("line {}: missing value", lineno + 1)))?
                .trim();
            let start: Timestamp = start_str.parse().map_err(|_| {
                ExecError::Io(format!(
                    "line {}: invalid start {:?}",
                    lineno + 1,
                    start_str
                ))
            })?;
            let end: Timestamp = end_str.parse().map_err(|_| {
                ExecError::Io(format!("line {}: invalid end {:?}", lineno + 1, end_str))
            })?;
            let value: Value = parse_literal(val_str)
                .map(|lit| Value::from(&lit))
                .ok_or_else(|| {
                    ExecError::Io(format!(
                        "line {}: cannot parse value {:?}",
                        lineno + 1,
                        val_str
                    ))
                })?;
            taus.push((start, end, value));
        }
        if taus.is_empty() {
            return Ok(Output::Empty);
        }
        let db_arc = self.active_db_arc()?;
        let mut state = db_arc.write().expect("db lock poisoned");
        let ty = state
            .base_types
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownLens(name.into()))?;
        let mut tau_vec: Vec<crate::model::Tau<Value>> = Vec::with_capacity(taus.len());
        for (start, end, value) in taus {
            if let Some(got) = value.ty()
                && got != ty
            {
                return Err(ExecError::TypeMismatch {
                    lens: name.into(),
                    expected: ty.clone(),
                    got: value.type_name().into(),
                });
            }
            tau_vec.push(crate::model::Tau::new(start, end, value));
        }
        let id = state.next_layer_id;
        state.next_layer_id += 1;
        let now = crate::model::now_ms();
        let layer = Layer::new_sorted_unchecked(id, tau_vec, now);
        state
            .db
            .append(name, layer)
            .map_err(|e| ExecError::Io(e.to_string()))?;
        Ok(Output::Empty)
    }

    fn derive_lens(&self, name: &str, expr: Expr) -> Result<Output, ExecError> {
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
        if !in_replay {
            let stmt_text = format!("DERIVE LENS {name} AS {expr}");
            state
                .db
                .append_schema(&stmt_text)
                .map_err(|e| ExecError::Io(e.to_string()))?;
        }
        state.derived.insert(name.into(), expr);
        Ok(Output::Empty)
    }

    fn at_lens(&self, name: &str, t: Timestamp) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
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
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        let effective_start = ttl_cutoff(&state, name).map_or(start, |c| start.max(c));
        if effective_start >= end {
            return Ok(Output::Range(vec![]));
        }
        // Fast path for unfiltered base-lens queries: single-pass O(E log E) sweep.
        if filter.is_none() && state.base_types.contains_key(name) {
            let layers = state.db.layers(name).unwrap_or_default();
            let raw = sweep_range(&layers, effective_start, end);
            let mut out: Vec<(Timestamp, Timestamp, Value)> = Vec::with_capacity(raw.len());
            for tau in raw {
                match out.last_mut() {
                    Some(last) if last.1 == tau.start && last.2 == tau.value => last.1 = tau.end,
                    _ => out.push((tau.start, tau.end, tau.value)),
                }
            }
            let out = apply_offset_limit(out, offset, limit);
            return Ok(Output::Range(out));
        }
        let (bounds, layers_snap) =
            collect_range_bounds(&state, name, effective_start, end, filter)?;
        let out = build_range_segments(
            &state,
            name,
            &bounds,
            layers_snap.as_ref().map(|v| v.as_slice()),
            filter,
        )?;
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
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        let effective_start = ttl_cutoff(&state, name).map_or(start, |c| start.max(c));
        if effective_start >= end {
            return Ok(Output::Value(None));
        }
        eval_agg(&state, name, func, effective_start, end).map(Output::Value)
    }

    fn set_ttl(&mut self, name: &str, secs: i64) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let mut state = db_arc.write().expect("db lock poisoned");
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        state.ttl_secs.insert(name.into(), secs);
        if !self.in_replay {
            state
                .db
                .append_schema(&format!("SET TTL LENS {name} {secs}"))
                .map_err(|e| ExecError::Io(e.to_string()))?;
        }
        Ok(Output::Empty)
    }

    fn unset_ttl(&mut self, name: &str) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let mut state = db_arc.write().expect("db lock poisoned");
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        state.ttl_secs.remove(name);
        if !self.in_replay {
            state
                .db
                .append_schema(&format!("UNSET TTL LENS {name}"))
                .map_err(|e| ExecError::Io(e.to_string()))?;
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
        if in_types || in_derived {
            state.db.drop_lens(name);
            if !in_replay {
                state
                    .db
                    .append_schema(&format!("DROP LENS {name}"))
                    .map_err(|e| ExecError::Io(e.to_string()))?;
            }
            Ok(Output::Empty)
        } else {
            Err(ExecError::UnknownLens(name.into()))
        }
    }

    fn batch_append_lens(
        &self,
        name: &str,
        taus: &[(i64, i64, crate::ql::ast::Literal)],
    ) -> Result<Output, ExecError> {
        if taus.is_empty() {
            return Ok(Output::Empty);
        }
        let db_arc = self.active_db_arc()?;
        let mut state = db_arc.write().expect("db lock poisoned");
        let ty = state
            .base_types
            .get(name)
            .cloned()
            .ok_or_else(|| ExecError::UnknownLens(name.into()))?;
        let mut tau_vec: Vec<crate::model::Tau<Value>> = Vec::with_capacity(taus.len());
        for (start, end, lit) in taus {
            if start >= end {
                return Err(ExecError::InvalidRange);
            }
            let value: Value = lit.into();
            if let Some(got) = value.ty()
                && got != ty
            {
                return Err(ExecError::TypeMismatch {
                    lens: name.into(),
                    expected: ty.clone(),
                    got: value.type_name().into(),
                });
            }
            tau_vec.push(crate::model::Tau::new(*start, *end, value));
        }
        let id = state.next_layer_id;
        state.next_layer_id += 1;
        let now = crate::model::now_ms();
        let layer = Layer::new_sorted_unchecked(id, tau_vec, now);
        state
            .db
            .append(name, layer)
            .map_err(|e| ExecError::Io(e.to_string()))?;
        Ok(Output::Empty)
    }

    fn at_as_of_lens(&self, name: &str, t: Timestamp, as_of: i64) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        if state.base_types.contains_key(name) {
            let filtered: Vec<Layer<Value>> = state
                .db
                .layers(name)
                .map(|arc| {
                    arc.iter()
                        .filter(|l| l.written_at == 0 || l.written_at <= as_of)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            Ok(Output::Value(if filtered.is_empty() {
                None
            } else {
                at_layers(&filtered, t)
            }))
        } else if state.derived.contains_key(name) {
            Err(ExecError::InvalidExpr(
                "AT AS OF is only supported for base lenses".into(),
            ))
        } else {
            Err(ExecError::UnknownLens(name.into()))
        }
    }

    fn at_layer_lens(&self, name: &str, t: Timestamp, layer_id: u64) -> Result<Output, ExecError> {
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        if !state.base_types.contains_key(name) {
            return if state.derived.contains_key(name) {
                Err(ExecError::InvalidExpr(
                    "AT LAYER is only supported for base lenses".into(),
                ))
            } else {
                Err(ExecError::UnknownLens(name.into()))
            };
        }
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
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        let layers = state.db.layers(name).unwrap_or_default();
        let infos = layers
            .iter()
            .filter(|l| match range {
                Some((start, end)) => l.max_end > start && l.min_start < end,
                None => true,
            })
            .map(|l| LayerInfo {
                id: l.id,
                written_at: l.written_at,
                min_start: l.min_start,
                max_end: l.max_end,
                tau_count: l.taus.len(),
            })
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
            schema_stmts.push(format!("DERIVE LENS {lens_name} AS {expr}"));
        }

        let bk_path = Path::new(path);
        if bk_path.exists() {
            fs::remove_file(bk_path).map_err(|e| ExecError::Io(e.to_string()))?;
        }

        let mut wal = Wal::open(path, None).map_err(|e| ExecError::Io(e.to_string()))?;
        for stmt in &schema_stmts {
            wal.append_schema(stmt)
                .map_err(|e| ExecError::Io(e.to_string()))?;
        }
        let all_layers = state.db.export_layers();
        for (lens_name, layers) in &all_layers {
            for layer in layers {
                let entry = WalEntry {
                    layer_id: layer.id,
                    written_at: layer.written_at,
                    lens: lens_name.clone(),
                    taus: layer
                        .taus
                        .iter()
                        .map(|t| (t.start, t.end, t.value.clone()))
                        .collect(),
                };
                wal.append(&entry)
                    .map_err(|e| ExecError::Io(e.to_string()))?;
            }
        }
        wal.sync().map_err(|e| ExecError::Io(e.to_string()))?;
        Ok(Output::Empty)
    }

    fn restore_database(&mut self, name: &str, path: &str) -> Result<Output, ExecError> {
        if self.databases.contains_key(name) {
            return Err(ExecError::DuplicateDatabase(name.into()));
        }
        if !Path::new(path).exists() {
            return Err(ExecError::Io(format!("backup file not found: {path}")));
        }

        let wal = Wal::open(path, None).map_err(|e| ExecError::Io(e.to_string()))?;
        let mut store = InMemory::<Value>::with_threshold(self.compact_threshold);
        wal.replay(&mut store)
            .map_err(|e| ExecError::Io(e.to_string()))?;
        let schema_stmts = wal
            .replay_schemas()
            .map_err(|e| ExecError::Io(e.to_string()))?;

        let db = Database::new(store);
        let next_layer_id = db.max_layer_id() + 1;
        self.databases.insert(
            name.into(),
            Arc::new(RwLock::new(DbState {
                db,
                base_types: HashMap::default(),
                next_layer_id,
                derived: HashMap::default(),
                ttl_secs: HashMap::default(),
            })),
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

/// Returns `true` for the statement kinds that are deferred when a transaction
/// is active.  Only lens-scoped mutations are buffered; database management,
/// user management, and DDL that operates outside lens storage are not.
fn is_transactable(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Create { .. }
            | Stmt::Append { .. }
            | Stmt::BatchAppend { .. }
            | Stmt::Copy { .. }
            | Stmt::Derive { .. }
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
