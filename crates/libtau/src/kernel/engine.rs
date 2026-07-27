//! A capability-based syscall microkernel: direct system calls between isolated
//! subsystems.
//!
//! Services are registered on the [`Kernel`] and accessed only through opaque,
//! typed [`Handle`] capabilities. The kernel provides a [`SyscallCtx`] that enables
//! direct synchronous system calls — no message passing, correlation tracking, or
//! queues. Each syscall completes immediately or returns an error.
//!
//! The core is pure and synchronous: it reads no clock and performs no I/O. All
//! side effects are handled by the [`host`](super::host) module. This split
//! keeps the system deterministically replayable.
//!
//! Statement execution is mediated here: the kernel routes each statement to
//! the service that owns it —
//! - read-only statements → [`QueryService`]
//! - mutations (DDL, appends, transactions, backup/restore) → [`DbService`]
//! - user management (`CREATE USER`, `GRANT`, `SHOW USERS`, …) → [`AuthService`]
//!
//! and applies per-user permission policy ([`super::policy`]) before any
//! service sees the statement.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use crate::clock::Clock;
use crate::func::Registry;
use crate::kernel::policy::{check_permission, filter_show_databases, record_metrics};
use crate::kernel::types::{AuthEvent, AuthResult, MetricEvent};
use crate::ql::Stmt;
use crate::ql::ast::{Cap, TriggerKind};
use crate::services::auth::{AuthService, Perm, User, UserStore};
use crate::services::db::{DbService, ExecError, Output, StorageBackend};
use crate::services::metrics::Metrics;
use crate::services::query::QueryService;
use crate::services::store::{COMPACT_THRESHOLD, FaultInjector};
use crate::value::Value;

use super::handle::{Handle, RawHandle};

/// Logical time, in monotonic ticks. The core never reads a wall clock; a host
/// stamps time at the I/O boundary, which keeps behaviour replayable.
pub type Tick = u64;

/// A pure state machine addressed by a [`Handle`]. It receives a [`SyscallCtx`]
/// for making direct system calls to other subsystems. No blocking, `.await`, or I/O.
pub trait Service: Send + 'static {
    /// Called once at boot with syscall access. Initialize state, start work.
    fn boot(&mut self, ctx: &mut SyscallCtx<'_>) -> Result<(), SyscallError>;
}

/// Errors that can occur during system calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyscallError {
    /// Handle doesn't exist or is stale.
    InvalidHandle,
    /// Operation not supported for this handle type.
    NotSupported,
    /// Resource exhausted (e.g., allocation limits).
    ResourceExhausted,
    /// I/O error from external resource.
    IoError(String),
    /// Internal error (e.g., synchronization failure).
    InternalError(String),
}

/// Capability-secured syscall context. Services use this to make direct
/// system calls to other kernel services.
pub struct SyscallCtx<'a> {
    /// The subsystem's own handle identity.
    me: RawHandle,
    /// Current logical time.
    now: Tick,
    /// Access to kernel resources (slab, external handlers).
    kernel: &'a KernelInner,
}

impl<'a> SyscallCtx<'a> {
    fn new(me: RawHandle, now: Tick, kernel: &'a KernelInner) -> Self {
        Self { me, now, kernel }
    }

    /// Create a syscall context from an immutable kernel reference.  Used by
    /// `call_function` and `fire_on_write_triggers` which run inside
    /// `exec_stmt` (taking `&self`).  All mutations use internal locking.
    fn from_ref(me: RawHandle, now: Tick, kernel: &'a KernelInner) -> Self {
        Self::new(me, now, kernel)
    }

    /// This subsystem's own handle identity.
    pub fn me(&self) -> RawHandle {
        self.me
    }

    /// Current logical time.
    pub fn now(&self) -> Tick {
        self.now
    }

    // NOTE: Core operations
    /// Execute a statement.  Routed by the kernel: reads to the query
    /// service, mutations to the db service, user management to auth.
    pub fn exec(&mut self, stmt: &Stmt) -> Result<Output, ExecError> {
        self.kernel.exec_stmt(stmt)
    }

