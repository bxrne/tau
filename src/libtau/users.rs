//! Multi-user authentication and per-database CRUDA permissions.
//!
//! Each user has a name, an argon2id password hash, and a map of
//! `database_name → Perm`.  `Perm` is a 5-bit bitmap covering Create / Read /
//! Update / Delete / Admin.  The special database name `"*"` is a wildcard
//! whose grants apply to every database (including ones created later).
//!
//! Persistence: plain text, one user per line:
//!
//! ```text
//! <name> <argon2-phc-hash> <db1>:<perms1> <db2>:<perms2> …
//! ```
//!
//! Where `<perms>` is any combination of the letters `CRUDA` (case-insensitive)
//! or the literal `*` (all 5 bits) or `-` (empty).

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use rand::rngs::OsRng;

/// Bitmap of granted operations.  Convertible to/from the letters `CRUDA`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Perm(pub u8);

impl Perm {
    pub const NONE: Self = Self(0);
    pub const C: Self = Self(0b0_0001);
    pub const R: Self = Self(0b0_0010);
    pub const U: Self = Self(0b0_0100);
    pub const D: Self = Self(0b0_1000);
    pub const A: Self = Self(0b1_0000);
    pub const ALL: Self = Self(0b1_1111);

    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Parse from a letter string like `"CRUD"`, `"R"`, `"CRUDA"`, `"*"`,
    /// `"-"` (empty).  Letters are case-insensitive and may appear in any order.
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.is_empty() || s == "-" {
            return Ok(Self::NONE);
        }
        if s == "*" {
            return Ok(Self::ALL);
        }
        let mut p = Self::NONE;
        for c in s.chars() {
            match c.to_ascii_uppercase() {
                'C' => p.insert(Self::C),
                'R' => p.insert(Self::R),
                'U' => p.insert(Self::U),
                'D' => p.insert(Self::D),
                'A' => p.insert(Self::A),
                ' ' | '\t' => {}
                other => return Err(format!("unknown permission letter: {}", other)),
            }
        }
        Ok(p)
    }
}

impl std::fmt::Display for Perm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return f.write_str("-");
        }
        let mut s = String::with_capacity(5);
        if self.contains(Self::C) {
            s.push('C');
        }
        if self.contains(Self::R) {
            s.push('R');
        }
        if self.contains(Self::U) {
            s.push('U');
        }
        if self.contains(Self::D) {
            s.push('D');
        }
        if self.contains(Self::A) {
            s.push('A');
        }
        f.write_str(&s)
    }
}

impl std::ops::BitOr for Perm {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitAnd for Perm {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// A single authenticated principal.
#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub phc_hash: String,
    /// `database_name → Perm`.  Special key `"*"` applies to every database
    /// (including ones created in the future).
    pub grants: HashMap<String, Perm>,
}

impl User {
    /// Create a fresh user with the given password (hashed with argon2id) and
    /// initial grants.
    pub fn new(name: &str, password: &str, grants: HashMap<String, Perm>) -> Self {
        let salt = SaltString::generate(&mut OsRng);
        let phc_hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .expect("argon2 hashing failed")
            .to_string();
        Self {
            name: name.to_string(),
            phc_hash,
            grants,
        }
    }

    /// Constant-time password verification.
    pub fn verify(&self, password: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(&self.phc_hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    }

    /// Effective permissions on `db`: grants for `db` unioned with wildcard
    /// grants on `"*"`.
    pub fn effective(&self, db: &str) -> Perm {
        let direct = self.grants.get(db).copied().unwrap_or_default();
        let wildcard = self.grants.get("*").copied().unwrap_or_default();
        direct | wildcard
    }

    /// `true` when the user has the admin bit on the wildcard database - only
    /// global admins may manage users or create databases.
    pub fn is_global_admin(&self) -> bool {
        self.grants
            .get("*")
            .map(|p| p.contains(Perm::A))
            .unwrap_or(false)
    }

    /// `true` when the user has the admin bit on at least one database.
    pub fn is_admin_anywhere(&self) -> bool {
        self.grants.values().any(|p| p.contains(Perm::A))
    }

    /// Replace the user's password with a freshly hashed one.
    pub fn set_password(&mut self, password: &str) {
        let salt = SaltString::generate(&mut OsRng);
        self.phc_hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .expect("argon2 hashing failed")
            .to_string();
    }

    fn to_line(&self) -> String {
        let mut grants: Vec<(&String, &Perm)> = self.grants.iter().collect();
        grants.sort_by(|a, b| a.0.cmp(b.0));
        let mut line = format!("{} {}", self.name, self.phc_hash);
        for (db, perm) in grants {
            line.push(' ');
            line.push_str(&format!("{}:{}", db, perm));
        }
        line
    }

    fn from_line(line: &str) -> Option<Self> {
        let mut parts = line.split_whitespace();
        let name = parts.next()?.to_string();
        let phc_hash = parts.next()?.to_string();
        let mut grants = HashMap::new();
        for grant in parts {
            let (db, perms) = grant.split_once(':')?;
            let perm = Perm::parse(perms).ok()?;
            grants.insert(db.to_string(), perm);
        }
        Some(Self {
            name,
            phc_hash,
            grants,
        })
    }
}

/// File-backed registry of users.  All mutators automatically persist when a
/// `path` is configured.
pub struct UserStore {
    users: HashMap<String, User>,
    path: Option<PathBuf>,
}

impl Default for UserStore {
    fn default() -> Self {
        Self::new()
    }
}

impl UserStore {
    pub fn new() -> Self {
        Self {
            users: HashMap::new(),
            path: None,
        }
    }

