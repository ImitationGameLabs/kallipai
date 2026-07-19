//! Username normalization + validation for the in-site display handle chosen at
//! invite redemption. The username is NOT the login id (login resolves by
//! `crate::email`); it is a required, unique handle stored on the user row and
//! surfaced in `/v1/me`, and used as the fallback WebAuthn `displayName` when
//! the client omits one.
//!
//! Rules: trim surrounding whitespace, fold to ASCII lower-case, then require
//! `^[a-z0-9_-]{3,32}$`. ASCII-only folding is deliberate — Unicode
//! case-folding would make matches normalization-form-dependent and open
//! homoglyph/collision surprises.

use kallip_common::protocol::ApiError;

/// Minimum/maximum username length after normalization.
const MIN_LEN: usize = 3;
const MAX_LEN: usize = 32;

/// A username that failed validation.
#[derive(Debug)]
pub enum UsernameError {
    TooShort,
    TooLong,
    InvalidChars,
}

impl std::fmt::Display for UsernameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            UsernameError::TooShort => format!("username must be at least {MIN_LEN} chars"),
            UsernameError::TooLong => format!("username must be at most {MAX_LEN} chars"),
            UsernameError::InvalidChars => {
                "username may only contain a-z, 0-9, '_', '-'".to_string()
            }
        };
        write!(f, "{msg}")
    }
}

impl std::error::Error for UsernameError {}

impl From<UsernameError> for ApiError {
    fn from(e: UsernameError) -> Self {
        ApiError::bad_request(e.to_string())
    }
}

/// Normalize + validate a raw username: trim, ASCII-lowercase, check length and
/// charset. Returns the canonical form used both for storage and for lookup.
pub fn normalize(raw: &str) -> Result<String, UsernameError> {
    let s = raw.trim().to_ascii_lowercase();
    let len = s.chars().count();
    if len < MIN_LEN {
        return Err(UsernameError::TooShort);
    }
    if len > MAX_LEN {
        return Err(UsernameError::TooLong);
    }
    if !s.chars().all(is_allowed) {
        return Err(UsernameError::InvalidChars);
    }
    Ok(s)
}

fn is_allowed(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn trims_and_lowercases() {
        assert_eq!(normalize("  Alice_Doe  ").unwrap(), "alice_doe");
    }

    #[test]
    fn rejects_too_short() {
        assert!(normalize("ab").is_err());
    }

    #[test]
    fn rejects_too_long() {
        assert!(normalize(&"a".repeat(33)).is_err());
    }

    #[test]
    fn accepts_boundary_lengths() {
        assert_eq!(normalize("abc").unwrap(), "abc");
        assert_eq!(normalize(&"a".repeat(32)).unwrap(), "a".repeat(32));
    }

    #[test]
    fn rejects_invalid_chars() {
        assert!(normalize("Alice Doe").is_err()); // space inside
        assert!(normalize("alice@doe").is_err());
        assert!(normalize("café").is_err()); // non-ASCII
    }

    #[test]
    fn allows_digits_and_separators() {
        assert_eq!(normalize("a1-b_c").unwrap(), "a1-b_c");
    }
}
