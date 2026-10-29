//! The shape of every secret Converge hands out: a prefix naming its kind,
//! then 32 random bytes as unpadded base64url (43 characters). Stored only as
//! SHA-256, shown once.
//!
//! The shape check runs before any hashing or lookup, so garbage in an
//! `Authorization` header costs nothing.

/// API keys, `Authorization: Bearer cvg_...`.
pub const KEY_PREFIX: &str = "cvg_";
/// One-time invite links.
pub const INVITE_PREFIX: &str = "cvi_";
/// Browser session cookies.
pub const SESSION_PREFIX: &str = "cvs_";

/// Length of the random part: 32 bytes, base64url without padding.
pub const BODY_LEN: usize = 43;

/// Whether `s` is `prefix` followed by exactly [`BODY_LEN`] base64url chars.
pub fn is_shaped(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|body| {
        body.len() == BODY_LEN && body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// The first characters of a key, safe to show and log (`cvg_AbCd`).
pub fn display_prefix(s: &str) -> &str {
    let end = s.char_indices().nth(KEY_PREFIX.len() + 4).map_or(s.len(), |(i, _)| i);
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "cvg_0123456789abcdefghijklmnopqrstuvwxyzABCDE-_";

    #[test]
    fn shape_is_prefix_plus_43_base64url_chars() {
        assert!(is_shaped(GOOD, KEY_PREFIX));
        assert!(!is_shaped(&format!("{GOOD}x"), KEY_PREFIX), "too long");
        let ok = format!("{KEY_PREFIX}{}", "A".repeat(BODY_LEN));
        assert!(is_shaped(&ok, KEY_PREFIX));
        assert!(!is_shaped(&ok, INVITE_PREFIX), "prefix names the kind");
        assert!(!is_shaped(&format!("{KEY_PREFIX}{}", "A".repeat(BODY_LEN - 1)), KEY_PREFIX));
        assert!(!is_shaped(&format!("{KEY_PREFIX}{}=", "A".repeat(BODY_LEN - 1)), KEY_PREFIX), "no padding");
        assert!(!is_shaped(&format!("{KEY_PREFIX}{}+", "A".repeat(BODY_LEN - 1)), KEY_PREFIX), "url-safe only");
        assert!(!is_shaped("", KEY_PREFIX));
    }

    #[test]
    fn display_prefix_shows_four_chars_after_the_kind() {
        assert_eq!(display_prefix(GOOD), "cvg_0123");
        assert_eq!(display_prefix("cvg_"), "cvg_");
        assert_eq!(display_prefix("short"), "short");
    }
}