    /// Execute a statement on behalf of `caller`, enforcing permission policy.
    pub fn exec_as(&mut self, stmt: &Stmt, caller: &str) -> Result<Output, ExecError> {
        self.kernel.exec_stmt_as(stmt, caller)
    }

    // NOTE: Transaction operations
    /// Begin a transaction.
    pub fn begin(&mut self) -> Result<Output, ExecError> {
        self.kernel.exec_stmt(&Stmt::StartTransaction)
    }

    /// Commit the current transaction.
    pub fn commit(&mut self) -> Result<Output, ExecError> {
        self.kernel.exec_stmt(&Stmt::Commit)
    }

    /// Rollback the current transaction.
    pub fn rollback(&mut self) -> Result<Output, ExecError> {
        self.kernel.exec_stmt(&Stmt::Rollback)
    }

    // NOTE: Low-level I/O operations
    /// Read from a host-backed external resource into a buffer.  The handle
    /// must be a live [`Slot::External`] capability issued by
    /// [`Kernel::register_external`].
    pub fn read(
        &mut self,
        handle: Handle<dyn std::any::Any>,
        buf: &mut [u8],
    ) -> Result<usize, SyscallError> {
        self.kernel.external_handler(handle.raw())?.read(buf)
    }

    /// Write a buffer to a host-backed external resource (same capability
    /// rules as [`SyscallCtx::read`]).
    pub fn write(
        &mut self,
        handle: Handle<dyn std::any::Any>,
        data: &[u8],
    ) -> Result<usize, SyscallError> {
        self.kernel.external_handler(handle.raw())?.write(data)
    }

    /// Allocate a new resource.
    pub fn allocate(&self) -> Result<RawHandle, SyscallError> {
        self.kernel.syscall_allocate()
    }

    /// Deallocate a resource.
    pub fn deallocate(&self, handle: RawHandle) -> Result<(), SyscallError> {
        if self.kernel.slab.lock().expect("slab lock").remove(handle) {
            Ok(())
        } else {
            Err(SyscallError::InvalidHandle)
        }
    }

    // NOTE: Capability accessors
    /// The kernel's virtual clock capability (transaction stamps, TTL "now").
    pub fn clock(&self) -> Arc<Clock> {
        self.kernel.clock.clone()
    }

    /// The kernel's fault-injection capability (deterministic simulation).
    pub fn faults(&self) -> Arc<FaultInjector> {
        self.kernel.faults.clone()
    }

    // NOTE: Metrics operations
    /// Record a metric event.
    pub fn metric_record(&self, metric: MetricEvent) -> Result<(), SyscallError> {
        self.kernel.metric_record(metric)
    }

    /// Collect all metrics as prometheus format.
    pub fn metric_collect(&self) -> Result<String, SyscallError> {
        self.kernel.metric_collect()
    }

    // NOTE: Minimal auth operations
    /// Perform an authentication/authorization operation.
    pub fn auth(&self, event: AuthEvent) -> Result<AuthResult, SyscallError> {
        self.kernel.auth(event)
    }
}

/// Internal kernel state shared across syscalls.
struct KernelInner {
    /// Resource slab with capabilities.
    slab: Mutex<Slab>,
    /// External I/O handlers (host-backed resources).
    external: HashMap<RawHandle, Arc<dyn ExternalHandler>>,
    /// Current logical time.
    now: Tick,
    /// Per-kernel virtual clock (transaction stamps, TTL cutoffs).
    clock: Arc<Clock>,
    /// Per-kernel I/O fault injector (deterministic simulation).
    faults: Arc<FaultInjector>,
    /// Metrics service.
    metrics: Arc<Metrics>,
    /// Auth service.
    auth: Arc<AuthService>,
    /// Db (mutation) service.
    db: Arc<DbService>,
    /// Query (read) service.
    query: Arc<QueryService>,
    /// User-defined Lua function registry.
    func: RwLock<Registry>,
    /// Reentrancy guard: triggers don't fire inside other triggers.
    trigger_depth: AtomicU32,
    /// Slab handles of the four built-in services (metrics, auth, db, query),
    /// in registration order.
    service_handles: [RawHandle; 4],
}

