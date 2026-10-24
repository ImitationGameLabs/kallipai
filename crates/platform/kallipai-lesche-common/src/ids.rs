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
/// nothing outside the set reaches the path, and the spellings encoding cannot
/// disarm -- the empty value, `.` and `..` -- are refused before it runs.
const ID_BYTES: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-');

/// Why a value cannot stand as one request path segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSegmentError {
    /// Nothing to address: an empty segment collapses into a bare separator.
    Empty,
    /// `.` or `..`. Percent-encoding cannot disarm either one, because a URL
    /// parser resolves both spellings after it decodes, so `%2E%2E` still walks
    /// up a level. Only refusing the value keeps it inside one segment.
    DotSegment,
}

impl std::fmt::Display for PathSegmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("an id cannot be empty"),
            Self::DotSegment => f.write_str("an id cannot be a dot segment"),
        }
    }
}

impl std::error::Error for PathSegmentError {}

/// Whether `raw` can stand as one request path segment, judged without
/// encoding: the rule a caller applies where the value enters.
pub fn check_segment(raw: &str) -> Result<(), PathSegmentError> {
    if raw.is_empty() {
        return Err(PathSegmentError::Empty);
    }
    if raw == "." || raw == ".." {
        return Err(PathSegmentError::DotSegment);
    }
    Ok(())
}

/// One id placed in a request path segment.
///
/// What a request addresses is decided by its path, so only the bytes an id is
/// made of pass through: a value carrying a separator or a query opener would
/// otherwise address another resource, and could reach a route with a different
/// authorization rule. The alphabet is the judgement rather than a list of
/// known-bad bytes, so no byte outside it can reshape the path. Encoding cannot
/// hold a dot segment inside its segment, so a value that is one is refused,
/// as is an empty one; every id the platform derives -- a plain UUID -- keeps
/// its exact form and does not fail.
pub fn path_segment(raw: &str) -> Result<String, PathSegmentError> {
    check_segment(raw)?;
    Ok(utf8_percent_encode(raw, ID_BYTES).to_string())
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
    use super::{PathSegmentError, canonical_uuid, check_segment, path_segment};

    /// An id keeps its own bytes in the path, and nothing else does: a canonical
    /// id rides unchanged, while separators and openers are escaped so the value
    /// stays inside its one segment.
    #[test]
    fn a_path_segment_keeps_its_value_inside_its_segment() {
        let id = "00000000-0000-4000-8000-000000000ee5";
        assert_eq!(path_segment(id).as_deref(), Ok(id));
        assert_eq!(
            path_segment("../../agents").as_deref(),
            Ok("%2E%2E%2F%2E%2E%2Fagents")
        );
        assert_eq!(path_segment("a?x=1#f").as_deref(), Ok("a%3Fx%3D1%23f"));
        assert_eq!(path_segment("100%").as_deref(), Ok("100%25"));
        // A value that merely carries dots is still one segment.
        assert_eq!(path_segment("a..b").as_deref(), Ok("a%2E%2Eb"));
    }

    /// A dot segment survives decoding, so escaping cannot hold it inside its
    /// segment: it is refused, as is the empty value that would collapse into a
    /// bare separator.
    #[test]
    fn a_dot_segment_is_refused_rather_than_escaped() {
        assert_eq!(path_segment(".."), Err(PathSegmentError::DotSegment));
        assert_eq!(path_segment("."), Err(PathSegmentError::DotSegment));
        assert_eq!(path_segment(""), Err(PathSegmentError::Empty));
        assert_eq!(check_segment(".."), Err(PathSegmentError::DotSegment));
        assert_eq!(check_segment(""), Err(PathSegmentError::Empty));
        assert_eq!(check_segment("a.b"), Ok(()));
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
