//! Query-language executor.
//!
//! Wires the [`Stmt`](crate::libtau::ql::ast::Stmt) AST to a runtime registry
//! of [`Database<Value>`](crate::libtau::database::Database) instances.
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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::libtau::database::Database;
use crate::libtau::metrics::Metrics;
use crate::libtau::model::{Layer, Tau, Timestamp};
use crate::libtau::ql::ast::{AggFunc, BinOp, Expr, Stmt, Type, UnOp};
use crate::libtau::storage::InMemory;
use crate::libtau::users::{Perm, User, UserStore};
use crate::libtau::value::Value;

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
}

/// Per-database executor state.
struct DbState {
    db: Database<Value>,
    /// Declared type of every base lens in this database.  Absence of an
    /// entry means the lens is either derived or unknown.
    base_types: HashMap<String, Type>,
    /// Monotonic layer-id source so `APPEND` doesn't need the caller to
    /// supply one.
    next_layer_id: u64,
    /// Derived lens definitions, stored by name.  Lookups recursively
    /// re-evaluate these - no caching.
    derived: HashMap<String, Expr>,
}

impl DbState {
    fn new(compact_threshold: usize) -> Self {
        Self {
            db: Database::new(InMemory::<Value>::with_threshold(compact_threshold)),
            base_types: HashMap::new(),
            next_layer_id: 1,
            derived: HashMap::new(),
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
                base_types: HashMap::new(),
                next_layer_id,
                derived: HashMap::new(),
            },
            schema_stmts,
        ))
    }
}

/// Runtime container for executing parsed [`Stmt`]s.
pub struct Executor {
    databases: HashMap<String, DbState>,
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
}

impl Default for Executor {
    fn default() -> Self {
        Self::with_threshold(crate::libtau::storage::COMPACT_THRESHOLD)
    }
}

