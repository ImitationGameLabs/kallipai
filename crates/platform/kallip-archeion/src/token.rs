//! Archeion-specific auth-token prefixes, built on the shared
//! [`kallip_common::authtoken`] core. Import `MintedToken`/`TokenHash` directly
//! from `kallip_common::authtoken`.

use kallip_common::authtoken::TokenKind;

/// Admin token — authorizes control-plane provisioning (enrollment codes,
/// user/passkey management). Operator-pinned or generated to a 0600
/// runtime file; the plaintext is never logged and only the hash retained.
pub const ADMIN: TokenKind = TokenKind("sk-admin-");

/// Platform-internal shared secret — the archeion provisions it into its
/// state directory and consumers read it by file location. The value only
/// has to agree across the four platform services; it never leaves the host.
pub const INTERNAL: TokenKind = TokenKind("sk-internal-");

/// Long-lived tagma token — held by the tagma's in-process relay connector to
/// reopen its tunnel.
/// Hash-indexed.
pub const TAGMA: TokenKind = TokenKind("sk-tagma-");

/// Single-use, short-TTL enrollment token — minted by a user (self-service) and
/// exchanged at `POST /tagmata` for a tagma token. Hash-indexed; consumed on
/// first use.
pub const ENROLLMENT: TokenKind = TokenKind("sk-enroll-");

/// Opaque session cookie value (random, never a bearer). Only its SHA-256 hash
/// is stored in the `sessions` table; the plaintext rides the
/// `kallip_session` cookie.
pub const SESSION: TokenKind = TokenKind("sk-sess-");

/// Single-use email-verification token -- the plaintext rides a verification
/// link delivered to the address; only its SHA-256 hash is stored on the
/// `emails` row, and it is cleared once the address is verified.
pub const EMAIL_VERIFY: TokenKind = TokenKind("sk-email-");

/// Single-use, short-TTL OAuth signup token -- minted at finish when an unlinked
/// OAuth identity is resolved; its SHA-256 hash rides the held `oauth_states`
/// row, the plaintext is returned to the SPA, and it is consumed at signup
/// completion (`POST /auth/oauth/signup/complete`).
pub const OAUTH_SIGNUP: TokenKind = TokenKind("sk-oauthsu-");
