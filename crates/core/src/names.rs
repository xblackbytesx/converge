//! Public ids: what clients put in `"model"`. Models and personas share one
//! namespace (they meet in `/v1/models`), so both follow these rules and the
//! server checks uniqueness across both tables.

/// Longest public id. Client model pickers truncate long before this.
pub const MAX_LEN: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("give it a name")]
    Empty,
    #[error("use at most {MAX_LEN} characters")]
    TooLong,
    #[error("use lowercase letters, digits and . _ - / only")]
    BadChar,
    #[error("start and end with a letter or digit, and use no empty / parts")]
    BadEdge,
}

/// Lowercase ASCII letters, digits and `. _ - /`; starts and ends with a
/// letter or digit; no `//`. So `local/qwen3-30b` and `image-prompt-helper`
/// pass, and nothing needs URL escaping in `/v1/models/{id}`.
pub fn check_public_id(s: &str) -> Result<(), NameError> {
    if s.is_empty() {
        return Err(NameError::Empty);
    }
    if s.len() > MAX_LEN {
        return Err(NameError::TooLong);
    }
    let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-' | b'/');
    if !s.bytes().all(allowed) {
        return Err(NameError::BadChar);
    }
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = s.as_bytes();
    if !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) || s.contains("//") {
        return Err(NameError::BadEdge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ids() {
        for ok in ["local/qwen3-30b", "image-prompt-helper", "gpt-5.1", "a", "openrouter/meta/llama-4_scout"] {
            assert_eq!(check_public_id(ok), Ok(()), "{ok}");
        }
        assert_eq!(check_public_id(""), Err(NameError::Empty));
        assert_eq!(check_public_id(&"a".repeat(MAX_LEN + 1)), Err(NameError::TooLong));
        assert_eq!(check_public_id(&"a".repeat(MAX_LEN)), Ok(()));
        for bad in ["Local/qwen", "has space", "emoji\u{1f600}", "q?x", "a%2f"] {
            assert_eq!(check_public_id(bad), Err(NameError::BadChar), "{bad}");
        }
        for bad in ["/lead", "trail/", "a//b", "-x", "x."] {
            assert_eq!(check_public_id(bad), Err(NameError::BadEdge), "{bad}");
        }
    }
}
