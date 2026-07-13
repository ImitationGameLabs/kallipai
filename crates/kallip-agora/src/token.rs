//! Agora-specific auth-token prefixes, built on the shared
//! [`kallip_common::authtoken`] core. Import `MintedToken`/`TokenHash` directly
//! from `kallip_common::authtoken`.

use kallip_common::authtoken::TokenKind;

/// Admin token — authorizes control-plane provisioning (minting users +
/// enrollment codes). Plaintext printed once at startup (or set via
/// `KALLIP_AGORA_ADMIN_TOKEN`); only its hash is retained.
pub const ADMIN: TokenKind = TokenKind("sk-admin-");

/// User access token — carried by the app as a bearer token. Hash-indexed.
pub const USER: TokenKind = TokenKind("sk-user-");

/// Long-lived team token — held by a `kallip-herald` to reopen its tunnel.
/// Hash-indexed.
pub const TEAM: TokenKind = TokenKind("sk-team-");

/// Single-use, short-TTL enrollment code — exchanged at `POST /v1/teams` for a
/// team token. Hash-indexed; consumed on first use.
pub const ENROLLMENT: TokenKind = TokenKind("sk-enroll-");
