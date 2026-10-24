//! How an id keeps its shape and how it enters a request path.
//!
//! Both questions belong to the id types this crate defines, so both answers
//! live here: `canonical_uuid` is the shape the platform mints, and
//! [`path_segment`] is the one rule for putting an id inside a path. Every
//! client that addresses a lesche, a tagma, or a room through a URL uses this
//! rule instead of pasting the raw value into a format string.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// The bytes an id may carry into a request path segment: letters, digits, and
/// the hyphen a UUID spells itself with. Everything else is percent-encoded, so
/// no value can introduce a separator, a query opener, or a dot segment.
const ID_BYTES: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-');

/// One id placed in a request path segment.
///
/// What a request addresses is decided by its path, so only the bytes an id is
/// made of pass through: a value carrying a separator, a query opener, or the
/// `.` of a dot segment would otherwise address another resource, and could
/// reach a route with a different authorization rule. The alphabet is the
/// judgement rather than a list of known-bad bytes, so no byte outside it can
/// reshape the path, and every id the platform derives -- a plain UUID -- keeps
/// its exact form.
pub fn path_segment(raw: &str) -> String {
    utf8_percent_encode(raw, ID_BYTES).to_string()
}

/// The canonical spelling of a UUID id, or `None` when `raw` is not one.
///
/// The platform mints ids as lowercase hyphenated UUIDs, so a value that
/// parses but spells itself differently is not one of ours: accepting it would
/// give the same id two spellings, and a lookup keyed on the string would miss.
pub fn canonical_uuid(raw: &str) -> Option<String> {
    let parsed = uuid::Uuid::parse_str(raw).ok()?;
    let canonical = parsed.hyphenated().to_string();
    (canonical == raw).then_some(canonical)
}

#[cfg(test)]
mod tests {
    use super::{canonical_uuid, path_segment};

    /// An id keeps its own bytes in the path, and nothing else does: a canonical
    /// id rides unchanged, while separators, openers, and dot segments are
    /// escaped so the value stays inside its one segment.
    #[test]
    fn a_path_segment_keeps_its_value_inside_its_segment() {
        assert_eq!(
            path_segment("00000000-0000-4000-8000-000000000ee5"),
            "00000000-0000-4000-8000-000000000ee5"
        );
        // A dot segment is the one value that changes the path without any
        // separator in it: `..` must never come back bare.
        assert_eq!(path_segment(".."), "%2E%2E");
        assert_eq!(path_segment("."), "%2E");
        assert_eq!(path_segment("../../agents"), "%2E%2E%2F%2E%2E%2Fagents");
        assert_eq!(path_segment("a?x=1#f"), "a%3Fx%3D1%23f");
        assert_eq!(path_segment("100%"), "100%25");
    }

    /// A canonical UUID is an id; every other spelling of one is not, because
    /// the same id spelled two ways would key two rows.
    #[test]
    fn only_the_canonical_spelling_of_a_uuid_is_an_id() {
        let canonical = "00000000-0000-4000-8000-000000000ee5";
        assert_eq!(canonical_uuid(canonical).as_deref(), Some(canonical));
        assert_eq!(canonical_uuid("00000000-0000-4000-8000-000000000EE5"), None);
        assert_eq!(canonical_uuid("00000000000040008000000000000ee5"), None);
        assert_eq!(
            canonical_uuid("{00000000-0000-4000-8000-000000000ee5}"),
            None
        );
        assert_eq!(canonical_uuid("../../agents"), None);
        assert_eq!(canonical_uuid(""), None);
    }
}