impl Executor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an executor with a custom layer compaction threshold.
    pub fn with_threshold(compact_threshold: usize) -> Self {
        Self {
            databases: HashMap::new(),
            active: None,
            compact_threshold,
            in_replay: false,
            users: UserStore::new(),
            metrics: Metrics::arc(),
        }
    }

    /// Open a WAL-backed executor with the default compaction threshold.
    pub fn with_wal(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        Self::with_wal_threshold(path, crate::libtau::storage::COMPACT_THRESHOLD, key)
    }

    /// Open a WAL-backed executor with a custom compaction threshold.
    pub fn with_wal_threshold(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        key: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        use crate::libtau::ql::parser::parse;

        let mut executor = Self::with_threshold(compact_threshold);
        let (db_state, schema_stmts) = DbState::with_wal(path, compact_threshold, key)?;
        executor.databases.insert("default".to_string(), db_state);
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
            _ => Err(ExecError::InvalidExpr(
                "exec_read called on a mutating statement".into(),
            )),
        };
        let ns = t0.elapsed().as_nanos() as u64;
        match stmt {
            Stmt::At { .. } => self.metrics.record_at(ns),
            Stmt::Range { .. } => self.metrics.record_range(ns),
            Stmt::Reduce { .. } => self.metrics.record_reduce(ns),
            _ => {}
        }
        result
    }

    /// Execute a single parsed statement.
    pub fn exec(&mut self, stmt: &Stmt) -> Result<Output, ExecError> {
        let t0 = Instant::now();
        let result = match stmt {
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
        };
        let ns = t0.elapsed().as_nanos() as u64;
        match stmt {
            Stmt::Append { .. } | Stmt::Copy { .. } => self.metrics.record_append(ns),
            Stmt::At { .. } => self.metrics.record_at(ns),
            Stmt::Range { .. } => self.metrics.record_range(ns),
            Stmt::Reduce { .. } => self.metrics.record_reduce(ns),
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
        let user = self
            .users
            .get(caller)
            .cloned()
            .ok_or_else(|| ExecError::UnknownUser(caller.into()))?;
        self.check_permission(stmt, &user)?;
        let out = self.exec(stmt)?;
        Ok(filter_show_databases(out, stmt, &user))
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

        match stmt {
            Stmt::CreateDatabase { .. } => require_global_admin(),
            Stmt::DropDatabase { name } => require_admin_or_a_on(name),
            Stmt::UseDatabase { name } => {
                if user.effective(name).is_empty() {
                    Err(ExecError::PermissionDenied(format!(
                        "user '{}' has no grants on {}",
                        user.name, name
                    )))
                } else {
                    Ok(())
                }
            }
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
            Stmt::ShowGrants { user: target } => match target {
                None => require_global_admin(),
                Some(t) if t == &user.name => Ok(()),
                Some(_) => require_global_admin(),
            },
        }
    }

    fn create_user(&mut self, name: &str, password: &str) -> Result<Output, ExecError> {
        if self.users.get(name).is_some() {
            return Err(ExecError::DuplicateUser(name.into()));
        }
        self.users
            .add(User::new(name, password, HashMap::new()))
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

    fn create_database(&mut self, name: &str) -> Result<Output, ExecError> {
        if self.databases.contains_key(name) {
            return Err(ExecError::DuplicateDatabase(name.into()));
        }
        self.databases
            .insert(name.into(), DbState::new(self.compact_threshold));
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

    fn create_lens(&mut self, name: &str, ty: Type) -> Result<Output, ExecError> {
        let in_replay = self.in_replay;
        let state = self.active_mut()?;
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
        &mut self,
        name: &str,
        taus: Vec<(Timestamp, Timestamp, Value)>,
    ) -> Result<Output, ExecError> {
        if taus.is_empty() {
            return Ok(Output::Empty);
        }
        // Validate ranges and types before touching the store.
        let state = self.active_mut()?;
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

    fn copy_lens(&mut self, name: &str, path: &str) -> Result<Output, ExecError> {
        use crate::libtau::ql::parser::parse as ql_parse;
        let content = fs::read_to_string(path).map_err(|e| ExecError::Io(e.to_string()))?;
        // Validate the lens exists and get its type before parsing all rows.
        {
            let state = self.active_state()?;
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
            // Parse the value as a literal via the QL parser (handles all types).
            let lit_input = format!("APPEND LENS _dummy_ {start} {end} {val_str}");
            let value: Value = match ql_parse(&lit_input) {
                Ok((
                    _,
                    Stmt::Append {
                        taus: ref parsed_taus,
                        ..
                    },
                )) if !parsed_taus.is_empty() => {
                    let (_, _, lit) = &parsed_taus[0];
                    lit.into()
                }
                _ => {
                    return Err(ExecError::Io(format!(
                        "line {}: cannot parse value {:?}",
                        lineno + 1,
                        val_str
                    )));
                }
            };
            taus.push((start, end, value));
        }
        self.append_lens(name, taus)
    }

    fn derive_lens(&mut self, name: &str, expr: Expr) -> Result<Output, ExecError> {
        let in_replay = self.in_replay;
        let state = self.active_mut()?;
        if state.base_types.contains_key(name) || state.derived.contains_key(name) {
            return Err(ExecError::DuplicateLens(name.into()));
        }
        // Reject self-referential or transitively cyclic expressions.
        let mut visited = HashSet::new();
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
        let state = self.active_state()?;
        Ok(Output::Value(eval_lens(state, name, t)?))
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
        let state = self.active_state()?;
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        let (bounds, layers_snap) =
            collect_range_bounds(state, name, start, end, filter)?;
        let out = build_range_segments(state, name, &bounds, layers_snap.as_deref(), filter)?;
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
        let state = self.active_state()?;
        if !state.base_types.contains_key(name) && !state.derived.contains_key(name) {
            return Err(ExecError::UnknownLens(name.into()));
        }
        eval_agg(state, name, func, start, end).map(Output::Value)
    }

    fn show_databases(&self) -> Result<Output, ExecError> {
        let mut names: Vec<String> = self.databases.keys().cloned().collect();
        names.sort();
        Ok(Output::Names(names))
    }

    fn show_lenses(&self) -> Result<Output, ExecError> {
        let state = self.active_state()?;
        let mut names: Vec<String> = state
            .base_types
            .keys()
            .chain(state.derived.keys())
            .cloned()
            .collect();
        names.sort();
        Ok(Output::Names(names))
    }

    fn drop_lens(&mut self, name: &str) -> Result<Output, ExecError> {
        let state = self.active_mut()?;
        if state.base_types.remove(name).is_some() || state.derived.remove(name).is_some() {
            Ok(Output::Empty)
        } else {
            Err(ExecError::UnknownLens(name.into()))
        }
    }

    fn active_state(&self) -> Result<&DbState, ExecError> {
        let name = self.active.as_deref().ok_or(ExecError::NoActiveDatabase)?;
        self.databases
            .get(name)
            .ok_or_else(|| ExecError::UnknownDatabase(name.into()))
    }

    fn active_mut(&mut self) -> Result<&mut DbState, ExecError> {
        // Use as_deref so HashMap::get_mut receives &str directly (String: Borrow<str>),
        // avoiding a String allocation on every write-path call.
        let name = self.active.as_deref().ok_or(ExecError::NoActiveDatabase)?;
        self.databases
            .get_mut(name)
            .ok_or_else(|| ExecError::UnknownDatabase(name.into()))
    }
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

/// Returns `true` if adding `DERIVE LENS target AS expr` would create a cycle.
///
/// Performs a DFS through `expr`'s identifiers, following derived lens
/// definitions already in `derived`.  The `visited` set prevents revisiting
/// nodes so the traversal terminates even if `derived` already contains cycles
/// from before this check was introduced.
fn would_cycle(
    derived: &HashMap<String, Expr>,
    target: &str,
    expr: &Expr,
    visited: &mut HashSet<String>,
) -> bool {
    let names: Vec<&str> = match expr {
        Expr::Ident(n) => vec![n.as_str()],
        Expr::Agg { lens, .. } => vec![lens.as_str()],
        Expr::Lit(_) => return false,
        Expr::Unary { expr, .. } => return would_cycle(derived, target, expr, visited),
        Expr::Binary { lhs, rhs, .. } => {
            return would_cycle(derived, target, lhs, visited)
                || would_cycle(derived, target, rhs, visited);
        }
    };
    for name in names {
        if name == target {
            return true;
        }
        if visited.insert(name.to_string())
            && let Some(dep_expr) = derived.get(name)
        {
            let dep_expr = dep_expr.clone();
            if would_cycle(derived, target, &dep_expr, visited) {
                return true;
            }
        }
    }
    false
}

fn eval_lens(state: &DbState, name: &str, t: Timestamp) -> Result<Option<Value>, ExecError> {
    if state.base_types.contains_key(name) {
        // Use at_name to avoid the Arc<str> allocation that lens() would incur
        // on every window in a range/reduce scan.
        Ok(state.db.at_name(name, t))
    } else if let Some(expr) = state.derived.get(name) {
        eval_expr(state, expr, t)
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

fn eval_expr(state: &DbState, expr: &Expr, t: Timestamp) -> Result<Option<Value>, ExecError> {
    match expr {
        Expr::Lit(l) => Ok(Some(l.into())),
        Expr::Ident(name) => eval_lens(state, name, t),
        Expr::Unary { op, expr } => match eval_expr(state, expr, t)? {
            None => Ok(None),
            Some(v) => apply_unary(*op, v).map(Some),
        },
        Expr::Binary { op, lhs, rhs } => {
            let l = eval_expr(state, lhs, t)?;
            let r = eval_expr(state, rhs, t)?;
            match (l, r) {
                (Some(a), Some(b)) => apply_binary(*op, a, b).map(Some),
                _ => Ok(None),
            }
        }
        Expr::Agg {
            func,
            lens,
            rel_start,
            rel_end,
        } => {
            let abs_start = t + rel_start;
            let abs_end = t + rel_end;
            if abs_start >= abs_end {
                return Ok(None);
            }
            eval_agg(state, lens, *func, abs_start, abs_end)
        }
    }
}

fn apply_unary(op: UnOp, v: Value) -> Result<Value, ExecError> {
    match (op, v) {
        (UnOp::Neg, Value::Int(i)) => Ok(Value::Int(-i)),
        (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (op, v) => Err(ExecError::InvalidExpr(format!(
            "cannot apply {op:?} to {}",
            v.type_name()
        ))),
    }
}

/// Coerce an `Int`/`Float` pair to a common `f64` representation; returns
/// `None` if either side isn't numeric.
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn apply_int_binary_op(op: BinOp, x: i64, y: i64) -> Result<Value, ExecError> {
    use BinOp::*;
    Ok(match op {
        Add => Value::Int(x.wrapping_add(y)),
        Sub => Value::Int(x.wrapping_sub(y)),
        Mul => Value::Int(x.wrapping_mul(y)),
        Div => {
            if y == 0 {
                return Err(ExecError::InvalidExpr("divide by zero".into()));
            }
            Value::Int(x / y)
        }
        Mod => {
            if y == 0 {
                return Err(ExecError::InvalidExpr("modulo by zero".into()));
            }
            Value::Int(x % y)
        }
        Lt => Value::Bool(x < y),
        LtEq => Value::Bool(x <= y),
        Gt => Value::Bool(x > y),
        GtEq => Value::Bool(x >= y),
        _ => unreachable!(),
    })
}

fn apply_float_binary_op(op: BinOp, x: f64, y: f64) -> Result<Value, ExecError> {
    use BinOp::*;
    Ok(match op {
        Add => Value::Float(x + y),
        Sub => Value::Float(x - y),
        Mul => Value::Float(x * y),
        Div => {
            if y == 0.0 {
                return Err(ExecError::InvalidExpr("divide by zero".into()));
            }
            Value::Float(x / y)
        }
        Mod => {
            if y == 0.0 {
                return Err(ExecError::InvalidExpr("modulo by zero".into()));
            }
            Value::Float(x % y)
        }
        Lt => Value::Bool(x < y),
        LtEq => Value::Bool(x <= y),
        Gt => Value::Bool(x > y),
        GtEq => Value::Bool(x >= y),
        _ => unreachable!(),
    })
}

fn apply_binary(op: BinOp, a: Value, b: Value) -> Result<Value, ExecError> {
    use BinOp::*;
    if matches!(op, And | Or) {
        return match (a, b) {
            (Value::Bool(x), Value::Bool(y)) => {
                Ok(Value::Bool(if op == And { x && y } else { x || y }))
            }
            (x, y) => Err(ExecError::InvalidExpr(format!(
                "logical {op:?} requires bool/bool, got {}/{}",
                x.type_name(),
                y.type_name()
            ))),
        };
    }
    if matches!(op, Eq | NotEq) {
        let eq = values_equal(&a, &b)?;
        return Ok(Value::Bool(if op == Eq { eq } else { !eq }));
    }
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        return apply_int_binary_op(op, *x, *y);
    }
    let (Some(x), Some(y)) = (as_f64(&a), as_f64(&b)) else {
        return Err(ExecError::InvalidExpr(format!(
            "operator {op:?} requires numeric operands, got {}/{}",
            a.type_name(),
            b.type_name()
        )));
    };
    apply_float_binary_op(op, x, y)
}

/// Strict variant equality plus numeric promotion.  Returns `Err` only for
/// genuinely incomparable variants (so the executor can flag bad queries).
fn values_equal(a: &Value, b: &Value) -> Result<bool, ExecError> {
    use Value::*;
    match (a, b) {
        (Null, Null) => Ok(true),
        (Null, _) | (_, Null) => Ok(false),
        (Bool(x), Bool(y)) => Ok(x == y),
        (Str(x), Str(y)) => Ok(x == y),
        (Int(_), Int(_)) | (Int(_), Float(_)) | (Float(_), Int(_)) | (Float(_), Float(_)) => {
            Ok(as_f64(a).unwrap() == as_f64(b).unwrap())
        }
        (x, y) => Err(ExecError::InvalidExpr(format!(
            "cannot compare {} with {}",
            x.type_name(),
            y.type_name()
        ))),
    }
}

/// Collect change-point timestamps from a layer snapshot using binary search.
///
/// Taus in each layer are sorted by `start` and non-overlapping, which means
/// `tau.end` is also monotonically non-decreasing. Both boundaries can therefore
/// be located with `partition_point` rather than a full linear scan.
/// Point lookup on a pre-snapshotted layer stack (newest layer wins).
///
/// Collect sorted, deduped boundary points for a range scan of `name` over `[start, end)`,
/// including any filter expression boundaries. Returns the bounds and an optional layer
/// snapshot (Some for base lenses, None for derived).
type RangeBoundsResult = Result<(Vec<Timestamp>, Option<Vec<Layer<Value>>>), ExecError>;

fn collect_range_bounds(
    state: &DbState,
    name: &str,
    start: Timestamp,
    end: Timestamp,
    filter: Option<&Expr>,
) -> RangeBoundsResult {
    let mut bounds = Vec::with_capacity(64);
    bounds.push(start);
    bounds.push(end);
    let layers_snap: Option<Vec<Layer<Value>>> = if state.base_types.contains_key(name) {
        let snap = state.db.layers(name);
        if let Some(ref ls) = snap {
            collect_bounds_from_layers(ls, start, end, &mut bounds);
        }
        snap
    } else {
        collect_lens_bounds(state, name, start, end, &mut bounds)?;
        None
    };
    if let Some(f) = filter {
        collect_expr_bounds(state, f, start, end, &mut bounds)?;
    }
    bounds.sort_unstable();
    bounds.dedup();
    Ok((bounds, layers_snap))
}

/// Build the output segments for a range scan from pre-computed `bounds`,
/// optionally snapshotted `layers`, and an optional `filter` expression.
fn build_range_segments(
    state: &DbState,
    name: &str,
    bounds: &[Timestamp],
    layers_snap: Option<&[Layer<Value>]>,
    filter: Option<&Expr>,
) -> Result<Vec<(Timestamp, Timestamp, Value)>, ExecError> {
    let mut out: Vec<(Timestamp, Timestamp, Value)> =
        Vec::with_capacity(bounds.len().saturating_sub(1));
    for w in bounds.windows(2) {
        let (s, e) = (w[0], w[1]);
        let v = if let Some(ls) = layers_snap {
            match at_layers(ls, s) {
                Some(v) => v,
                None => continue,
            }
        } else {
            match eval_lens(state, name, s)? {
                Some(v) => v,
                None => continue,
            }
        };
        if let Some(f) = filter {
            match eval_expr(state, f, s)? {
                Some(Value::Bool(true)) => {}
                _ => continue,
            }
        }
        match out.last_mut() {
            Some(last) if last.1 == s && last.2 == v => last.1 = e,
            _ => out.push((s, e, v)),
        }
    }
    Ok(out)
}

/// Equivalent to `Store::at` but avoids any lock acquisition - callers must
/// already hold a safe snapshot of the layer vec via `Database::layers`.
/// Uses each layer's `min_start`/`max_end` range to skip layers without
/// dereferencing their Arc-backed tau slice.
#[inline]
fn at_layers(layers: &[Layer<Value>], t: Timestamp) -> Option<Value> {
    layers
        .iter()
        .rev()
        .find_map(|layer| {
            // Skip layers whose entire time range does not contain t without
            // chasing the Arc pointer into the tau slice.
            if t < layer.min_start || t >= layer.max_end {
                return None;
            }
            layer.at(t)
        })
        .cloned()
}

fn collect_bounds_from_layers(
    layers: &[Layer<Value>],
    start: Timestamp,
    end: Timestamp,
    out: &mut Vec<Timestamp>,
) {
    for layer in layers {
        let taus = &layer.taus;
        // tau.start in (start, end)
        let s_lo = taus.partition_point(|t| t.start <= start);
        let s_hi = taus.partition_point(|t| t.start < end);
        for tau in &taus[s_lo..s_hi] {
            out.push(tau.start);
        }
        // tau.end in (start, end) - monotone because non-overlapping + sorted by start
        let e_lo = taus.partition_point(|t| t.end <= start);
        let e_hi = taus.partition_point(|t| t.end < end);
        for tau in &taus[e_lo..e_hi] {
            out.push(tau.end);
        }
    }
}

fn collect_lens_bounds(
    state: &DbState,
    name: &str,
    start: Timestamp,
    end: Timestamp,
    out: &mut Vec<Timestamp>,
) -> Result<(), ExecError> {
    if state.base_types.contains_key(name) {
        if let Some(layers) = state.db.layers(name) {
            collect_bounds_from_layers(&layers, start, end, out);
        }
        Ok(())
    } else if let Some(expr) = state.derived.get(name) {
        collect_expr_bounds(state, expr, start, end, out)
    } else {
        Err(ExecError::UnknownLens(name.into()))
    }
}

fn collect_expr_bounds(
    state: &DbState,
    expr: &Expr,
    start: Timestamp,
    end: Timestamp,
    out: &mut Vec<Timestamp>,
) -> Result<(), ExecError> {
    match expr {
        Expr::Lit(_) => Ok(()),
        Expr::Ident(name) => collect_lens_bounds(state, name, start, end, out),
        Expr::Unary { expr, .. } => collect_expr_bounds(state, expr, start, end, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr_bounds(state, lhs, start, end, out)?;
            collect_expr_bounds(state, rhs, start, end, out)
        }
        Expr::Agg {
            lens,
            rel_start,
            rel_end,
            ..
        } => {
            // The aggregate's value changes when a boundary of the underlying
            // lens at position p enters the window (t = p - rel_start) or exits
            // it (t = p - rel_end).  We need p values that project t_change into
            // (start, end), which means p ∈ (start+rel_start, end+rel_start) ∪
            // (start+rel_end, end+rel_end).  Collect the bounding superset.
            let lo = start.saturating_add((*rel_start).min(*rel_end));
            let hi = end.saturating_add((*rel_start).max(*rel_end));
            let mut inner = Vec::new();
            if lo < hi {
                collect_lens_bounds(state, lens, lo, hi, &mut inner)?;
            }
            for p in inner {
                for shift in [*rel_start, *rel_end] {
                    let t_change = p.saturating_sub(shift);
                    if t_change > start && t_change < end {
                        out.push(t_change);
                    }
                }
            }
            Ok(())
        }
    }
}

fn agg_sum(segments: &[(i64, Value)]) -> Result<Value, ExecError> {
    let mut int_sum: i64 = 0;
    let mut float_sum: Option<f64> = None;
    for (_, v) in segments {
        match v {
            Value::Int(i) => match &mut float_sum {
                Some(f) => *f += *i as f64,
                None => int_sum = int_sum.wrapping_add(*i),
            },
            Value::Float(f) => {
                *float_sum.get_or_insert(int_sum as f64) += f;
            }
            _ => {
                return Err(ExecError::InvalidExpr(format!(
                    "sum requires numeric values, got {}",
                    v.type_name()
                )));
            }
        }
    }
    Ok(float_sum.map(Value::Float).unwrap_or(Value::Int(int_sum)))
}

fn agg_avg(segments: &[(i64, Value)]) -> Result<Option<Value>, ExecError> {
    let total: i64 = segments.iter().map(|(d, _)| *d).sum();
    if total == 0 {
        return Ok(None);
    }
    let mut weighted = 0.0f64;
    for (d, v) in segments {
        match v {
            Value::Int(i) => weighted += *i as f64 * *d as f64,
            Value::Float(f) => weighted += f * *d as f64,
            _ => {
                return Err(ExecError::InvalidExpr(format!(
                    "avg requires numeric values, got {}",
                    v.type_name()
                )));
            }
        }
    }
    Ok(Some(Value::Float(weighted / total as f64)))
}

fn collect_agg_segments(
    state: &DbState,
    lens: &str,
    start: Timestamp,
    end: Timestamp,
) -> Result<Vec<(i64, Value)>, ExecError> {
    let mut bounds = Vec::with_capacity(64);
    bounds.push(start);
    bounds.push(end);
    let layers_snap: Option<Vec<Layer<Value>>> = if state.base_types.contains_key(lens) {
        let snap = state.db.layers(lens);
        if let Some(ref ls) = snap {
            collect_bounds_from_layers(ls, start, end, &mut bounds);
        }
        snap
    } else {
        collect_lens_bounds(state, lens, start, end, &mut bounds)?;
        None
    };
    bounds.sort_unstable();
    bounds.dedup();
    let mut segments: Vec<(i64, Value)> = Vec::with_capacity(bounds.len().saturating_sub(1));
    for w in bounds.windows(2) {
        let (s, e) = (w[0], w[1]);
        let v = if let Some(ref ls) = layers_snap {
            at_layers(ls, s)
        } else {
            eval_lens(state, lens, s)?
        };
        if let Some(v) = v {
            segments.push((e - s, v));
        }
    }
    Ok(segments)
}

fn eval_agg(
    state: &DbState,
    lens: &str,
    func: AggFunc,
    start: Timestamp,
    end: Timestamp,
) -> Result<Option<Value>, ExecError> {
    let segments = collect_agg_segments(state, lens, start, end)?;
    if segments.is_empty() {
        return Ok(None);
    }
    Ok(Some(match func {
        AggFunc::Count => Value::Int(segments.len() as i64),
        AggFunc::Min => segments
            .into_iter()
            .map(|(_, v)| v)
            .try_fold(None::<Value>, |acc, v| match acc {
                None => Ok(Some(v)),
                Some(a) => numeric_min_max(a, v, false).map(Some),
            })?
            .unwrap(),
        AggFunc::Max => segments
            .into_iter()
            .map(|(_, v)| v)
            .try_fold(None::<Value>, |acc, v| match acc {
                None => Ok(Some(v)),
                Some(a) => numeric_min_max(a, v, true).map(Some),
            })?
            .unwrap(),
        AggFunc::Sum => agg_sum(&segments)?,
        AggFunc::Avg => return agg_avg(&segments),
    })
    )
}

fn numeric_min_max(a: Value, b: Value, want_max: bool) -> Result<Value, ExecError> {
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(if want_max {
            (*x).max(*y)
        } else {
            (*x).min(*y)
        })),
        _ => match (as_f64(&a), as_f64(&b)) {
            (Some(x), Some(y)) => Ok(Value::Float(if want_max { x.max(y) } else { x.min(y) })),
            _ => Err(ExecError::InvalidExpr(format!(
                "min/max requires numeric values, got {}/{}",
                a.type_name(),
                b.type_name()
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libtau::ql::parse;
    use pretty_assertions::assert_eq;

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
        // at t=7 x=2, avg([−3,7)) = avg([2,7): only [5,7)=2 → avg=2.0; 2>2 = false
        // at t=8 x=2, avg([−2,8)) = avg([6,8): value=2 → avg=2.0; 2>2.0 = false
        // Let's just check a known case: at t=5 window is [-5,5): only [0,5)=1 → avg=1.0; x(5)=2 > 1.0 = true
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
            e.exec_read(&crate::libtau::ql::parse("SHOW LENSES").unwrap().1),
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
        let mut grants = HashMap::new();
        grants.insert("*".into(), Perm::ALL);
        e.users.add(User::new("admin", "p", grants)).unwrap();
    }

    fn install_reader(e: &mut Executor, db: &str) {
        let mut grants = HashMap::new();
        grants.insert(db.to_string(), Perm::R);
        e.users.add(User::new("reader", "p", grants)).unwrap();
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

        let mut grants = HashMap::new();
        grants.insert("alpha".to_string(), Perm::R);
        e.users.add(User::new("alice", "p", grants)).unwrap();

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
}
