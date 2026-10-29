//! Roles and the secrets Converge hands out (formats in `converge_core::secrets`):
//! 256 random bits, shown once, stored only as SHA-256.

use base64::Engine as _;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn parse(s: &str) -> Role {
        if s == "admin" { Role::Admin } else { Role::User }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }
}

/// A fresh 256-bit secret with `prefix`.
pub fn new_secret(prefix: &str) -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS randomness");
    format!("{prefix}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

/// Plain SHA-256 is enough for 256-bit random secrets (no dictionary to try),
/// and it is cheap on the request path, where API keys are checked.
pub fn secret_hash(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use converge_core::secrets::{KEY_PREFIX, is_shaped};

    #[test]
    fn new_secrets_have_the_shape_core_checks() {
        let a = new_secret(KEY_PREFIX);
        assert!(is_shaped(&a, KEY_PREFIX), "{a}");
        assert_ne!(a, new_secret(KEY_PREFIX));
        assert_eq!(secret_hash(&a).len(), 32);
    }
}
