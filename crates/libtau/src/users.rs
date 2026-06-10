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

#[cfg(test)]
use argon2::{Algorithm, Params, Version};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use bitflags::bitflags;
use rand::rngs::OsRng;

bitflags! {
    /// Bitmap of granted operations.  Convertible to/from the letters `CRUDA`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct Perm: u8 {
        const C = 0b0_0001;
        const R = 0b0_0010;
        const U = 0b0_0100;
        const D = 0b0_1000;
        const A = 0b1_0000;
    }
}

impl Perm {
    pub const NONE: Self = Self::empty();
    pub const ALL: Self = Self::from_bits_truncate(0b1_1111);

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
        let bits = [
            (Self::C, 'C'),
            (Self::R, 'R'),
            (Self::U, 'U'),
            (Self::D, 'D'),
            (Self::A, 'A'),
        ];
        let s: String = bits
            .iter()
            .filter(|(bit, _)| self.contains(*bit))
            .map(|(_, c)| c)
            .collect();
        f.write_str(&s)
    }
}

fn argon2() -> Argon2<'static> {
    #[cfg(test)]
    {
        let params = Params::new(64, 1, 1, None).expect("argon2 test params");
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    }
    #[cfg(not(test))]
    {
        Argon2::default()
    }
}

fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    argon2()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hashing failed")
        .to_string()
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
        Self {
            name: name.to_string(),
            phc_hash: hash_password(password),
            grants,
        }
    }

    /// Constant-time password verification.
    pub fn verify(&self, password: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(&self.phc_hash) else {
            return false;
        };
        argon2()
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

    /// Replace the user's password with a freshly hashed one.
    pub fn set_password(&mut self, password: &str) {
        self.phc_hash = hash_password(password);
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
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    /// Generator over `Perm` (any of the 32 possible 5-bit bitmaps).
    fn perm_gen() -> impl Generator<Perm> {
        gs::integers::<u8>()
            .min_value(0)
            .max_value(0b1_1111)
            .map(Perm::from_bits_truncate)
    }

    /// Generator over short ASCII identifiers - good enough for db / user
    /// names.  `fullmatch(true)` ensures the *entire* string is alphanumeric;
    /// the default `from_regex` only requires a substring match, which would
    /// happily include `\r`, `\n`, or whitespace and break the line-oriented
    /// user-store format.  This matches the identifier grammar enforced by
    /// the QL parser before any name reaches `User` / `UserStore`.
    fn name_gen() -> impl Generator<String> {
        gs::from_regex("[a-z][a-z0-9_]{0,15}").fullmatch(true)
    }

    /// Generator over a grants HashMap.
    fn grants_gen() -> impl Generator<HashMap<String, Perm>> {
        gs::vecs(name_gen().flat_map(|n| perm_gen().map(move |p| (n.clone(), p))))
            .max_size(6)
            .map(|pairs| pairs.into_iter().collect())
    }

    #[hegel::test]
    fn pbt_perm_display_parse_roundtrip(tc: TestCase) {
        let p = tc.draw(perm_gen());
        let s = p.to_string();
        assert_eq!(Perm::parse(&s).unwrap(), p);
    }

    #[hegel::test]
    fn pbt_perm_parse_letter_order_is_irrelevant(tc: TestCase) {
        // Any permutation of a subset of CRUDA parses to the same Perm.
        let p = tc.draw(perm_gen());
        let s = p.to_string();
        let mut chars: Vec<char> = s.chars().collect();
        if chars.len() <= 1 || chars == vec!['-'] {
            return;
        }
        // Permute by rotating - cheap deterministic shuffle from a draw.
        let shift = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let len = chars.len();
        chars.rotate_left(shift % len.max(1));
        let permuted: String = chars.into_iter().collect();
        assert_eq!(Perm::parse(&permuted).unwrap(), p);
    }

    #[hegel::test]
    fn pbt_perm_parse_case_insensitive(tc: TestCase) {
        let p = tc.draw(perm_gen());
        let upper = p.to_string();
        let lower = upper.to_lowercase();
        assert_eq!(Perm::parse(&upper), Perm::parse(&lower));
    }

    #[test]
    fn perm_parse_special_tokens() {
        assert_eq!(Perm::parse("*").unwrap(), Perm::ALL);
        assert_eq!(Perm::parse("-").unwrap(), Perm::NONE);
        assert_eq!(Perm::parse("").unwrap(), Perm::NONE);
    }

    #[hegel::test]
    fn pbt_perm_parse_rejects_unknown_letter(tc: TestCase) {
        let bad = tc.draw(
            gs::characters().filter(|c| !"CRUDAcruda \t".contains(*c) && *c != '*' && *c != '-'),
        );
        let mut s = String::from("CR");
        s.push(bad);
        assert!(Perm::parse(&s).is_err());
    }

    #[hegel::test]
    fn pbt_perm_bitops_are_commutative_and_associative(tc: TestCase) {
        let a = tc.draw(perm_gen());
        let b = tc.draw(perm_gen());
        let c = tc.draw(perm_gen());
        assert_eq!(a | b, b | a);
        assert_eq!(a & b, b & a);
        assert_eq!((a | b) | c, a | (b | c));
        assert_eq!((a & b) & c, a & (b & c));
    }

    #[hegel::test]
    fn pbt_perm_contains_iff_bits_are_subset(tc: TestCase) {
        let p = tc.draw(perm_gen());
        let q = tc.draw(perm_gen());
        let expected = (p.bits() & q.bits()) == q.bits();
        assert_eq!(p.contains(q), expected);
    }

    #[hegel::test]
    fn pbt_perm_insert_remove_inverse(tc: TestCase) {
        let mut p = tc.draw(perm_gen());
        let q = tc.draw(perm_gen());
        let original = p;
        p.insert(q);
        p.remove(q);
        // After insert+remove of the same bits, the originally-not-present bits
        // are now gone; the result is `original & !q`.
        assert_eq!(p, Perm::from_bits_truncate(original.bits() & !q.bits()));
    }

    #[hegel::test]
    fn pbt_user_verify_accepts_correct_password_only(tc: TestCase) {
        let name = tc.draw(name_gen());
        let pw = tc.draw(gs::text().min_size(1).max_size(32));
        let other = tc.draw(gs::text().min_size(1).max_size(32).filter({
            let pw = pw.clone();
            move |s| *s != pw
        }));
        let u = User::new(&name, &pw, HashMap::new());
        assert!(u.verify(&pw));
        assert!(!u.verify(&other));
    }

    #[hegel::test]
    fn pbt_user_effective_equals_direct_union_wildcard(tc: TestCase) {
        let grants = tc.draw(grants_gen());
        let db = tc.draw(name_gen());
        let u = User::new("u", "p", grants.clone()); // codeql[rust/hard-coded-cryptographic-value]

        let direct = grants.get(&db).copied().unwrap_or_default();
        let wildcard = grants.get("*").copied().unwrap_or_default();
        let expected = direct | wildcard;

        assert_eq!(u.effective(&db), expected);
    }

    #[hegel::test]
    fn pbt_user_is_global_admin_iff_admin_on_wildcard(tc: TestCase) {
        let grants = tc.draw(grants_gen());
        let u = User::new("u", "p", grants.clone()); // codeql[rust/hard-coded-cryptographic-value]
        let expected = grants
            .get("*")
            .map(|p| p.contains(Perm::A))
            .unwrap_or(false);
        assert_eq!(u.is_global_admin(), expected);
    }

    #[hegel::test]
    fn pbt_user_to_from_line_roundtrip(tc: TestCase) {
        let name = tc.draw(name_gen());
        let pw = tc.draw(gs::text().min_size(1).max_size(32));
        let grants = tc.draw(grants_gen());
        let u = User::new(&name, &pw, grants);

        let line = u.to_line();
        let parsed = User::from_line(&line).expect("from_line on its own to_line must succeed");
        assert_eq!(parsed.name, u.name);
        assert_eq!(parsed.grants, u.grants);
        assert!(parsed.verify(&pw));
    }

    #[hegel::test]
    fn pbt_store_add_then_get_returns_user(tc: TestCase) {
        let name = tc.draw(name_gen());
        let mut s = UserStore::new();
        s.add(User::new(&name, "p", HashMap::new())).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
        assert!(s.get(&name).is_some());
        assert!(s.add(User::new(&name, "p", HashMap::new())).is_err()); // codeql[rust/hard-coded-cryptographic-value]
    }

    #[hegel::test]
    fn pbt_store_remove_then_get_returns_none(tc: TestCase) {
        let name = tc.draw(name_gen());
        let mut s = UserStore::new();
        s.add(User::new(&name, "p", HashMap::new())).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
        s.remove(&name).unwrap();
        assert!(s.get(&name).is_none());
        assert!(s.remove(&name).is_err());
    }

    #[hegel::test]
    fn pbt_store_grant_then_revoke_is_identity_on_perms(tc: TestCase) {
        let name = tc.draw(name_gen());
        let db = tc.draw(name_gen());
        let to_add = tc.draw(perm_gen());

        let mut s = UserStore::new();
        s.add(User::new(&name, "p", HashMap::new())).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
        let before = s.get(&name).unwrap().effective(&db);
        s.grant(&name, &db, to_add).unwrap();
        s.revoke(&name, &db, to_add).unwrap();
        assert_eq!(s.get(&name).unwrap().effective(&db), before);
    }

    #[hegel::test]
    fn pbt_store_grant_unions_into_existing_perms(tc: TestCase) {
        let name = tc.draw(name_gen());
        let db = tc.draw(name_gen());
        let p1 = tc.draw(perm_gen());
        let p2 = tc.draw(perm_gen());

        let mut s = UserStore::new();
        s.add(User::new(&name, "p", HashMap::new())).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
        s.grant(&name, &db, p1).unwrap();
        let result = s.grant(&name, &db, p2).unwrap();
        assert_eq!(result, p1 | p2);
    }

    #[hegel::test]
    fn pbt_store_persists_round_trip(tc: TestCase) {
        let n = tc.draw(gs::integers::<usize>().min_value(0).max_value(5));
        let dir = tempdir().unwrap();
        let path = dir.path().join("u");

        let entries: Vec<(String, String, HashMap<String, Perm>)> = (0..n)
            .map(|i| (format!("user{i}"), format!("pw{i}"), tc.draw(grants_gen())))
            .collect();

        {
            let mut s = UserStore::open(&path).unwrap();
            for (name, pw, grants) in &entries {
                s.add(User::new(name, pw, grants.clone())).unwrap();
            }
        }
        let s2 = UserStore::open(&path).unwrap();
        for (name, pw, grants) in &entries {
            assert!(s2.verify(name, pw).is_some(), "verify after reload {name}");
            assert_eq!(&s2.get(name).unwrap().grants, grants);
        }
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
        fs::write(&path, "only_one_field\n").unwrap();
        assert!(UserStore::open(&path).is_err());
    }
}
