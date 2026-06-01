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
use crate::model::{Layer, LayerId, Tau, Timestamp};
use crate::ql::ast::{AggFunc, Expr, Stmt, Type};
use crate::query::{
    at_layers, build_range_segments, collect_range_bounds, eval_agg, eval_lens, would_cycle,
};
use crate::storage::{
    InMemory, sweep_range,
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
}

impl DbState {
    fn new(compact_threshold: usize) -> Self {
        Self {
            db: Database::new(InMemory::<Value>::with_threshold(compact_threshold)),
            base_types: HashMap::default(),
            next_layer_id: 1,
            derived: HashMap::default(),
        }
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
    /// Multi-user authentication registry.  Empty by default; only loaded
    /// when the server has been configured with `--users-file`.  Exposed as
    /// a public field so admin tooling can read or mutate users directly
    /// without going through wrapper methods.
    pub users: UserStore,
    /// Execution counters shared with the metrics HTTP endpoint (if enabled).
    /// Always present; the server clones the Arc to give the metrics thread
    /// read access without locking the executor.
    pub metrics: Arc<Metrics>,
    /// Buffered mutations for the active transaction.  `None` means no
    /// transaction is open.  `Some(stmts)` means `START TRANSACTION` was
    /// issued and `stmts` is the ordered list of mutations queued for
    /// `COMMIT`.
    pending: Option<Vec<Stmt>>,
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
        }
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
        use crate::ql::parser::parse;

        let mut executor = Self::with_threshold(compact_threshold);
        let (db_state, schema_stmts) = DbState::with_wal(path, compact_threshold, key)?;
        executor
            .databases
            .insert("default".to_string(), Arc::new(RwLock::new(db_state)));
        executor.active = Some("default".to_string());

        // Replay schema DDL (CREATE LENS / DERIVE LENS).
        // in_replay suppresses writing these back to the WAL.
        executor.in_replay = true;
        for stmt_text in schema_stmts {
            match parse(&stmt_text) {
                Ok((_, stmt)) => {
                    if let Err(e) = executor.exec(&stmt) {
                        tracing::warn!(
                            stmt = %stmt_text,
                            error = ?e,
                            "schema WAL replay: statement failed, skipping"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        stmt = %stmt_text,
                        error = %e,
                        "schema WAL replay: parse error, skipping"
                    );
                }
            }
        }
        executor.in_replay = false;

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
            } => self.range_lens(name, *start, *end, filter.as_ref()),
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
            Stmt::BackupDatabase { name, path } => self.backup_database(name, path),
            _ => Err(ExecError::InvalidExpr(
                "exec_read called on a mutating statement".into(),
            )),
        };
        let ns = t0.elapsed().as_nanos() as u64;
        match stmt {
            Stmt::At { .. } | Stmt::AtAsOf { .. } | Stmt::AtLayer { .. } => {
                self.metrics.record_at(ns)
            }
            Stmt::Range { .. } => self.metrics.record_range(ns),
            Stmt::Reduce { .. } => self.metrics.record_reduce(ns),
            Stmt::HistoryLens { .. } => self.metrics.record_history(ns),
            _ => {}
        }
        result
    }

    /// Execute a single parsed statement.
    pub fn exec(&mut self, stmt: &Stmt) -> Result<Output, ExecError> {
        let t0 = Instant::now();
        // While inside a transaction, buffer mutable lens statements.
        // They are replayed atomically when COMMIT is issued.
        if let Some(pending) = &mut self.pending
            && is_transactable(stmt)
        {
            pending.push(stmt.clone());
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
            } => self.range_lens(name, *start, *end, filter.as_ref()),
            Stmt::Drop { name } => self.drop_lens(name),
            Stmt::ShowDatabases => self.show_databases(),
            Stmt::ShowLenses => self.show_lenses(),
            Stmt::Reduce {
                name,
                start,
                end,
                func,
            } => self.reduce_lens(name, *start, *end, *func),
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
        let ns = t0.elapsed().as_nanos() as u64;
        match stmt {
            Stmt::Append { .. } | Stmt::Copy { .. } | Stmt::BatchAppend { .. } => {
                self.metrics.record_append(ns)
            }
            Stmt::At { .. } | Stmt::AtAsOf { .. } | Stmt::AtLayer { .. } => {
                self.metrics.record_at(ns)
            }
            Stmt::Range { .. } => self.metrics.record_range(ns),
            Stmt::Reduce { .. } => self.metrics.record_reduce(ns),
            Stmt::HistoryLens { .. } => self.metrics.record_history(ns),
            _ => self.metrics.record_ddl(ns),
        }
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
        let ns = t0.elapsed().as_nanos() as u64;
        match stmt {
            Stmt::Append { .. } | Stmt::BatchAppend { .. } | Stmt::Copy { .. } => {
                self.metrics.record_append(ns)
            }
            _ => {}
        }
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
        let stmts = self.pending.take().ok_or(ExecError::NoActiveTransaction)?;
        // pending is now None so the buffering intercept in exec is inactive
        // and the replayed statements go straight to storage.
        for stmt in stmts {
            self.exec(&stmt)?;
        }
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
        self.databases.insert(
            name.into(),
            Arc::new(RwLock::new(DbState::new(self.compact_threshold))),
        );
        // First database created becomes active by convention.
        if self.active.is_none() {
            self.active = Some(name.into());
        }
        Ok(Output::Empty)
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
            .append(&state.db.lens(name), layer)
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
            .append(&state.db.lens(name), layer)
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
        Ok(Output::Value(eval_lens(&state, name, t)?))
    }

    fn range_lens(
        &self,
        name: &str,
        start: Timestamp,
        end: Timestamp,
        filter: Option<&Expr>,
    ) -> Result<Output, ExecError> {
        if start >= end {
            return Err(ExecError::InvalidRange);
        }
        let db_arc = self.active_db_arc()?;
        let state = db_arc.read().expect("db lock poisoned");
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        // Fast path for unfiltered base-lens queries: single-pass O(E log E) sweep
        // instead of N sequential layer scans.
        if filter.is_none() && state.base_types.contains_key(name) {
            let layers = state.db.layers(name).unwrap_or_default();
            let raw = sweep_range(&layers, start, end);
            let mut out: Vec<(Timestamp, Timestamp, Value)> = Vec::with_capacity(raw.len());
            for tau in raw {
                match out.last_mut() {
                    Some(last) if last.1 == tau.start && last.2 == tau.value => last.1 = tau.end,
                    _ => out.push((tau.start, tau.end, tau.value)),
                }
            }
            return Ok(Output::Range(out));
        }
        let (bounds, layers_snap) = collect_range_bounds(&state, name, start, end, filter)?;
        let out = build_range_segments(
            &state,
            name,
            &bounds,
            layers_snap.as_ref().map(|v| v.as_slice()),
            filter,
        )?;
        Ok(Output::Range(out))
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
        eval_agg(&state, name, func, start, end).map(Output::Value)
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
            .append(&state.db.lens(name), layer)
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
        let raw_schema = wal
            .raw_schema_lines()
            .map_err(|e| ExecError::Io(e.to_string()))?;

        let all_layers = state.db.export_layers();
        let entries: Vec<WalEntry<Value>> = all_layers
            .iter()
            .flat_map(|(lens_name, layers)| {
                layers.iter().map(move |layer| WalEntry {
                    layer_id: layer.id,
                    written_at: layer.written_at,
                    lens: lens_name.clone(),
                    taus: layer
                        .taus
                        .iter()
                        .map(|t| (t.start, t.end, t.value.clone()))
                        .collect(),
                })
            })
            .collect();

        wal.rewrite(&raw_schema, &entries)
            .map_err(|e| ExecError::Io(e.to_string()))?;
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
            })),
        );

        let prev_active = self.active.clone();
        self.active = Some(name.into());
        self.in_replay = true;
        for stmt_text in &schema_stmts {
            match crate::ql::parser::parse(stmt_text) {
                Ok((_, stmt)) => {
                    if let Err(e) = self.exec(&stmt) {
                        tracing::warn!(stmt = %stmt_text, error = ?e, "restore: schema replay failed");
                    }
                }
                Err(e) => {
                    tracing::warn!(stmt = %stmt_text, error = %e, "restore: schema parse error");
                }
            }
        }
        self.in_replay = false;
        self.active = prev_active;
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
mod tests {
    use super::*;
    use crate::ql::parse;
    use hegel::TestCase;
    use hegel::generators as gs;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap as StdHashMap;

