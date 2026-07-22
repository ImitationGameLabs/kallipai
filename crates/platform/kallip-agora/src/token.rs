//! Agora-specific auth-token prefixes, built on the shared
//! [`kallip_common::authtoken`] core. Import `MintedToken`/`TokenHash` directly
//! from `kallip_common::authtoken`.

use kallip_common::authtoken::TokenKind;

/// Admin token — authorizes control-plane provisioning (minting invite codes).
/// Plaintext printed once at startup (or set via `KALLIP_AGORA_ADMIN_TOKEN`);
/// only its hash is retained.
pub const ADMIN: TokenKind = TokenKind("sk-admin-");

/// Long-lived tagma token — held by a `kallip-herald` to reopen its tunnel.
/// Hash-indexed.
pub const TAGMA: TokenKind = TokenKind("sk-tagma-");

/// Single-use, short-TTL enrollment token — minted by a user (self-service) and
/// exchanged at `POST /v1/tagmata` for a tagma token. Hash-indexed; consumed on
/// first use.
pub const ENROLLMENT: TokenKind = TokenKind("sk-enroll-");

/// Single-use invite code — admin-minted, redeemed at `POST /v1/auth/register`
/// to create a user account + bind a passkey. Hash-indexed; consumed on first
/// use.
pub const INVITE: TokenKind = TokenKind("sk-invite-");

/// Opaque session cookie value (random, never a bearer). Only its SHA-256 hash
/// is stored in the `sessions` table; the plaintext rides the
/// `kallip_session` cookie.
pub const SESSION: TokenKind = TokenKind("sk-sess-");
