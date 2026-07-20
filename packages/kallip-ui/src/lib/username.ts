// Username shape validation for the register page -- immediate UX feedback
// only, NOT the authority.
//
// Mirrors `crates/kallip-agora/src/username.rs::normalize` (GitHub-aligned):
// trim + ASCII-lowercase, 3-32 chars of `[a-z0-9-]` where hyphens are single
// and interior (no leading/trailing/consecutive), no underscores. The server
// is authoritative and re-normalizes, so this is purely a gate to avoid a
// 400 round-trip.
//
// Unlike `lib/email.ts` -- where the helper MUST NOT canonicalize because
// `email.rs` treats the local part as case-sensitive -- the username IS
// case-insensitive (the server ASCII-lowercases it), so lowercasing here is
// correct and matches what the server will store.

/** Alphanumeric runs separated by single interior hyphens. */
const USERNAME_RE = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;

const USERNAME_MIN_LEN = 3;
const USERNAME_MAX_LEN = 32;

/**
 * True if `raw` normalizes to a valid GitHub-style handle. Trims and
 * ASCII-lowercases (matching the server), then checks length 3-32 BEFORE the
 * shape regex: the regex alone accepts `"a"`/`"ab"`, so the length gate is
 * load-bearing, not redundant.
 */
export function isValidUsername(raw: string): boolean {
  const trimmed = raw.trim().toLowerCase();
  if (trimmed.length < USERNAME_MIN_LEN || trimmed.length > USERNAME_MAX_LEN) {
    return false;
  }
  return USERNAME_RE.test(trimmed);
}