/// Trait for external I/O handlers (host-backed resources).
pub trait ExternalHandler: Send + Sync {
    fn read(&self, buf: &mut [u8]) -> Result<usize, SyscallError>;
    fn write(&self, data: &[u8]) -> Result<usize, SyscallError>;
}

impl KernelInner {
    fn new(backend: StorageBackend, compact_threshold: usize) -> Self {
        let clock = Arc::new(Clock::system());
        let faults = Arc::new(FaultInjector::new());
        let metrics = Metrics::arc();
        let auth = Arc::new(AuthService::new(Arc::new(Mutex::new(UserStore::new()))));
        let db = Arc::new(DbService::new(
            backend,
            compact_threshold,
            metrics.clone(),
            clock.clone(),
            faults.clone(),
        ));
        let query = Arc::new(QueryService::new(db.registry()));
        let mut slab = Slab::new();
        // The built-in services are subsystems like any other: each holds a
        // kernel-issued handle, so all capabilities trace back to the slab.
        let service_handles = [(); 4].map(|_| Handle::<()>::new(slab.insert(Slot::Service)).raw());
        Self {
            slab: Mutex::new(slab),
            external: HashMap::new(),
            now: 0,
            clock,
            faults,
            metrics,
            auth,
            db,
            query,
            func: RwLock::new(Registry::new()),
            trigger_depth: AtomicU32::new(0),
            service_handles,
        }
    }

    fn syscall_allocate(&self) -> Result<RawHandle, SyscallError> {
        let raw_handle = self.slab.lock().expect("slab lock").insert(Slot::Service);
        Ok(raw_handle)
    }

    fn register_external(&mut self, handler: Arc<dyn ExternalHandler>) -> RawHandle {
        let raw_handle = self.slab.lock().expect("slab lock").insert(Slot::External);
        self.external.insert(raw_handle, handler);
        raw_handle
    }

    /// Resolve an external-resource capability: the handle must be live in
    /// the slab, of the external kind, and have a registered handler.
    fn external_handler(&self, h: RawHandle) -> Result<&Arc<dyn ExternalHandler>, SyscallError> {
        match self.slab.lock().expect("slab lock").get(h) {
            Some(Slot::External) => self.external.get(&h).ok_or(SyscallError::InvalidHandle),
            Some(Slot::Service) => Err(SyscallError::NotSupported),
            None => Err(SyscallError::InvalidHandle),
        }
    }

    /// Route one statement to the owning service and record metrics.
    fn exec_stmt(&self, stmt: &Stmt) -> Result<Output, ExecError> {
        let t0 = Instant::now();
        let result = match stmt {
            Stmt::ShowUsers => self.show_users(),
            Stmt::ShowGrants { user } => self.show_grants(user.as_deref()),
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
            Stmt::ShowFunctions => Ok(Output::Names(self.func.read().expect("func lock").list())),
            Stmt::CreateFunction {
                name,
                kind,
                caps,
                body,
            } => self.create_function(name, kind, *caps, body),
            Stmt::DropFunction { name } => self.drop_function(name),
            Stmt::CallFunction { name, args } => self.call_function(name, args),
            // Classified read-only (it doesn't mutate database state) but it
            // writes a backup file, which is the db service's job.
            Stmt::BackupDatabase { .. } => self.db.exec(stmt),
            _ if stmt.is_read_only() => self.query.exec_read(stmt),
            _ => {
                let out = self.db.exec(stmt)?;
                // Fire ON WRITE triggers after a successful append (top-level only).
                if self.trigger_depth.load(Ordering::Relaxed) == 0
                    && let Some((lens, taus)) = extract_write_info(stmt)
                {
                    self.fire_on_write_triggers(lens, &taus);
                }
                Ok(out)
            }
        };
        record_metrics(
            &self.metrics,
            self.db.active().as_deref(),
            stmt,
            t0.elapsed().as_nanos() as u64,
        );
        result
    }