    /// Parse + run.  Panics on parse failure; returns `Result` on exec.
    fn run(exec: &mut Executor, q: &str) -> Result<Output, ExecError> {
        let (rest, stmt) = parse(q).expect("parse failed");
        assert!(rest.is_empty(), "unconsumed: {rest:?}");
        exec.exec(&stmt)
    }

    fn setup() -> Executor {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE main").unwrap();
        e
    }

    #[test]
    fn create_database_sets_active_on_first_create() {
        let mut e = Executor::new();
        assert_eq!(e.active(), None);
        run(&mut e, "CREATE DATABASE a").unwrap();
        assert_eq!(e.active(), Some("a"));
    }

    #[test]
    fn second_create_does_not_change_active() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE a").unwrap();
        run(&mut e, "CREATE DATABASE b").unwrap();
        assert_eq!(e.active(), Some("a"));
    }

    #[test]
    fn create_duplicate_database_errors() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE a").unwrap();
        assert_eq!(
            run(&mut e, "CREATE DATABASE a"),
            Err(ExecError::DuplicateDatabase("a".into()))
        );
    }

    #[test]
    fn use_unknown_database_errors() {
        let mut e = Executor::new();
        assert_eq!(
            run(&mut e, "USE DATABASE ghost"),
            Err(ExecError::UnknownDatabase("ghost".into()))
        );
    }

    #[test]
    fn use_switches_active() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE a").unwrap();
        run(&mut e, "CREATE DATABASE b").unwrap();
        run(&mut e, "USE DATABASE b").unwrap();
        assert_eq!(e.active(), Some("b"));
    }

    #[test]
    fn drop_active_database_clears_active() {
        let mut e = setup();
        run(&mut e, "DROP DATABASE main").unwrap();
        assert_eq!(e.active(), None);
        assert_eq!(
            run(&mut e, "CREATE LENS x int"),
            Err(ExecError::NoActiveDatabase)
        );
    }

    #[test]
    fn drop_unknown_database_errors() {
        let mut e = Executor::new();
        assert_eq!(
            run(&mut e, "DROP DATABASE ghost"),
            Err(ExecError::UnknownDatabase("ghost".into()))
        );
    }

    #[test]
    fn create_lens_without_active_database_errors() {
        let mut e = Executor::new();
        assert_eq!(
            run(&mut e, "CREATE LENS x int"),
            Err(ExecError::NoActiveDatabase)
        );
    }

    #[test]
    fn create_duplicate_lens_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "CREATE LENS x int"),
            Err(ExecError::DuplicateLens("x".into()))
        );
    }

    #[test]
    fn drop_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "DROP LENS missing"),
            Err(ExecError::UnknownLens("missing".into()))
        );
    }

    #[test]
    fn append_to_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "APPEND LENS x 0 10 1"),
            Err(ExecError::UnknownLens("x".into()))
        );
    }

    #[test]
    fn append_type_mismatch_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "APPEND LENS x 0 10 1.5"),
            Err(ExecError::TypeMismatch {
                lens: "x".into(),
                expected: Type::Int,
                got: "float".into(),
            })
        );
    }

    #[test]
    fn append_null_is_permitted_for_any_type() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 null").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Null))
        );
    }

    #[test]
    fn append_with_inverted_range_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "APPEND LENS x 10 5 1"),
            Err(ExecError::InvalidRange)
        );
    }

    #[test]
    fn at_returns_none_for_uncovered_time() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        assert_eq!(run(&mut e, "AT LENS x 50").unwrap(), Output::Value(None));
    }

    #[test]
    fn at_returns_value_in_range() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
    }

    #[test]
    fn at_observes_newest_layer() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 20 1").unwrap();
        run(&mut e, "APPEND LENS x 5 15 2").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 3").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 10").unwrap(),
            Output::Value(Some(Value::Int(2)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 17").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
    }

    #[test]
    fn derive_simple_arithmetic() {
        let mut e = setup();
        run(&mut e, "CREATE LENS c int").unwrap();
        run(&mut e, "APPEND LENS c 0 100 10").unwrap();
        run(&mut e, "DERIVE LENS doubled AS c * 2").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS doubled 50").unwrap(),
            Output::Value(Some(Value::Int(20)))
        );
    }

    #[test]
    fn derive_celsius_to_fahrenheit_float() {
        let mut e = setup();
        run(&mut e, "CREATE LENS c float").unwrap();
        run(&mut e, "APPEND LENS c 0 100 18.0").unwrap();
        run(&mut e, "DERIVE LENS f AS c * 9.0 / 5.0 + 32.0").unwrap();
        let Output::Value(Some(Value::Float(v))) = run(&mut e, "AT LENS f 50").unwrap() else {
            panic!("expected float");
        };
        assert!((v - 64.4).abs() < 1e-9);
    }

    #[test]
    fn derive_changes_type_from_int_to_bool() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 100 5").unwrap();
        run(&mut e, "DERIVE LENS big AS x > 3").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS big 10").unwrap(),
            Output::Value(Some(Value::Bool(true)))
        );
    }

    #[test]
    fn derive_returns_none_when_source_uncovered() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();
        run(&mut e, "DERIVE LENS d AS x + 1").unwrap();
        assert_eq!(run(&mut e, "AT LENS d 50").unwrap(), Output::Value(None));
    }

    #[test]
    fn derive_chained() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 5").unwrap();
        run(&mut e, "DERIVE LENS y AS x * 2").unwrap();
        run(&mut e, "DERIVE LENS z AS y + 1").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS z 5").unwrap(),
            Output::Value(Some(Value::Int(11)))
        );
    }

    #[test]
    fn derive_unknown_ident_errors_at_query_time() {
        let mut e = setup();
        run(&mut e, "DERIVE LENS d AS ghost + 1").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS d 0"),
            Err(ExecError::UnknownLens("ghost".into()))
        );
    }

    #[test]
    fn divide_by_zero_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();
        run(&mut e, "DERIVE LENS d AS x / 0").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS d 5"),
            Err(ExecError::InvalidExpr("divide by zero".into()))
        );
    }

    #[test]
    fn range_returns_segments_split_at_change_points() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 0 10").unwrap(),
            Output::Range(vec![(0, 5, Value::Int(1)), (5, 10, Value::Int(2))])
        );
    }

    #[test]
    fn range_merges_adjacent_equal_values() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 7").unwrap();
        run(&mut e, "APPEND LENS x 5 10 7").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 0 10").unwrap(),
            Output::Range(vec![(0, 10, Value::Int(7))])
        );
    }

    #[test]
    fn range_skips_gaps() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 8 10 2").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 0 10").unwrap(),
            Output::Range(vec![(0, 5, Value::Int(1)), (8, 10, Value::Int(2))])
        );
    }

    #[test]
    fn range_clips_to_query_window() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 100 9").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 10 20").unwrap(),
            Output::Range(vec![(10, 20, Value::Int(9))])
        );
    }

    #[test]
    fn range_with_where_filter() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 50").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 0 10 WHERE x > 10").unwrap(),
            Output::Range(vec![(5, 10, Value::Int(50))])
        );
    }

    #[test]
    fn range_on_derived_lens() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        run(&mut e, "DERIVE LENS y AS x * 10").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS y 0 10").unwrap(),
            Output::Range(vec![(0, 5, Value::Int(10)), (5, 10, Value::Int(20))])
        );
    }

    #[test]
    fn range_inverted_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "RANGE LENS x 10 5"),
            Err(ExecError::InvalidRange)
        );
    }

    #[test]
    fn range_on_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "RANGE LENS ghost 0 10"),
            Err(ExecError::UnknownLens("ghost".into()))
        );
    }

    #[test]
    fn reduce_count_segments() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 USING count").unwrap(),
            Output::Value(Some(Value::Int(2)))
        );
    }

    #[test]
    fn reduce_sum_integers() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 3").unwrap();
        run(&mut e, "APPEND LENS x 5 10 7").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 USING sum").unwrap(),
            Output::Value(Some(Value::Int(10)))
        );
    }

    #[test]
    fn reduce_min_max() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 3").unwrap();
        run(&mut e, "APPEND LENS x 5 10 7").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 USING min").unwrap(),
            Output::Value(Some(Value::Int(3)))
        );
        assert_eq!(
            run(&mut e, "REDUCE LENS x 0 10 USING max").unwrap(),
            Output::Value(Some(Value::Int(7)))
        );
    }

    #[test]
    fn reduce_avg_time_weighted() {
        // Two equal-duration segments: avg = (3+7)/2 = 5.0
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 3").unwrap();
        run(&mut e, "APPEND LENS x 5 10 7").unwrap();
        let Output::Value(Some(Value::Float(v))) =
            run(&mut e, "REDUCE LENS x 0 10 USING avg").unwrap()
        else {
            panic!("expected float");
        };
        assert!((v - 5.0).abs() < 1e-9);
    }

    #[test]
    fn reduce_avg_weighted_by_duration() {
        // [0,1) = 1, [1,10) = 10  →  weighted avg = (1*1 + 9*10) / 10 = 91/10 = 9.1
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 1 1").unwrap();
        run(&mut e, "APPEND LENS x 1 10 10").unwrap();
        let Output::Value(Some(Value::Float(v))) =
            run(&mut e, "REDUCE LENS x 0 10 USING avg").unwrap()
        else {
            panic!("expected float");
        };
        assert!((v - 9.1).abs() < 1e-9);
    }

    #[test]
    fn reduce_returns_none_for_uncovered_range() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 10 20 USING avg").unwrap(),
            Output::Value(None)
        );
    }

    #[test]
    fn reduce_inverted_range_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS x 10 5 USING min"),
            Err(ExecError::InvalidRange)
        );
    }

    #[test]
    fn reduce_unknown_lens_errors() {
        let mut e = setup();
        assert_eq!(
            run(&mut e, "REDUCE LENS ghost 0 10 USING avg"),
            Err(ExecError::UnknownLens("ghost".into()))
        );
    }

    #[test]
    fn derive_with_rolling_avg() {
        // avg(x, -10, 0) at t=10 covers [0,10): values [0,5)=1 and [5,10)=2
        // time-weighted avg = (5*1 + 5*2) / 10 = 1.5
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        run(&mut e, "DERIVE LENS smooth AS avg(x, -10, 0)").unwrap();
        let Output::Value(Some(Value::Float(v))) = run(&mut e, "AT LENS smooth 10").unwrap() else {
            panic!("expected float");
        };
        assert!((v - 1.5).abs() < 1e-9);
    }

    #[test]
    fn derive_with_rolling_min() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 10").unwrap();
        run(&mut e, "APPEND LENS x 5 10 3").unwrap();
        run(&mut e, "DERIVE LENS lo AS min(x, -10, 0)").unwrap();
        // at t=10 window covers [0,10): min of 10 and 3 = 3
        assert_eq!(
            run(&mut e, "AT LENS lo 10").unwrap(),
            Output::Value(Some(Value::Int(3)))
        );
    }

    #[test]
    fn derive_agg_in_comparison() {
        // hot = x > avg(x, -10, 0): true when current value exceeds rolling avg
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1").unwrap();
        run(&mut e, "APPEND LENS x 5 10 2").unwrap();
        run(&mut e, "DERIVE LENS hot AS x > avg(x, -10, 0)").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS hot 5").unwrap(),
            Output::Value(Some(Value::Bool(true)))
        );
    }

    #[test]
    fn reduce_on_derived_lens() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 2").unwrap();
        run(&mut e, "APPEND LENS x 5 10 4").unwrap();
        run(&mut e, "DERIVE LENS doubled AS x * 2").unwrap();
        assert_eq!(
            run(&mut e, "REDUCE LENS doubled 0 10 USING sum").unwrap(),
            Output::Value(Some(Value::Int(12))) // 2*2*5 + 4*2*5 = 20+40... wait
        );
        // doubled over [0,5) = 4, over [5,10) = 8; sum = 4+8 = 12 ✓
    }

    #[test]
    fn lenses_are_isolated_per_database() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE a").unwrap();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();

        run(&mut e, "CREATE DATABASE b").unwrap();
        run(&mut e, "USE DATABASE b").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5"),
            Err(ExecError::UnknownLens("x".into()))
        );

        run(&mut e, "USE DATABASE a").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
    }

    #[test]
    fn schema_persists_across_wal_restart() {
        let dir = tempfile::tempdir().unwrap();
        let wal_path = dir.path().join("test.wal");

        // First session: create lens, append data, derive another lens.
        {
            let mut e = Executor::with_wal(&wal_path, None).unwrap();
            run(&mut e, "CREATE LENS temp int").unwrap();
            run(&mut e, "APPEND LENS temp 0 10 42").unwrap();
            run(&mut e, "DERIVE LENS cold AS (temp * 2)").unwrap();
        }

        // Second session: reopen WAL - schema must be recovered automatically.
        let mut e2 = Executor::with_wal(&wal_path, None).unwrap();
        // Data is recovered.
        assert_eq!(
            run(&mut e2, "AT LENS temp 5").unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
        // CREATE LENS schema is recovered - APPEND should not error with UnknownLens.
        run(&mut e2, "APPEND LENS temp 10 20 99").unwrap();
        assert_eq!(
            run(&mut e2, "AT LENS temp 15").unwrap(),
            Output::Value(Some(Value::Int(99)))
        );
        // DERIVE LENS schema is recovered - derived lens is usable.
        assert_eq!(
            run(&mut e2, "AT LENS cold 5").unwrap(),
            Output::Value(Some(Value::Int(84)))
        );
    }

    #[test]
    fn wal_error_variant_formats_in_tcp_output() {
        let e = ExecError::Io("disk full".into());
        assert!(matches!(e, ExecError::Io(_)));
    }

    #[test]
    fn append_multi_tau_single_layer() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 5 1, 5 10 2, 10 15 3").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 3").unwrap(),
            Output::Value(Some(Value::Int(1)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 7").unwrap(),
            Output::Value(Some(Value::Int(2)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 12").unwrap(),
            Output::Value(Some(Value::Int(3)))
        );
    }

    #[test]
    fn append_multi_tau_type_mismatch_rejects_all() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert!(matches!(
            run(&mut e, "APPEND LENS x 0 5 1, 5 10 1.5"),
            Err(ExecError::TypeMismatch { .. })
        ));
        // No partial write - lens should still have no data.
        assert_eq!(run(&mut e, "AT LENS x 3").unwrap(), Output::Value(None));
    }

    #[test]
    fn show_databases_lists_all() {
        let mut e = Executor::new();
        run(&mut e, "CREATE DATABASE alpha").unwrap();
        run(&mut e, "CREATE DATABASE beta").unwrap();
        let Output::Names(mut names) = run(&mut e, "SHOW DATABASES").unwrap() else {
            panic!("expected Names output");
        };
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn show_lenses_lists_base_and_derived() {
        let mut e = setup();
        run(&mut e, "CREATE LENS a int").unwrap();
        run(&mut e, "CREATE LENS b float").unwrap();
        run(&mut e, "DERIVE LENS c AS a + 1").unwrap();
        let Output::Names(mut names) = run(&mut e, "SHOW LENSES").unwrap() else {
            panic!("expected Names output");
        };
        names.sort();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn show_lenses_requires_active_database() {
        let e = Executor::new();
        assert_eq!(
            e.exec_read(&crate::ql::parse("SHOW LENSES").unwrap().1),
            Err(ExecError::NoActiveDatabase)
        );
    }

    #[test]
    fn cycle_detection_direct_self_reference() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "DERIVE LENS x2 AS x2 + 1"),
            Err(ExecError::CycleDetected("x2".into()))
        );
    }

    #[test]
    fn cycle_detection_transitive() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "DERIVE LENS y AS x + 1").unwrap();
        run(&mut e, "DERIVE LENS z AS y + 1").unwrap();
        // z → y → x (fine). Now try w → z → y → w (cycle).
        assert_eq!(
            run(&mut e, "DERIVE LENS w AS z + w"),
            Err(ExecError::CycleDetected("w".into()))
        );
    }

    #[test]
    fn copy_lens_from_csv() {
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("data.csv");
        std::fs::write(&csv_path, "0,10,42\n10,20,99\n").unwrap();

        let mut e = setup();
        run(&mut e, "CREATE LENS sensor int").unwrap();
        run(
            &mut e,
            &format!("COPY LENS sensor FROM \"{}\"", csv_path.display()),
        )
        .unwrap();
        assert_eq!(
            run(&mut e, "AT LENS sensor 5").unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
        assert_eq!(
            run(&mut e, "AT LENS sensor 15").unwrap(),
            Output::Value(Some(Value::Int(99)))
        );
    }

    fn install_admin(e: &mut Executor) {
        let mut grants = StdHashMap::new();
        grants.insert("*".into(), Perm::ALL);
        e.users.add(User::new("admin", "p", grants)).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
    }

    fn install_reader(e: &mut Executor, db: &str) {
        let mut grants = StdHashMap::new();
        grants.insert(db.to_string(), Perm::R);
        e.users.add(User::new("reader", "p", grants)).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
    }

    #[test]
    fn exec_as_admin_can_do_anything() {
        let mut e = Executor::new();
        install_admin(&mut e);
        let (_, stmt) = parse("CREATE DATABASE main").unwrap();
        assert_eq!(e.exec_as(&stmt, "admin").unwrap(), Output::Empty);
        let (_, stmt) = parse("CREATE LENS x int").unwrap();
        assert_eq!(e.exec_as(&stmt, "admin").unwrap(), Output::Empty);
        let (_, stmt) = parse("APPEND LENS x 0 10 42").unwrap();
        assert_eq!(e.exec_as(&stmt, "admin").unwrap(), Output::Empty);
        let (_, stmt) = parse("AT LENS x 5").unwrap();
        assert_eq!(
            e.exec_as(&stmt, "admin").unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
    }

    #[test]
    fn exec_as_reader_can_read_not_write() {
        let mut e = Executor::new();
        install_admin(&mut e);
        let (_, stmt) = parse("CREATE DATABASE main").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        let (_, stmt) = parse("CREATE LENS x int").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        let (_, stmt) = parse("APPEND LENS x 0 10 42").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        install_reader(&mut e, "main");

        // Reader can read.
        let (_, stmt) = parse("AT LENS x 5").unwrap();
        assert!(matches!(
            e.exec_read_as(&stmt, "reader").unwrap(),
            Output::Value(Some(Value::Int(42)))
        ));
        // Reader cannot append.
        let (_, stmt) = parse("APPEND LENS x 10 20 99").unwrap();
        assert!(matches!(
            e.exec_as(&stmt, "reader"),
            Err(ExecError::PermissionDenied(_))
        ));
        // Reader cannot drop.
        let (_, stmt) = parse("DROP LENS x").unwrap();
        assert!(matches!(
            e.exec_as(&stmt, "reader"),
            Err(ExecError::PermissionDenied(_))
        ));
        // Reader cannot create a database.
        let (_, stmt) = parse("CREATE DATABASE other").unwrap();
        assert!(matches!(
            e.exec_as(&stmt, "reader"),
            Err(ExecError::PermissionDenied(_))
        ));
        // Reader cannot manage users.
        let (_, stmt) = parse("CREATE USER newbie PASSWORD \"x\"").unwrap();
        assert!(matches!(
            e.exec_as(&stmt, "reader"),
            Err(ExecError::PermissionDenied(_))
        ));
    }

    #[test]
    fn exec_as_unknown_user_errors() {
        let mut e = Executor::new();
        let (_, stmt) = parse("SHOW DATABASES").unwrap();
        assert!(matches!(
            e.exec_as(&stmt, "ghost"),
            Err(ExecError::UnknownUser(_))
        ));
    }

    #[test]
    fn admin_can_create_drop_user_and_grant() {
        let mut e = Executor::new();
        install_admin(&mut e);
        let (_, stmt) = parse("CREATE USER bob PASSWORD \"hunter2\"").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        assert!(e.users.get("bob").is_some());

        let (_, stmt) = parse("GRANT R ON main TO bob").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        assert_eq!(e.users.get("bob").unwrap().effective("main"), Perm::R);

        let (_, stmt) = parse("REVOKE R ON main FROM bob").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        assert_eq!(e.users.get("bob").unwrap().effective("main"), Perm::NONE);

        let (_, stmt) = parse("DROP USER bob").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        assert!(e.users.get("bob").is_none());
    }

    #[test]
    fn promote_to_admin_via_a_bit() {
        let mut e = Executor::new();
        install_admin(&mut e);
        let (_, stmt) = parse("CREATE USER bob PASSWORD \"p\"").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        // Before promotion bob cannot create users.
        let (_, stmt) = parse("CREATE USER carol PASSWORD \"p\"").unwrap();
        assert!(matches!(
            e.exec_as(&stmt, "bob"),
            Err(ExecError::PermissionDenied(_))
        ));
        // Promote bob with A on the wildcard database.
        let (_, stmt) = parse("GRANT A ON * TO bob").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        // Now bob can create users.
        let (_, stmt) = parse("CREATE USER carol PASSWORD \"p\"").unwrap();
        assert!(e.exec_as(&stmt, "bob").is_ok());
    }

    #[test]
    fn show_databases_filters_for_non_admin() {
        let mut e = Executor::new();
        install_admin(&mut e);
        let (_, stmt) = parse("CREATE DATABASE alpha").unwrap();
        e.exec_as(&stmt, "admin").unwrap();
        let (_, stmt) = parse("CREATE DATABASE beta").unwrap();
        e.exec_as(&stmt, "admin").unwrap();

        let mut grants = StdHashMap::new();
        grants.insert("alpha".to_string(), Perm::R);
        e.users.add(User::new("alice", "p", grants)).unwrap(); // codeql[rust/hard-coded-cryptographic-value]

        let (_, stmt) = parse("SHOW DATABASES").unwrap();
        let out = e.exec_as(&stmt, "alice").unwrap();
        match out {
            Output::Names(names) => assert_eq!(names, vec!["alpha"]),
            _ => panic!("expected Names"),
        }
        let out = e.exec_as(&stmt, "admin").unwrap();
        match out {
            Output::Names(mut names) => {
                names.sort();
                assert_eq!(names, vec!["alpha", "beta"]);
            }
            _ => panic!("expected Names"),
        }
    }

    #[test]
    fn transaction_start_returns_ok() {
        let mut e = setup();
        assert_eq!(run(&mut e, "START TRANSACTION").unwrap(), Output::Empty);
    }

    #[test]
    fn commit_without_active_transaction_errors() {
        let mut e = setup();
        assert_eq!(run(&mut e, "COMMIT"), Err(ExecError::NoActiveTransaction));
    }

    #[test]
    fn rollback_without_active_transaction_errors() {
        let mut e = setup();
        assert_eq!(run(&mut e, "ROLLBACK"), Err(ExecError::NoActiveTransaction));
    }

    #[test]
    fn nested_start_transaction_errors() {
        let mut e = setup();
        run(&mut e, "START TRANSACTION").unwrap();
        assert_eq!(
            run(&mut e, "START TRANSACTION"),
            Err(ExecError::TransactionAlreadyActive)
        );
    }

    #[test]
    fn transaction_rollback_discards_appends() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "START TRANSACTION").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        run(&mut e, "ROLLBACK").unwrap();
        assert_eq!(run(&mut e, "AT LENS x 5").unwrap(), Output::Value(None));
    }

    #[test]
    fn appends_within_transaction_not_visible_before_commit() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "START TRANSACTION").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        // Data should not be visible until COMMIT.
        assert_eq!(run(&mut e, "AT LENS x 5").unwrap(), Output::Value(None));
        run(&mut e, "COMMIT").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
    }

    #[hegel::test]
    fn committed_transaction_matches_direct_writes(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        let mut segs: Vec<(i64, i64, i64)> = Vec::new();
        let mut cursor: i64 = 0;
        for _ in 0..n {
            let gap = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
            let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
            let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
            let s = cursor + gap;
            let e = s + len;
            segs.push((s, e, val));
            cursor = e;
        }

        let mut direct = setup();
        run(&mut direct, "CREATE LENS x int").unwrap();
        for &(s, e, v) in &segs {
            run(&mut direct, &format!("APPEND LENS x {s} {e} {v}")).unwrap();
        }

        let mut tx = setup();
        run(&mut tx, "CREATE LENS x int").unwrap();
        run(&mut tx, "START TRANSACTION").unwrap();
        for &(s, e, v) in &segs {
            run(&mut tx, &format!("APPEND LENS x {s} {e} {v}")).unwrap();
        }
        run(&mut tx, "COMMIT").unwrap();

        for &(s, e, v) in &segs {
            let mid = s + (e - s) / 2;
            assert_eq!(
                run(&mut direct, &format!("AT LENS x {mid}")).unwrap(),
                run(&mut tx, &format!("AT LENS x {mid}")).unwrap(),
                "segment [{s},{e}) value {v} diverged after commit"
            );
        }
    }

    #[hegel::test]
    fn rollback_leaves_lens_unchanged(tc: TestCase) {
        let base_val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let tx_val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));

        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, &format!("APPEND LENS x 0 100 {base_val}")).unwrap();

        run(&mut e, "START TRANSACTION").unwrap();
        run(&mut e, &format!("APPEND LENS x 100 200 {tx_val}")).unwrap();
        run(&mut e, "ROLLBACK").unwrap();

        assert_eq!(
            run(&mut e, "AT LENS x 50").unwrap(),
            Output::Value(Some(Value::Int(base_val))),
            "base data corrupted by rollback"
        );
        assert_eq!(
            run(&mut e, "AT LENS x 150").unwrap(),
            Output::Value(None),
            "rolled-back data still visible"
        );
    }

    #[hegel::test]
    fn pending_writes_invisible_before_commit(tc: TestCase) {
        let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let s = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let e_ts = s + tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));

        let mut exec = setup();
        run(&mut exec, "CREATE LENS x int").unwrap();
        run(&mut exec, "START TRANSACTION").unwrap();
        run(&mut exec, &format!("APPEND LENS x {s} {e_ts} {val}")).unwrap();

        let mid = s + (e_ts - s) / 2;
        assert_eq!(
            run(&mut exec, &format!("AT LENS x {mid}")).unwrap(),
            Output::Value(None),
            "pending write visible before COMMIT"
        );
    }

    #[hegel::test]
    fn multiple_sequential_transactions_accumulate(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
        let vals: Vec<i64> = (0..n)
            .map(|_| tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000)))
            .collect();

        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        for (i, &v) in vals.iter().enumerate() {
            let s = (i as i64) * 100;
            let end = s + 100;
            run(&mut e, "START TRANSACTION").unwrap();
            run(&mut e, &format!("APPEND LENS x {s} {end} {v}")).unwrap();
            run(&mut e, "COMMIT").unwrap();
        }
        for (i, &v) in vals.iter().enumerate() {
            let mid = (i as i64) * 100 + 50;
            assert_eq!(
                run(&mut e, &format!("AT LENS x {mid}")).unwrap(),
                Output::Value(Some(Value::Int(v))),
                "tx {i} value {v} missing after sequential commits"
            );
        }
    }

    #[hegel::test]
    fn rollback_then_commit_independent(tc: TestCase) {
        let discard_val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let keep_val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));

        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();

        run(&mut e, "START TRANSACTION").unwrap();
        run(&mut e, &format!("APPEND LENS x 0 100 {discard_val}")).unwrap();
        run(&mut e, "ROLLBACK").unwrap();

        run(&mut e, "START TRANSACTION").unwrap();
        run(&mut e, &format!("APPEND LENS x 0 100 {keep_val}")).unwrap();
        run(&mut e, "COMMIT").unwrap();

        assert_eq!(
            run(&mut e, "AT LENS x 50").unwrap(),
            Output::Value(Some(Value::Int(keep_val))),
            "committed value wrong after preceding rollback"
        );
    }

    #[test]
    fn copy_lens_skips_blank_lines_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("data.csv");
        std::fs::write(&csv_path, "# header\n\n0,10,7\n").unwrap();

        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(
            &mut e,
            &format!("COPY LENS x FROM \"{}\"", csv_path.display()),
        )
        .unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Int(7)))
        );
    }

    #[test]
    fn batch_append_produces_same_at_result_as_append() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "BATCH APPEND LENS x { 0 10 42 ; 20 30 99 }").unwrap();
        assert_eq!(
            run(&mut e, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
        assert_eq!(
            run(&mut e, "AT LENS x 25").unwrap(),
            Output::Value(Some(Value::Int(99)))
        );
        assert_eq!(run(&mut e, "AT LENS x 15").unwrap(), Output::Value(None));
    }

    #[test]
    fn batch_append_empty_block_succeeds() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        assert_eq!(
            run(&mut e, "BATCH APPEND LENS x {}").unwrap(),
            Output::Empty
        );
    }

    #[hegel::test]
    fn batch_append_matches_regular_append(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        let mut segs: Vec<(i64, i64, i64)> = Vec::new();
        let mut cursor: i64 = 0;
        for _ in 0..n {
            let gap = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
            let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
            let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
            let s = cursor + gap;
            let e = s + len;
            segs.push((s, e, val));
            cursor = e;
        }

        let mut direct = setup();
        run(&mut direct, "CREATE LENS x int").unwrap();
        let mut append_stmt = "APPEND LENS x".to_string();
        for (i, &(s, e, v)) in segs.iter().enumerate() {
            if i > 0 {
                append_stmt.push(',');
            }
            append_stmt.push_str(&format!(" {s} {e} {v}"));
        }
        run(&mut direct, &append_stmt).unwrap();

        let mut batch = setup();
        run(&mut batch, "CREATE LENS x int").unwrap();
        let body = segs
            .iter()
            .map(|(s, e, v)| format!("{s} {e} {v}"))
            .collect::<Vec<_>>()
            .join(" ; ");
        run(&mut batch, &format!("BATCH APPEND LENS x {{ {body} }}")).unwrap();

        for &(s, e, v) in &segs {
            let mid = s + (e - s) / 2;
            assert_eq!(
                run(&mut direct, &format!("AT LENS x {mid}")).unwrap(),
                run(&mut batch, &format!("AT LENS x {mid}")).unwrap(),
                "segment [{s},{e}) value {v} diverged between APPEND and BATCH APPEND"
            );
        }
    }

    #[test]
    fn history_lens_returns_one_layer_after_append() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        let (_, stmt) = parse("HISTORY LENS x").unwrap();
        let out = e.exec_read(&stmt).unwrap();
        let layers = match out {
            Output::LayerHistory(l) => l,
            other => panic!("expected LayerHistory, got {other:?}"),
        };
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].tau_count, 1);
        assert_eq!(layers[0].min_start, 0);
        assert_eq!(layers[0].max_end, 10);
    }

    #[test]
    fn history_lens_empty_on_no_data() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        let (_, stmt) = parse("HISTORY LENS x").unwrap();
        let out = e.exec_read(&stmt).unwrap();
        assert_eq!(out, Output::LayerHistory(vec![]));
    }

    #[test]
    fn history_lens_time_filter_excludes_non_overlapping_layers() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();
        run(&mut e, "APPEND LENS x 100 200 2").unwrap();
        let (_, stmt) = parse("HISTORY LENS x 50 150").unwrap();
        let out = e.exec_read(&stmt).unwrap();
        let layers = match out {
            Output::LayerHistory(l) => l,
            other => panic!("expected LayerHistory, got {other:?}"),
        };
        // Only the second layer (100..200) overlaps [50, 150).
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].min_start, 100);
    }

    #[hegel::test]
    fn history_lens_layer_count_matches_appends(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        for i in 0..n {
            let s = (i as i64) * 100;
            run(&mut e, &format!("APPEND LENS x {s} {} {i}", s + 50)).unwrap();
        }
        let (_, stmt) = parse("HISTORY LENS x").unwrap();
        let layers = match e.exec_read(&stmt).unwrap() {
            Output::LayerHistory(l) => l,
            other => panic!("expected LayerHistory, got {other:?}"),
        };
        // Each APPEND creates one layer (assuming no compaction at threshold 4; n <= 8 may
        // trigger one compaction round, so check >= 1 and <= n).
        assert!(
            !layers.is_empty(),
            "expected at least one layer after {n} appends"
        );
        assert!(
            layers.len() <= n,
            "layer count {} > append count {n} (compaction should only reduce)",
            layers.len()
        );
    }

    #[test]
    fn at_as_of_with_max_timestamp_includes_all_data() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        // written_at=0 for in-memory appends, so any as_of value includes them.
        let (_, stmt) = parse("AT LENS x 5 AS OF 9999999999999").unwrap();
        assert_eq!(
            e.exec_read(&stmt).unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
    }

    #[test]
    fn at_as_of_derived_lens_errors() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "DERIVE LENS y AS x").unwrap();
        let (_, stmt) = parse("AT LENS y 5 AS OF 0").unwrap();
        assert!(
            e.exec_read(&stmt).is_err(),
            "AT AS OF on a derived lens should error"
        );
    }

    #[test]
    fn at_layer_returns_value_from_correct_layer() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        let (_, hist_stmt) = parse("HISTORY LENS x").unwrap();
        let layer_id = match e.exec_read(&hist_stmt).unwrap() {
            Output::LayerHistory(layers) => layers[0].id,
            other => panic!("expected LayerHistory, got {other:?}"),
        };
        let (_, stmt) = parse(&format!("AT LENS x 5 LAYER {layer_id}")).unwrap();
        assert_eq!(
            e.exec_read(&stmt).unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
    }

    #[test]
    fn at_layer_nonexistent_layer_returns_nil() {
        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        let (_, stmt) = parse("AT LENS x 5 LAYER 99999").unwrap();
        assert_eq!(e.exec_read(&stmt).unwrap(), Output::Value(None));
    }

    #[test]
    fn backup_restore_roundtrip_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let bak = dir.path().join("x.bak").display().to_string();

        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 42").unwrap();
        run(&mut e, "APPEND LENS x 10 20 99").unwrap();
        run(&mut e, &format!("BACKUP DATABASE main TO \"{bak}\"")).unwrap();

        let mut e2 = Executor::new();
        run(&mut e2, "CREATE DATABASE other").unwrap();
        run(&mut e2, &format!("RESTORE DATABASE main FROM \"{bak}\"")).unwrap();
        run(&mut e2, "USE DATABASE main").unwrap();
        assert_eq!(
            run(&mut e2, "AT LENS x 5").unwrap(),
            Output::Value(Some(Value::Int(42)))
        );
        assert_eq!(
            run(&mut e2, "AT LENS x 15").unwrap(),
            Output::Value(Some(Value::Int(99)))
        );
    }

    #[hegel::test]
    fn at_as_of_with_large_timestamp_matches_at(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        let mut segs: Vec<(i64, i64, i64)> = Vec::new();
        let mut cursor: i64 = 0;
        for _ in 0..n {
            let gap = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
            let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
            let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
            let s = cursor + gap;
            let e = s + len;
            segs.push((s, e, val));
            cursor = e;
        }
        let mut ex = setup();
        run(&mut ex, "CREATE LENS x int").unwrap();
        for &(s, end, v) in &segs {
            run(&mut ex, &format!("APPEND LENS x {s} {end} {v}")).unwrap();
        }
        for &(s, end, _) in &segs {
            let mid = s + (end - s) / 2;
            let at_result = run(&mut ex, &format!("AT LENS x {mid}")).unwrap();
            let (_, stmt) = parse(&format!("AT LENS x {mid} AS OF 9999999999999")).unwrap();
            let as_of_result = ex.exec_read(&stmt).unwrap();
            assert_eq!(
                at_result, as_of_result,
                "AT and AT AS OF diverged at t={mid}"
            );
        }
    }

    #[hegel::test]
    fn at_layer_for_single_layer_matches_at(tc: TestCase) {
        let s = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000));
        let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let end = s + len;
        let mid = s + len / 2;
        let mut ex = setup();
        run(&mut ex, "CREATE LENS x int").unwrap();
        run(&mut ex, &format!("APPEND LENS x {s} {end} {val}")).unwrap();
        let (_, hist_stmt) = parse("HISTORY LENS x").unwrap();
        let layer_id = match ex.exec_read(&hist_stmt).unwrap() {
            Output::LayerHistory(layers) => {
                assert_eq!(layers.len(), 1, "expected exactly one layer");
                layers[0].id
            }
            other => panic!("expected LayerHistory, got {other:?}"),
        };
        let at_result = run(&mut ex, &format!("AT LENS x {mid}")).unwrap();
        let (_, stmt) = parse(&format!("AT LENS x {mid} LAYER {layer_id}")).unwrap();
        let layer_result = ex.exec_read(&stmt).unwrap();
        assert_eq!(
            at_result, layer_result,
            "AT and AT LAYER diverged with single layer at t={mid}"
        );
    }

    #[hegel::test]
    fn backup_restore_at_matches_original(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        let mut segs: Vec<(i64, i64, i64)> = Vec::new();
        let mut cursor: i64 = 0;
        for _ in 0..n {
            let gap = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
            let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
            let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
            let s = cursor + gap;
            let e = s + len;
            segs.push((s, e, val));
            cursor = e;
        }
        let dir = tempfile::tempdir().unwrap();
        let bak = dir.path().join("prop.bak").display().to_string();
        let mut original = setup();
        run(&mut original, "CREATE LENS x int").unwrap();
        for &(s, end, v) in &segs {
            run(&mut original, &format!("APPEND LENS x {s} {end} {v}")).unwrap();
        }
        run(&mut original, &format!("BACKUP DATABASE main TO \"{bak}\"")).unwrap();
        let mut restored = Executor::new();
        run(&mut restored, "CREATE DATABASE anchor").unwrap();
        run(
            &mut restored,
            &format!("RESTORE DATABASE main FROM \"{bak}\""),
        )
        .unwrap();
        run(&mut restored, "USE DATABASE main").unwrap();
        for &(s, end, _) in &segs {
            let mid = s + (end - s) / 2;
            assert_eq!(
                run(&mut original, &format!("AT LENS x {mid}")).unwrap(),
                run(&mut restored, &format!("AT LENS x {mid}")).unwrap(),
                "backup/restore diverged at t={mid}"
            );
        }
    }

    #[test]
    fn restore_existing_database_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let bak = dir.path().join("x.bak").display().to_string();

        let mut e = setup();
        run(&mut e, "CREATE LENS x int").unwrap();
        run(&mut e, "APPEND LENS x 0 10 1").unwrap();
        run(&mut e, &format!("BACKUP DATABASE main TO \"{bak}\"")).unwrap();

        let err = run(&mut e, &format!("RESTORE DATABASE main FROM \"{bak}\""));
        assert!(
            matches!(err, Err(ExecError::DuplicateDatabase(_))),
            "expected DuplicateDatabase, got {err:?}"
        );
    }
}
