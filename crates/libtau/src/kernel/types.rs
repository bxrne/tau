//! Types shared across the kernel subsystem.

use crate::services::auth::Perm;
use std::collections::HashMap;

/// Result types for auth operations.
#[derive(Debug)]
pub enum AuthResult {
    /// Authentication succeeded (user found and password correct)
    Success { username: String },
    /// Authentication failed (user found but password incorrect)
    Failed,
    /// User not found
    NotFound,
    /// Permission check result
    Permission { allowed: bool },
    /// User operation succeeded
    UserOp {},
    /// List of users
    Users { users: Vec<String> },
    /// Grants for a user
    Grants { grants: HashMap<String, Perm> },
    /// Admin status
    Admin { is_admin: bool },
}

/// Metric events that can be recorded via syscalls.
#[derive(Debug)]
pub enum MetricEvent {
    /// Record an operation with execution time.
    Op {
        op: crate::services::metrics::Op,
        ns: u64,
    },

    /// Record an operation for a specific database.
    DbOp {
        db: String,
        op: crate::services::metrics::Op,
    },

    /// Record a compaction event.
    Compaction,

    /// Record WAL write latency.
    WalWrite { ns: u64 },

    /// Set active connection count.
    SetActiveConnections { n: u64 },

    /// Record a connection accepted.
    ConnectionAccepted,

    /// Record a connection rejected.
    ConnectionRejected,

    /// Record an authentication attempt.
    AuthAttempt,

    /// Record an authentication failure.
    AuthFailure,

    /// Record an error response.
    Error,
}

/// Authentication and authorization events that can be performed via syscalls.
#[derive(Debug)]
pub enum AuthEvent {
    /// Authenticate a user with a password.
    Authenticate { username: String, password: String },

    /// Check if a user has a specific permission on a database.
    CheckPermission {
        username: String,
        database: String,
        perm: Perm,
    },

    /// Create a new user.
    CreateUser {
        name: String,
        password: String,
        grants: HashMap<String, Perm>,
    },

    /// Remove a user.
    RemoveUser { name: String },

    /// Set a user's password.
    SetPassword { username: String, password: String },

    /// Grant permissions to a user on a database.
    Grant {
        username: String,
        database: String,
        perms: Perm,
    },

    /// Revoke permissions from a user on a database.
    Revoke {
        username: String,
        database: String,
        perms: Perm,
    },

    /// List all users.
    ListUsers,

    /// List grants for a specific user.
    ListGrants { username: String },

    /// Check if a user is a global admin.
    IsAdmin { username: String },
}