    /// Permission-checked [`KernelInner::exec_stmt`].  `SHOW DATABASES` is
    /// filtered to databases the caller has any grant on.
    fn exec_stmt_as(&self, stmt: &Stmt, caller: &str) -> Result<Output, ExecError> {
        let user = self
            .lookup_user(caller)
            .ok_or_else(|| ExecError::UnknownUser(caller.into()))?;
        check_permission(stmt, &user, self.db.active().as_deref())?;

        // Check permission hooks (consultative — both must pass).
        // Hooks run without kernel access: they evaluate caller/stmt globals
        // and return a verdict. If a hook needs tau.exec, it should be an
        // ON WRITE trigger instead.
        {
            let stmt_text = format!("{stmt:?}");
            let func = self.func.read().expect("func lock");
            let verdict = func.check_permission_hooks_simple(caller, &stmt_text);
            if let crate::func::PermissionVerdict::Deny(reason) = verdict {
                return Err(ExecError::PermissionDenied(reason));
            }
        }

        let out = self.exec_stmt(stmt)?;
        Ok(filter_show_databases(out, stmt, &user))
    }

    fn lookup_user(&self, name: &str) -> Option<User> {
        self.auth
            .users()
            .lock()
            .expect("user store lock poisoned")
            .get(name)
            .cloned()
    }

    fn create_user(&self, name: &str, password: &str) -> Result<Output, ExecError> {
        if self.lookup_user(name).is_some() {
            return Err(ExecError::DuplicateUser(name.into()));
        }
        self.auth
            .create_user(name, password, Default::default())
            .map_err(ExecError::Io)?;
        Ok(Output::Empty)
    }

    fn drop_user(&self, name: &str) -> Result<Output, ExecError> {
        self.auth
            .remove_user(name)
            .map_err(|_| ExecError::UnknownUser(name.into()))?;
        Ok(Output::Empty)
    }

    fn grant(&self, perms: Perm, database: &str, user: &str) -> Result<Output, ExecError> {
        if self.lookup_user(user).is_none() {
            return Err(ExecError::UnknownUser(user.into()));
        }
        self.auth
            .grant(user, database, perms)
            .map_err(ExecError::Io)?;
        Ok(Output::Empty)
    }

    fn revoke(&self, perms: Perm, database: &str, user: &str) -> Result<Output, ExecError> {
        if self.lookup_user(user).is_none() {
            return Err(ExecError::UnknownUser(user.into()));
        }
        self.auth
            .revoke(user, database, perms)
            .map_err(ExecError::Io)?;
        Ok(Output::Empty)
    }

    fn show_users(&self) -> Result<Output, ExecError> {
        Ok(Output::Names(self.auth.list_users()))
    }

    fn show_grants(&self, target: Option<&str>) -> Result<Output, ExecError> {
        let mut out = Vec::new();
        match target {
            Some(name) => {
                let grants = self
                    .auth
                    .list_grants(name)
                    .ok_or_else(|| ExecError::UnknownUser(name.into()))?;
                out.push((name.to_string(), grants));
            }
            None => {
                for name in self.auth.list_users() {
                    if let Some(g) = self.auth.list_grants(&name) {
                        out.push((name, g));
                    }
                }
            }
        }
        Ok(Output::Grants(out))
    }

    fn create_function(
        &self,
        name: &str,
        kind: &TriggerKind,
        caps: Cap,
        body: &str,
    ) -> Result<Output, ExecError> {
        // Persist to schema WAL (like CREATE LENS / DERIVE).
        let stmt = Stmt::CreateFunction {
            name: name.to_string(),
            kind: kind.clone(),
            caps,
            body: body.to_string(),
        };
        let _ = self.db.append_schema_active(&stmt.to_string());
        let now_ms = self.clock.now_ms();
        self.func
            .write()
            .expect("func lock")
            .register(name, kind.clone(), caps, body, now_ms)
            .map_err(ExecError::Io)?;
        Ok(Output::Empty)
    }

    fn drop_function(&self, name: &str) -> Result<Output, ExecError> {
        let mut func = self.func.write().expect("func lock");
        if !func.has(name) {
            return Err(ExecError::InvalidExpr(format!("unknown function: {name}")));
        }
        func.drop_fn(name);
        drop(func);
        let stmt = Stmt::DropFunction {
            name: name.to_string(),
        };
        let _ = self.db.append_schema_active(&stmt.to_string());
        Ok(Output::Empty)
    }

