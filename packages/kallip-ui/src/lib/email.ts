// Light email shape validation for the auth pages -- immediate UX feedback
// only, NOT canonicalization.
//
// The agora is authoritative: `crates/platform/kallip-agora/src/email.rs::normalize`
// runs RFC 5321 validation via the `email_address` crate and produces the
// canonical form `{local-part verbatim}@{domain lowercased}`. The local part
// is case-sensitive (RFC 5321 sec 2.4 -- `John@x.com` and `john@x.com` are
// DISTINCT accounts, `email.rs:83-91`), so this helper MUST NOT lowercase,
// trim the domain, or otherwise mutate the address -- a user must be able to
// type exactly the address they registered. We only return a boolean.

/**
 * Permissive addr-spec shape. Intentionally MORE permissive than
 * `email.rs`: false positives are fine (the server rejects), false negatives
 * are not (the user couldn't type an address the server would accept).
 */
const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

/** RFC 5321 cap on total address length (octets). Mirrors `email.rs:110-114`. */
const EMAIL_MAX_LEN = 254;

/**
 * True if `raw` looks like a deliverable email address. Trims surrounding
 * whitespace (the server does too), then checks both the shape
 * ([`EMAIL_RE`]) and the RFC 5321 length cap. Does NOT canonicalize or
 * mutate the input beyond the trim.
 */
export function isValidEmail(raw: string): boolean {
  const trimmed = raw.trim();
  if (trimmed.length === 0 || trimmed.length > EMAIL_MAX_LEN) return false;
  return EMAIL_RE.test(trimmed);
}
