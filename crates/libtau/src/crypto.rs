use std::env;
use std::io;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use rand::RngCore;

pub fn parse_key_from_env() -> Result<Option<[u8; 32]>, String> {
    let hex_str = match env::var("TAU_ENCRYPTION_KEY") {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let bytes = hex::decode(&hex_str)
        .map_err(|_| "TAU_ENCRYPTION_KEY must be 64 hex chars (32 bytes)".to_string())?;
    if bytes.len() != 32 {
        return Err("TAU_ENCRYPTION_KEY must be exactly 32 bytes (64 hex chars)".to_string());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(Some(key))
}

pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> io::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    // AES-GCM only fails here if the plaintext exceeds the GCM message limit
    // (~64 GiB); propagate it rather than panicking a write mid-flight.
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "encryption failed"))?;
    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub fn decrypt(key: &[u8; 32], blob: &[u8]) -> io::Result<Vec<u8>> {
    if blob.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "encrypted blob too short",
        ));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
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
    fn pbt_encrypt_decrypt_roundtrips(tc: TestCase) {
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let plaintext = tc.draw(gs::vecs(gs::integers::<u8>()).max_size(2048));
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        let cipher = encrypt(&key, &plaintext).expect("encrypt");
        let recovered = decrypt(&key, &cipher).expect("decrypt must succeed on its own ciphertext");
        assert_eq!(plaintext, recovered);
    }

    #[hegel::test]
    fn pbt_decrypt_rejects_too_short_blobs(tc: TestCase) {
        let blob = tc.draw(gs::vecs(gs::integers::<u8>()).max_size(11));
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let key: [u8; 32] = key_bytes.try_into().expect("exactly 32 bytes");
        assert!(decrypt(&key, &blob).is_err());
    }

    #[hegel::test]
    fn pbt_decrypt_rejects_wrong_key(tc: TestCase) {
        let plaintext = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(1).max_size(512));
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let key_a: [u8; 32] = key_bytes.try_into().expect("exactly 32 bytes");
        let mut key_b = key_a;
        key_b[0] ^= 1;
        let cipher = encrypt(&key_a, &plaintext).expect("encrypt");
        assert!(decrypt(&key_b, &cipher).is_err());
    }

    #[test]
    fn decode_hex_valid() {
        assert_eq!(
            hex::decode("deadbeef").ok(),
            Some(vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(hex::decode("00ff").ok(), Some(vec![0x00, 0xff]));
        assert_eq!(hex::decode("").ok(), Some(vec![]));
    }

    #[test]
    fn decode_hex_odd_length_returns_none() {
        assert!(hex::decode("abc").is_err());
        assert!(hex::decode("f").is_err());
    }

    #[test]
    fn decode_hex_invalid_chars_returns_none() {
        assert!(hex::decode("zz").is_err());
        assert!(hex::decode("gg").is_err());
    }
}