    fn call_function(
        &self,
        name: &str,
        args: &[crate::ql::ast::Literal],
    ) -> Result<Output, ExecError> {
        let func = self.func.read().expect("func lock");
        if !func.has(name) {
            return Err(ExecError::InvalidExpr(format!("unknown function: {name}")));
        }
        let mut ctx = SyscallCtx::from_ref(
            RawHandle {
                index: 0,
                generation: 0,
            },
            self.now,
            self,
        );
        func.invoke_call(name, args, &mut ctx)
    }

    /// Fire ON WRITE triggers after a successful append.  Sets the
    /// reentrancy guard so triggers don't fire inside other triggers.
    fn fire_on_write_triggers(&self, lens: &str, taus: &[(i64, i64, Value)]) {
        self.trigger_depth.fetch_add(1, Ordering::Relaxed);
        let func = self.func.read().expect("func lock");
        let mut ctx = SyscallCtx::from_ref(
            RawHandle {
                index: 0,
                generation: 0,
            },
            self.now,
            self,
        );
        if let Err(e) = func.invoke_on_write(lens, taus, &mut ctx) {
            tracing::warn!(lens = %lens, error = ?e, "on_write trigger error");
        }
        self.trigger_depth.fetch_sub(1, Ordering::Relaxed);
    }

    /// Fire all due `SCHEDULE EVERY` functions using the virtual clock.
    fn tick_cron(&self) -> Result<usize, ExecError> {
        self.trigger_depth.fetch_add(1, Ordering::Relaxed);
        let mut func = self.func.write().expect("func lock");
        let mut ctx = SyscallCtx::from_ref(
            RawHandle {
                index: 0,
                generation: 0,
            },
            self.now,
            self,
        );
        let now_ms = self.clock.now_ms();
        let result = func.invoke_due_cron(now_ms, &mut ctx);
        self.trigger_depth.fetch_sub(1, Ordering::Relaxed);
        result
    }

    pub fn metric_record(&self, metric: MetricEvent) -> Result<(), SyscallError> {
        match metric {
            MetricEvent::Op { op, ns } => {
                self.metrics.record_op(op, ns);
            }
            MetricEvent::DbOp { db, op } => {
                self.metrics.record_db_op(&db, op);
            }
            MetricEvent::Compaction => {
                self.metrics.record_compaction();
            }
            MetricEvent::WalWrite { ns } => {
                self.metrics.record_wal_write(ns);
            }
            MetricEvent::SetActiveConnections { n } => {
                self.metrics.set_active_connections(n);
            }
            MetricEvent::ConnectionAccepted => {
                self.metrics.connections.inc();
            }
            MetricEvent::ConnectionRejected => {
                self.metrics.record_rejected_connection();
            }
            MetricEvent::AuthAttempt => {
                self.metrics.record_auth_attempt();
            }
            MetricEvent::AuthFailure => {
                self.metrics.record_auth_failure();
            }
            MetricEvent::Error => {
                self.metrics.record_error();
            }
        }
        Ok(())
    }

    pub fn metric_collect(&self) -> Result<String, SyscallError> {
        Ok(self.metrics.prometheus_text())
    }

