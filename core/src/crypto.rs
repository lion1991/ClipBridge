use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub const NONCE_LEN: usize = 12;
pub const KEY_LEN: usize = 32;
pub const SHA256_LEN: usize = 32;

/// Hex-encoded SHA-256 of `bytes`. Used as the content-addressed key for blob
/// storage and as a duplicate-detection signal on the receive path.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(SHA256_LEN * 2);
    for b in digest {
        s.push(hex_nibble(b >> 4));
        s.push(hex_nibble(b & 0x0f));
    }
    s
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("encrypt failed")]
    Encrypt,
    #[error("decrypt failed")]
    Decrypt,
    #[error("invalid nonce length: expected {NONCE_LEN}, got {0}")]
    NonceLen(usize),
}

pub fn encrypt(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<(Vec<u8>, [u8; NONCE_LEN]), CryptoError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| CryptoError::Encrypt)?;
    Ok((ciphertext, nonce_bytes))
}

pub fn decrypt(
    key: &[u8; KEY_LEN],
    nonce_bytes: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if nonce_bytes.len() != NONCE_LEN {
        return Err(CryptoError::NonceLen(nonce_bytes.len()));
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = [7u8; KEY_LEN];
        let msg = b"hello clipbridge";
        let (ct, nonce) = encrypt(&key, msg).unwrap();
        let pt = decrypt(&key, &nonce, &ct).unwrap();
        assert_eq!(pt, msg);
    }

    #[test]
    fn wrong_key_fails() {
        let key = [7u8; KEY_LEN];
        let bad = [8u8; KEY_LEN];
        let (ct, nonce) = encrypt(&key, b"x").unwrap();
        assert!(decrypt(&bad, &nonce, &ct).is_err());
    }

    #[test]
    fn tamper_fails() {
        let key = [7u8; KEY_LEN];
        let (mut ct, nonce) = encrypt(&key, b"abc").unwrap();
        ct[0] ^= 1;
        assert!(decrypt(&key, &nonce, &ct).is_err());
    }

    #[test]
    fn sha256_known_vectors() {
        // RFC 6234 / FIPS 180-4 well-known vectors
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
