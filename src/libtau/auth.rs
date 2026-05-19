// TODO: support a registry of multiple users rather than a single credential
// pair; required for multi-tenant deployments and for credential revocation.

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use rand::rngs::OsRng;

pub struct Credentials {
    pub username: String,
    phc_hash: String,
}

impl Credentials {
    pub fn new(username: &str, password: &str) -> Self {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .expect("argon2 hashing failed")
            .to_string();
        Self {
            username: username.to_string(),
            phc_hash: hash,
        }
    }

    pub fn verify(&self, username: &str, password: &str) -> bool {
        if self.username != username {
            return false;
        }
        let Ok(parsed) = PasswordHash::new(&self.phc_hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    }
}