    pub fn auth(&self, event: AuthEvent) -> Result<AuthResult, SyscallError> {
        let auth = &self.auth;
        match event {
            AuthEvent::Authenticate { username, password } => {
                match auth.authenticate(&username, &password) {
                    Some(success) => {
                        if success {
                            Ok(AuthResult::Success { username })
                        } else {
                            Ok(AuthResult::Failed)
                        }
                    }
                    None => Ok(AuthResult::NotFound),
                }
            }
            AuthEvent::CheckPermission {
                username,
                database,
                perm,
            } => {
                let allowed = auth.check_permission(&username, &database, perm);
                Ok(AuthResult::Permission { allowed })
            }
            AuthEvent::CreateUser {
                name,
                password,
                grants,
            } => auth
                .create_user(&name, &password, grants)
                .map(|_| AuthResult::UserOp {})
                .map_err(SyscallError::InternalError),
            AuthEvent::RemoveUser { name } => auth
                .remove_user(&name)
                .map(|_| AuthResult::UserOp {})
                .map_err(SyscallError::InternalError),
            AuthEvent::SetPassword { username, password } => auth
                .set_password(&username, &password)
                .map(|_| AuthResult::UserOp {})
                .map_err(SyscallError::InternalError),
            AuthEvent::Grant {
                username,
                database,
                perms,
            } => auth
                .grant(&username, &database, perms)
                .map(|_| AuthResult::UserOp {})
                .map_err(SyscallError::InternalError),
            AuthEvent::Revoke {
                username,
                database,
                perms,
            } => auth
                .revoke(&username, &database, perms)
                .map(|_| AuthResult::UserOp {})
                .map_err(SyscallError::InternalError),
            AuthEvent::ListUsers => {
                let users = auth.list_users();
                Ok(AuthResult::Users { users })
            }
            AuthEvent::ListGrants { username } => match auth.list_grants(&username) {
                Some(grants_vec) => {
                    let grants: HashMap<String, Perm> = grants_vec.into_iter().collect();
                    Ok(AuthResult::Grants { grants })
                }
                None => Ok(AuthResult::NotFound),
            },
            AuthEvent::IsAdmin { username } => {
                let is_admin = auth.is_admin(&username);
                Ok(AuthResult::Admin { is_admin })
            }
        }
    }
}

/// Generational slab with capability-based handles.
struct Slab {
    entries: Vec<Entry>,
    free: Vec<u32>,
}

struct Entry {
    generation: u32,
    slot: Option<Slot>,
}

enum Slot {
    /// A registered service (built-in or user subsystem).
    Service,
    /// A host-backed external I/O resource.
    External,
}

impl Slab {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            free: Vec::new(),
        }
    }

    fn insert(&mut self, slot: Slot) -> RawHandle {
        if let Some(index) = self.free.pop() {
            let e = &mut self.entries[index as usize];
            e.slot = Some(slot);
            RawHandle {
                index,
                generation: e.generation,
            }
        } else {
            let index = self.entries.len() as u32;
            self.entries.push(Entry {
                generation: 0,
                slot: Some(slot),
            });
            RawHandle {
                index,
                generation: 0,
            }
        }
    }

    fn get(&self, h: RawHandle) -> Option<&Slot> {
        let e = self.entries.get(h.index as usize)?;
        if e.generation != h.generation {
            return None;
        }
        e.slot.as_ref()
    }

    fn remove(&mut self, h: RawHandle) -> bool {
        let Some(e) = self.entries.get_mut(h.index as usize) else {
            return false;
        };
        if e.generation != h.generation || e.slot.is_none() {
            return false;
        }
        e.slot = None;
        e.generation = e.generation.wrapping_add(1);
        self.free.push(h.index);
        true
    }
}

/// The syscall microkernel. Owns the built-in services (metrics, auth, db,
/// query) as registered subsystems with slab handles, routes statements
/// between them, and manages user-registered subsystems the same way.
pub struct Kernel {
    inner: KernelInner,
}

impl Kernel {
    /// In-memory kernel with the default compaction threshold.
    pub fn new() -> Self {
        Self::with_threshold(COMPACT_THRESHOLD)
    }

    /// In-memory kernel with a custom layer compaction threshold.
    pub fn with_threshold(compact_threshold: usize) -> Self {
        Self::with_backend(StorageBackend::Memory, compact_threshold)
    }

    /// Kernel with an explicit storage backend for new databases.
    pub fn with_backend(backend: StorageBackend, compact_threshold: usize) -> Self {
        Self {
            inner: KernelInner::new(backend, compact_threshold),
        }
    }

