//! Connection credentials and extra headers at rest: XChaCha20-Poly1305
//! under the operator's `CONVERGE_SECRET_KEY`. The row id and the column are
//! the associated data, so a ciphertext copied onto another row or column
//! fails to open instead of leaking there. Stored as
//! `nonce (24) || ciphertext+tag`.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

pub struct Secrets {
    cipher: XChaCha20Poly1305,
}

#[derive(Debug, thiserror::Error)]
#[error("stored credential cannot be decrypted (was CONVERGE_SECRET_KEY changed?); enter it again")]
pub struct Unreadable;

impl Secrets {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { cipher: XChaCha20Poly1305::new(key.into()) }
    }

    pub fn seal(&self, plaintext: &str, aad: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).expect("OS randomness");
        let ct = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext.as_bytes(), aad })
            .expect("XChaCha20 encryption cannot fail for in-memory buffers");
        [nonce.as_slice(), &ct].concat()
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<String, Unreadable> {
        if sealed.len() < 24 + 16 {
            return Err(Unreadable);
        }
        let (nonce, ct) = sealed.split_at(24);
        let pt = self.cipher.decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad }).map_err(|_| Unreadable)?;
        String::from_utf8(pt).map_err(|_| Unreadable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_binding() {
        let s = Secrets::new(&[7u8; 32]);
        let sealed = s.seal("sk-secret", b"row-1");
        assert_eq!(s.open(&sealed, b"row-1").unwrap(), "sk-secret");
        assert!(s.open(&sealed, b"row-2").is_err(), "bound to its row");
        assert!(Secrets::new(&[8u8; 32]).open(&sealed, b"row-1").is_err(), "bound to the key");
        assert_ne!(s.seal("sk-secret", b"row-1"), sealed, "fresh nonce every time");
        assert!(s.open(&sealed[..10], b"row-1").is_err());
    }
}
