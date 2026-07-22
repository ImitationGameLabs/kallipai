//! Username normalization + validation for the in-site display handle chosen at
//! invite redemption. The username is NOT the login id (login resolves by
//! `crate::email`); it is a required, unique handle stored on the user row and
//! surfaced in `/v1/me`, and used as the fallback WebAuthn `displayName` when
//! the client omits one.
//!
//! Rules (GitHub-aligned): trim surrounding whitespace, fold to ASCII
//! lower-case, require 3-32 chars of `[a-z0-9-]` where a hyphen must be single
//! and interior -- no leading, trailing, or consecutive hyphens (`-foo`,
//! `foo-`, `foo--bar`), and no underscores. ASCII-only folding is deliberate --
//! Unicode case-folding would make matches normalization-form-dependent and
//! open homoglyph/collision surprises.

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
                "username may only contain a-z, 0-9, and single hyphens (not at the start or end)"
                    .to_string()
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
/// shape. Returns the canonical form used both for storage and for lookup.
pub fn normalize(raw: &str) -> Result<String, UsernameError> {
    let s = raw.trim().to_ascii_lowercase();
    let len = s.chars().count();
    if len < MIN_LEN {
        return Err(UsernameError::TooShort);
    }
    if len > MAX_LEN {
        return Err(UsernameError::TooLong);
    }
    if !is_valid_handle(&s) {
        return Err(UsernameError::InvalidChars);
    }
    Ok(s)
}

/// GitHub-style handle shape: alphanumeric runs separated by SINGLE interior
/// hyphens. Splitting on `-` yields one non-empty run per segment, so a
/// leading / trailing / consecutive hyphen surfaces as an empty segment, and
/// any non-`[a-z0-9]` char (underscore, punctuation, non-ASCII) fails the
/// inner check.
fn is_valid_handle(s: &str) -> bool {
    s.split('-').all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::normalize;

    // -- happy paths --------------------------------------------------------

    #[test]
    fn trims_and_lowercases() {
        assert_eq!(normalize("  Alice-Doe  ").unwrap(), "alice-doe");
    }

    #[test]
    fn accepts_alphanumeric() {
        assert_eq!(normalize("alice").unwrap(), "alice");
    }

    #[test]
    fn accepts_single_interior_hyphens() {
        assert_eq!(normalize("a-b").unwrap(), "a-b");
        assert_eq!(normalize("a-b-c").unwrap(), "a-b-c");
        assert_eq!(normalize("abc-def").unwrap(), "abc-def");
    }

    #[test]
    fn accepts_all_digits() {
        // No "must contain a letter" rule.
        assert_eq!(normalize("123").unwrap(), "123");
    }

    #[test]
    fn accepts_boundary_lengths() {
        assert_eq!(normalize("abc").unwrap(), "abc");
        assert_eq!(normalize(&"a".repeat(32)).unwrap(), "a".repeat(32));
    }

    // -- hyphen placement ---------------------------------------------------

    #[test]
    fn rejects_leading_hyphen() {
        assert!(normalize("-foo").is_err());
    }

    #[test]
    fn rejects_trailing_hyphen() {
        assert!(normalize("foo-").is_err());
        // Trailing hyphen at the max-length boundary too.
        let mut s = "a".repeat(31);
        s.push('-');
        assert!(normalize(&s).is_err());
    }

    #[test]
    fn rejects_consecutive_hyphens() {
        assert!(normalize("foo--bar").is_err());
        assert!(normalize("a---b").is_err());
    }

    #[test]
    fn rejects_hyphen_only() {
        assert!(normalize("-").is_err());
        assert!(normalize("---").is_err());
    }

    // -- invalid characters -------------------------------------------------

    #[test]
    fn rejects_underscore() {
        // The whole point of the tightening: underscores are no longer allowed.
        assert!(normalize("foo_bar").is_err());
        assert!(normalize("_foo").is_err());
        assert!(normalize("foo_").is_err());
    }

    #[test]
    fn rejects_special_chars_and_non_ascii() {
        assert!(normalize("foo@bar").is_err());
        assert!(normalize("foo.bar").is_err());
        assert!(normalize("foo bar").is_err()); // interior space
        assert!(normalize("café").is_err()); // non-ASCII
    }

    // -- length bounds ------------------------------------------------------

    #[test]
    fn rejects_too_short() {
        assert!(normalize("ab").is_err());
        assert!(normalize("a").is_err());
    }

    #[test]
    fn rejects_too_long() {
        assert!(normalize(&"a".repeat(33)).is_err());
    }
}