    /// Disk-backed kernel (see [`crate::services::store::Sstable`]). Each
    /// `CREATE DATABASE` allocates `<dir>/<name>.manifest` +
    /// `<dir>/<name>.run.<id>` files paired with a `<dir>/<name>.wal`
    /// write-ahead log, which is the durability mechanism for every append.
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
        Ok(Self::with_backend(
            StorageBackend::Disk {
                dir,
                compression_level,
                enc_key,
                wal_fsync_each,
                wal_max_bytes,
            },
            compact_threshold,
        ))
    }

    /// WAL-backed kernel with the default compaction threshold: an in-memory
    /// store for a single `default` database made durable by a write-ahead log.
    pub fn with_wal(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        Self::with_wal_threshold(path, COMPACT_THRESHOLD, key)
    }

    /// WAL-backed kernel with a custom compaction threshold.
    pub fn with_wal_threshold(
        path: impl AsRef<Path>,
        compact_threshold: usize,
        key: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        let kernel = Self::with_threshold(compact_threshold);
        kernel.inner.db.open_wal_default(path, key)?;
        Ok(kernel)
    }

    /// Execute a single parsed statement.
    pub fn exec(&self, stmt: &Stmt) -> Result<Output, ExecError> {
        self.inner.exec_stmt(stmt)
    }

    /// Execute a statement on behalf of `caller`, applying permission checks
    /// before routing.
    pub fn exec_as(&self, stmt: &Stmt, caller: &str) -> Result<Output, ExecError> {
        self.inner.exec_stmt_as(stmt, caller)
    }

    /// Execute a read-only statement.  Returns [`ExecError::InvalidExpr`] for
    /// any mutating statement — an explicit read-path contract for callers
    /// that must never write (e.g. the server's shared-lock fast path).
    pub fn exec_read(&self, stmt: &Stmt) -> Result<Output, ExecError> {
        if !stmt.is_read_only() {
            return Err(ExecError::InvalidExpr(
                "exec_read called on a mutating statement".into(),
            ));
        }
        self.inner.exec_stmt(stmt)
    }

    /// Read-only counterpart of [`Kernel::exec_as`].  Same permission rules.
    pub fn exec_read_as(&self, stmt: &Stmt, caller: &str) -> Result<Output, ExecError> {
        if !stmt.is_read_only() {
            return Err(ExecError::InvalidExpr(
                "exec_read called on a mutating statement".into(),
            ));
        }
        self.inner.exec_stmt_as(stmt, caller)
    }

    /// Name of the active database, if any.
    pub fn active(&self) -> Option<String> {
        self.inner.db.active()
    }

    /// Whether a transaction is currently buffering mutations.
    pub fn is_in_transaction(&self) -> bool {
        self.inner.db.is_in_transaction()
    }

    /// The kernel's virtual clock.  Pin it ([`Clock::set_fixed_now_ms`]) for
    /// deterministic simulation; other kernels in the process are unaffected.
    pub fn clock(&self) -> Arc<Clock> {
        self.inner.clock.clone()
    }

    /// The kernel's fault injector: arm deterministic I/O failures at the
    /// storage syscall boundary (per-kernel, safe to use in parallel).
    pub fn faults(&self) -> Arc<FaultInjector> {
        self.inner.faults.clone()
    }

    /// The shared metrics sink.
    pub fn metrics(&self) -> Arc<Metrics> {
        self.inner.metrics.clone()
    }

    /// The auth service (user store + policy primitives).
    pub fn auth(&self) -> Arc<AuthService> {
        self.inner.auth.clone()
    }

    /// Replace the backing user store (e.g. a file-backed store at startup).
    pub fn set_users(&self, store: UserStore) {
        *self
            .inner
            .auth
            .users()
            .lock()
            .expect("user store lock poisoned") = store;
    }

    /// Disable per-record WAL fsync across all databases (bulk-load paths).
    pub fn set_wal_fsync_each(&self, on: bool) {
        self.inner.db.set_wal_fsync_each(on);
    }

    /// Set a soft WAL file-size cap (bytes) across all databases.
    pub fn set_wal_max_bytes(&self, bytes: u64) {
        self.inner.db.set_wal_max_bytes(bytes);
    }

    /// Flush the WAL for all databases (group-commit durability boundary).
    pub fn flush_wal(&self) -> io::Result<()> {
        self.inner.db.flush_wal()
    }

    /// Handles of the built-in services (metrics, auth, db, query), in
    /// registration order.
    pub fn service_handles(&self) -> [RawHandle; 4] {
        self.inner.service_handles
    }

    /// Register a subsystem and call its boot method with syscall access.
    pub fn register<S: Service>(&mut self, mut subsystem: S) -> Handle<S> {
        let raw_handle = self
            .inner
            .syscall_allocate()
            .expect("registration should not fail");

        let handle = Handle::new(raw_handle);

        // Call boot with syscall context
        let mut ctx = SyscallCtx::new(raw_handle, self.inner.now, &self.inner);
        let _ = subsystem.boot(&mut ctx);

        handle
    }

    /// Register an external I/O handler (host-backed resource).
    pub fn register_external(&mut self, handler: Arc<dyn ExternalHandler>) -> RawHandle {
        self.inner.register_external(handler)
    }

    /// Advance logical time.
    pub fn advance_to(&mut self, now: Tick) {
        self.inner.now = now;
    }

    pub fn now(&self) -> Tick {
        self.inner.now
    }

    /// Fire due `SCHEDULE EVERY` Lua functions. Intended for the server tick
    /// loop (~100ms) and deterministic simulation.
    pub fn tick_cron(&self) -> Result<usize, ExecError> {
        self.inner.tick_cron()
    }

    /// Create a syscall context for testing/manual intervention.
    pub fn syscall_ctx(&mut self) -> SyscallCtx<'_> {
        SyscallCtx::new(
            RawHandle {
                index: 0,
                generation: 0,
            },
            self.inner.now,
            &self.inner,
        )
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

