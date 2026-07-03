//! Authentication and authorization service for the syscall kernel.
//!
//! This module provides the Service implementation and syscall routing
//! for user authentication and CRUDA authorization.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::kernel::{Service, SyscallCtx, SyscallError};

mod core;

pub use core::{Perm, User, UserStore};

pub struct AuthService {
    users: Arc<Mutex<UserStore>>,
}

impl AuthService {
    pub fn new(users: Arc<Mutex<UserStore>>) -> Self {
        Self { users }
    }

    pub fn users(&self) -> Arc<Mutex<UserStore>> {
        self.users.clone()
    }
}

impl Service for AuthService {
    fn boot(&mut self, _ctx: &mut SyscallCtx<'_>) -> Result<(), SyscallError> {
        // Auth doesn't need initialization - just acknowledge we're ready
        Ok(())
    }
}

impl AuthService {
    /// Authenticate a username + password pair.
    pub fn authenticate(&self, username: &str, password: &str) -> Option<bool> {
        // Return some(true) if auth succeeds, some(false) if auth fails, None if user not found
        let users = self.users.lock().unwrap();
        if let Some(_user) = users.verify(username, password) {
            Some(true)
        } else if users.get(username).is_some() {
            Some(false)
        } else {
            None
        }
    }

    /// Check if a user has a specific permission on a database.
    pub fn check_permission(&self, username: &str, database: &str, perm: Perm) -> bool {
        let users = self.users.lock().unwrap();
        if let Some(user) = users.get(username) {
            let effective = user.effective(database);
            effective.contains(perm)
        } else {
            false
        }
    }

    /// Create a new user with a password and grants.
    pub fn create_user(
        &self,
        name: &str,
        password: &str,
        grants: HashMap<String, Perm>,
    ) -> Result<(), String> {
        let user = User::new(name, password, grants);
        let mut users = self.users.lock().unwrap();
        users.add(user)
    }

    /// Remove a user from the store.
    pub fn remove_user(&self, name: &str) -> Result<(), String> {
        let mut users = self.users.lock().unwrap();
        users.remove(name)
    }

    /// Set a user's password.
    pub fn set_password(&self, username: &str, password: &str) -> Result<(), String> {
        let mut users = self.users.lock().unwrap();
        users.set_password(username, password)
    }

    /// Grant permissions to a user on a database.
    pub fn grant(&self, username: &str, database: &str, perms: Perm) -> Result<Perm, String> {
        let mut users = self.users.lock().unwrap();
        users.grant(username, database, perms)
    }

    /// Revoke permissions from a user on a database.
    pub fn revoke(&self, username: &str, database: &str, perms: Perm) -> Result<Perm, String> {
        let mut users = self.users.lock().unwrap();
        users.revoke(username, database, perms)
    }

    /// Get all user names.
    pub fn list_users(&self) -> Vec<String> {
        let users = self.users.lock().unwrap();
        users.names()
    }

    /// Get grants for a specific user.
    pub fn list_grants(&self, username: &str) -> Option<Vec<(String, Perm)>> {
        let users = self.users.lock().unwrap();
        users.grants_for(username)
    }

    /// Check if a user is a global admin.
    pub fn is_admin(&self, username: &str) -> bool {
        let users = self.users.lock().unwrap();
        if let Some(user) = users.get(username) {
            user.is_global_admin()
        } else {
            false
        }
    }
}
