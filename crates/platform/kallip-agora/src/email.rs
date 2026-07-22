//! Email normalization + validation, shared by `register_begin` and
//! `login_begin` so a user can always log in with exactly the address they
//! registered.
//!
//! Rules (applied at every write AND lookup): RFC 5321/5322 validation via the
//! `email_address` crate, restricted to a bare `local@domain` addr-spec (no
//! display-name wrapper, no domain literal), then assemble the canonical form
//! as `{local-part}@{domain-in-lowercase}`. The local part is preserved
//! verbatim (including case) per RFC 5321 sec 2.4 -- the local-part of a
//! mailbox MUST be treated as case-sensitive -- while the domain is lowercased
//! (DNS names are case-insensitive). Thus `John@Example.COM` canonicalizes to
//! `John@example.com`, and `John@x.com` / `john@x.com` are *distinct* accounts.
//!
//! The bare-addr-spec restriction is deliberate for a login id: the crate's
//! default also accepts `"Display Name <addr>"` (silently stripping the name)
//! and quoted-string local parts (`"john..doe"@x.com`, stored WITH the quotes).
//! Both would let a user register with a form they cannot reproduce at login,
//! so they are rejected here. `email_address` provides no normalization helper
//! and its parse stores the address verbatim (`Self(address.into())`), so the
//! canonical form is reassembled from the `local_part()` / `domain()` accessors.

use email_address::{EmailAddress, Options};
use kallip_common::protocol::ApiError;

/// Reject any input containing a `"`. After display-text is disabled, a `"` can
/// only come from a quoted-string local part -- RFC-valid but operationally
/// pathological as a login id (the user will not type the quotes back at login,
/// so they could not resolve). Bare addr-specs never contain a `"`.
const QUOTE: char = '"';

/// An email address that failed validation.
#[derive(Debug)]
pub struct EmailError(email_address::Error);

impl std::fmt::Display for EmailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid email address: {}", self.0)
    }
}

impl std::error::Error for EmailError {}

impl From<EmailError> for ApiError {
    fn from(e: EmailError) -> Self {
        ApiError::bad_request(e.to_string())
    }
}

/// Validate + canonicalize a raw email address: RFC check (bare addr-spec only
/// -- rejects display-name wrappers and quoted local parts), then
/// `{local-part verbatim}@{domain lowercased}`. Returns the canonical form used
/// both for storage and for lookup.
pub fn normalize(raw: &str) -> Result<String, EmailError> {
    if raw.contains(QUOTE) {
        // No dedicated crate variant for this; surface a clear bad-request.
        return Err(EmailError(email_address::Error::InvalidCharacter));
    }
    let parsed = EmailAddress::parse_with_options(raw, Options::default().without_display_text())
        .map_err(EmailError)?;
    Ok(format!(
        "{}@{}",
        parsed.local_part(),
        parsed.domain().to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn accepts_canonical_address() {
        assert_eq!(normalize("alice@example.com").unwrap(), "alice@example.com");
    }

    #[test]
    fn lowercases_domain_only() {
        // Domain folds to lowercase; local part is preserved verbatim.
        assert_eq!(normalize("John@Example.COM").unwrap(), "John@example.com");
    }

    #[test]
    fn local_part_case_is_significant() {
        // RFC 5321 sec 2.4: local-part is case-sensitive. These are distinct
        // accounts and must NOT canonicalize to the same string.
        assert_ne!(
            normalize("John@x.com").unwrap(),
            normalize("john@x.com").unwrap()
        );
        assert_eq!(normalize("John@x.com").unwrap(), "John@x.com");
    }

    #[test]
    fn rejects_missing_at() {
        assert!(normalize("not-an-email").is_err());
    }

    #[test]
    fn rejects_empty_local_part() {
        assert!(normalize("@example.com").is_err());
    }

    #[test]
    fn rejects_interior_space() {
        assert!(normalize("a @b.com").is_err());
        assert!(normalize("a@b .com").is_err());
    }

    #[test]
    fn rejects_too_long() {
        // RFC 5321 caps an address at 254 octets.
        let local = "a".repeat(300);
        assert!(normalize(&format!("{local}@x.com")).is_err());
    }

    #[test]
    fn accepts_plus_addressing_and_dots() {
        assert_eq!(
            normalize("user.name+tag@example.org").unwrap(),
            "user.name+tag@example.org"
        );
    }

    #[test]
    fn rejects_display_name_wrapper() {
        // A bare login id only; display-name wrappers would silently strip the
        // name and let distinct inputs collide on the same login id.
        assert!(normalize("Alice <a@example.com>").is_err());
        assert!(normalize("a@example.com").unwrap() == "a@example.com");
    }

    #[test]
    fn rejects_quoted_local_part() {
        // RFC-valid but operationally pathological as a login id: the user
        // would never type the quotes back at login, so they could not resolve.
        assert!(normalize("\"john..doe\"@example.com").is_err());
    }
}