/// Lens name plus the taus written by an append statement.
type WriteInfo<'a> = (&'a str, Vec<(i64, i64, Value)>);

/// Extract the lens name and written taus from an append statement, for
/// firing ON WRITE triggers.  Returns `None` for non-append statements.
fn extract_write_info(stmt: &Stmt) -> Option<WriteInfo<'_>> {
    match stmt {
        Stmt::Append { name, taus } => {
            let vals = taus
                .iter()
                .map(|(s, e, v)| (*s, *e, crate::value::Value::from(v)))
                .collect();
            Some((name, vals))
        }
        Stmt::BatchAppend { name, taus } => {
            let vals = taus
                .iter()
                .map(|(s, e, v)| (*s, *e, crate::value::Value::from(v)))
                .collect();
            Some((name, vals))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // A simple counter subsystem
    struct Counter {
        handle: Option<RawHandle>,
    }

    impl Service for Counter {
        fn boot(&mut self, ctx: &mut SyscallCtx<'_>) -> Result<(), SyscallError> {
            self.handle = Some(ctx.me());
            Ok(())
        }
    }

    #[test]
    fn subsystem_registration_creates_handle() {
        let mut k = Kernel::new();
        let counter = k.register(Counter { handle: None });
        assert!(counter.raw().index > 0 || counter.raw().generation == 0);
    }

    #[test]
    fn invalid_handle_returns_error() {
        let mut k = Kernel::new();
        let mut ctx = k.syscall_ctx();

        let fake_handle = Handle::new(RawHandle {
            index: 999,
            generation: 0,
        });
        assert_eq!(
            ctx.read(fake_handle, &mut [0u8; 10]),
            Err(SyscallError::InvalidHandle)
        );
    }

    #[test]
    fn logical_time_advances() {
        let mut k = Kernel::new();
        assert_eq!(k.now(), 0);

        k.advance_to(100);
        assert_eq!(k.now(), 100);

        k.advance_to(250);
        assert_eq!(k.now(), 250);
    }

    #[test]
    fn kernel_routes_statements_between_services() {
        use crate::ql::parse;
        let k = Kernel::new();
        for q in [
            "CREATE DATABASE main",
            "CREATE LENS temp int",
            "APPEND LENS temp 0 10 42",
        ] {
            let (_, stmt) = parse(q).expect("parse");
            k.exec(&stmt).expect(q);
        }
        let (_, at) = parse("AT LENS temp 5").expect("parse");
        assert_eq!(
            k.exec(&at).unwrap(),
            Output::Value(Some(crate::value::Value::Int(42)))
        );
    }
}