    /// Open the store at `path`, loading existing entries or creating an empty
    /// in-memory store if the file does not yet exist.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut users = HashMap::new();
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            for (lineno, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let user = User::from_line(trimmed).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("malformed users-file line {}", lineno + 1),
                    )
                })?;
                users.insert(user.name.clone(), user);
            }
        }
        Ok(Self {
            users,
            path: Some(path),
        })
    }

    /// Persist the current user set to the configured file (no-op when no path
    /// is set - useful for tests / in-memory stores).
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut tmp = path.clone();
        tmp.set_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            let mut names: Vec<&String> = self.users.keys().collect();
            names.sort();
            for name in names {
                let user = &self.users[name];
                writeln!(f, "{}", user.to_line())?;
            }
            f.sync_data()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn add(&mut self, user: User) -> Result<(), String> {
        if self.users.contains_key(&user.name) {
            return Err(format!("user already exists: {}", user.name));
        }
        let name = user.name.clone();
        self.users.insert(name, user);
        self.save().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        if self.users.remove(name).is_none() {
            return Err(format!("no such user: {}", name));
        }
        self.save().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&User> {
        self.users.get(name)
    }

    /// Verify a username + password pair, returning the matched user on success.
    pub fn verify(&self, name: &str, password: &str) -> Option<&User> {
        let user = self.users.get(name)?;
        if user.verify(password) {
            Some(user)
        } else {
            None
        }
    }

    /// Sorted list of all user names.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.users.keys().cloned().collect();
        out.sort();
        out
    }

    /// Update one user's grants on a single database.  Returns the new effective
    /// `Perm` for that database after the change.
    pub fn grant(&mut self, user: &str, database: &str, perms: Perm) -> Result<Perm, String> {
        let u = self
            .users
            .get_mut(user)
            .ok_or_else(|| format!("no such user: {}", user))?;
        let entry = u.grants.entry(database.to_string()).or_default();
        entry.insert(perms);
        let new = *entry;
        self.save().map_err(|e| e.to_string())?;
        Ok(new)
    }

    pub fn revoke(&mut self, user: &str, database: &str, perms: Perm) -> Result<Perm, String> {
        let u = self
            .users
            .get_mut(user)
            .ok_or_else(|| format!("no such user: {}", user))?;
        let entry = u.grants.entry(database.to_string()).or_default();
        entry.remove(perms);
        let new = *entry;
        // Drop empty grant entries so display stays tidy.
        if new.is_empty() {
            u.grants.remove(database);
        }
        self.save().map_err(|e| e.to_string())?;
        Ok(new)
    }

    /// Return `(database, perm)` pairs for one user, sorted by database name.
    pub fn grants_for(&self, name: &str) -> Option<Vec<(String, Perm)>> {
        let u = self.users.get(name)?;
        let mut out: Vec<(String, Perm)> = u.grants.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_grants(items: &[(&str, Perm)]) -> HashMap<String, Perm> {
        items.iter().map(|(d, p)| (d.to_string(), *p)).collect()
    }

    #[test]
    fn perm_parse_letters_in_any_case_and_order() {
        assert_eq!(Perm::parse("CRUDA").unwrap(), Perm::ALL);
        assert_eq!(Perm::parse("Adurc").unwrap(), Perm::ALL);
        assert_eq!(Perm::parse("R").unwrap(), Perm::R);
        assert_eq!(Perm::parse("rd").unwrap(), Perm::R | Perm::D);
    }

    #[test]
    fn perm_parse_special_tokens() {
        assert_eq!(Perm::parse("*").unwrap(), Perm::ALL);
        assert_eq!(Perm::parse("-").unwrap(), Perm::NONE);
        assert_eq!(Perm::parse("").unwrap(), Perm::NONE);
    }

    #[test]
    fn perm_parse_rejects_unknown_letter() {
        assert!(Perm::parse("CRX").is_err());
    }

    #[test]
    fn perm_display_round_trips() {
        for p in [
            Perm::NONE,
            Perm::R,
            Perm::C | Perm::R | Perm::U | Perm::D,
            Perm::ALL,
        ] {
            let s = p.to_string();
            assert_eq!(Perm::parse(&s).unwrap(), p);
        }
    }

    #[test]
    fn perm_contains_and_bitops() {
        let p = Perm::C | Perm::R;
        assert!(p.contains(Perm::R));
        assert!(!p.contains(Perm::U));
        assert!((p & Perm::C).contains(Perm::C));
    }

    #[test]
    fn user_verify_accepts_correct_password() {
        let u = User::new("alice", "s3cret", HashMap::new());
        assert!(u.verify("s3cret"));
        assert!(!u.verify("nope"));
    }

    #[test]
    fn user_effective_unions_wildcard_with_direct() {
        let u = User::new(
            "alice",
            "p",
            make_grants(&[("main", Perm::R), ("*", Perm::C)]),
        );
        let eff = u.effective("main");
        assert!(eff.contains(Perm::R));
        assert!(eff.contains(Perm::C));
        let other = u.effective("other_db");
        assert!(other.contains(Perm::C));
        assert!(!other.contains(Perm::R));
    }

    #[test]
    fn user_is_global_admin_only_with_wildcard_admin() {
        let u1 = User::new("a", "p", make_grants(&[("*", Perm::A)]));
        assert!(u1.is_global_admin());
        let u2 = User::new("a", "p", make_grants(&[("main", Perm::A)]));
        assert!(!u2.is_global_admin());
        assert!(u2.is_admin_anywhere());
    }

    #[test]
    fn user_to_and_from_line_round_trip() {
        let u = User::new(
            "alice",
            "secret",
            make_grants(&[("main", Perm::R | Perm::U), ("*", Perm::A)]),
        );
        let line = u.to_line();
        let parsed = User::from_line(&line).unwrap();
        assert_eq!(parsed.name, u.name);
        assert_eq!(parsed.grants, u.grants);
        assert!(parsed.verify("secret"));
    }

    #[test]
    fn store_add_get_remove() {
        let mut s = UserStore::new();
        s.add(User::new("alice", "p", HashMap::new())).unwrap();
        assert!(s.get("alice").is_some());
        assert!(s.add(User::new("alice", "p", HashMap::new())).is_err());
        s.remove("alice").unwrap();
        assert!(s.get("alice").is_none());
        assert!(s.remove("alice").is_err());
    }

    #[test]
    fn store_verify_returns_user_on_match() {
        let mut s = UserStore::new();
        s.add(User::new("a", "p", HashMap::new())).unwrap();
        assert!(s.verify("a", "p").is_some());
        assert!(s.verify("a", "wrong").is_none());
        assert!(s.verify("nope", "p").is_none());
    }

    #[test]
    fn store_grant_and_revoke_update_persisted_state() {
        let mut s = UserStore::new();
        s.add(User::new("a", "p", HashMap::new())).unwrap();
        let perms = s.grant("a", "main", Perm::R | Perm::U).unwrap();
        assert_eq!(perms, Perm::R | Perm::U);
        let after = s.revoke("a", "main", Perm::U).unwrap();
        assert_eq!(after, Perm::R);
        let final_ = s.revoke("a", "main", Perm::R).unwrap();
        assert_eq!(final_, Perm::NONE);
        // Empty grant entries are pruned.
        assert!(s.grants_for("a").unwrap().is_empty());
    }

    #[test]
    fn store_persists_to_disk_and_reloads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("users");
        {
            let mut s = UserStore::open(&path).unwrap();
            s.add(User::new(
                "admin",
                "topsecret",
                make_grants(&[("*", Perm::ALL)]),
            ))
            .unwrap();
            s.add(User::new("reader", "rp", make_grants(&[("main", Perm::R)])))
                .unwrap();
        }
        let s2 = UserStore::open(&path).unwrap();
        assert_eq!(s2.names(), vec!["admin", "reader"]);
        assert!(s2.verify("admin", "topsecret").is_some());
        assert!(s2.verify("reader", "rp").is_some());
        assert_eq!(s2.get("reader").unwrap().effective("main"), Perm::R);
    }

    #[test]
    fn store_open_returns_empty_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope");
        let s = UserStore::open(&path).unwrap();
        assert!(s.names().is_empty());
    }

    #[test]
    fn store_open_rejects_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("u");
        std::fs::write(&path, "only_one_field\n").unwrap();
        assert!(UserStore::open(&path).is_err());
    }
}
