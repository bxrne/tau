use std::env;
use std::io;
use std::process;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use rand::RngCore;

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

pub fn parse_key_from_env() -> Option<[u8; 32]> {
    let hex = env::var("TAU_ENCRYPTION_KEY").ok()?;
    let bytes = decode_hex(&hex).unwrap_or_else(|| {
        eprintln!("TAU_ENCRYPTION_KEY must be 64 hex chars (32 bytes)");
        process::exit(1);
    });
    if bytes.len() != 32 {
        eprintln!("TAU_ENCRYPTION_KEY must be exactly 32 bytes (64 hex chars)");
        process::exit(1);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Some(key)
}

pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("key is 32 bytes");
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext).expect("encryption failed");
    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    out
}

pub fn decrypt(key: &[u8; 32], blob: &[u8]) -> io::Result<Vec<u8>> {
    if blob.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "encrypted blob too short",
        ));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(key).expect("key is 32 bytes");
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ciphertext).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "decryption failed: wrong key or corrupted data",
        )
    })
}
