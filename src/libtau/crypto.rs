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

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use pretty_assertions::assert_eq;

    #[hegel::test]
    fn encrypt_decrypt_roundtrips(tc: TestCase) {
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let plaintext = tc.draw(gs::vecs(gs::integers::<u8>()).max_size(2048));
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        let cipher = encrypt(&key, &plaintext);
        let recovered = decrypt(&key, &cipher).expect("decrypt must succeed on its own ciphertext");
        assert_eq!(plaintext, recovered);
    }

    #[hegel::test]
    fn decrypt_rejects_too_short_blobs(tc: TestCase) {
        let blob = tc.draw(gs::vecs(gs::integers::<u8>()).max_size(11));
        let key = [0u8; 32];
        assert!(decrypt(&key, &blob).is_err());
    }

    #[hegel::test]
    fn decrypt_rejects_wrong_key(tc: TestCase) {
        let plaintext = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(1).max_size(512));
        let key_a = [0x11u8; 32];
        let key_b = [0x22u8; 32];
        let cipher = encrypt(&key_a, &plaintext);
        assert!(decrypt(&key_b, &cipher).is_err());
    }
}
