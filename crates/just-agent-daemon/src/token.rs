//! Auth-token generation, formatting, and SHA-256 hashing for in-memory comparison.
//!
//! Honest scope: the agent token's plaintext is unavoidably present in
//! [`crate::state::Agent::env`] (`JUST_AGENT_AUTH_TOKEN`) — the PTY must inject it so
//! the agent can authenticate back, so hashing the agent index is *not* primary
//! secret protection. The solid reason to hash it anyway is **consistency**: both
//! token kinds resolve through the *same* path — `TokenHash::of(incoming)` compared
//! against a stored hash — giving one uniform comparison mechanism instead of a
//! plaintext lookup for agents and a hash compare for the operator. The real
//! hardening here is a centralized 256-bit CSPRNG with an explicit entropy budget,
//! type-tagged prefixes, and constant-time operator comparison. The operator
//! plaintext is never retained (only its hash on [`crate::state::AppState`]); the
//! agent plaintext lives only in [`crate::state::Agent::env`] — everything else
//! ([`crate::state::Agent::auth_token_hash`], the registry index) holds only hashes.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Type-tag prefix for operator tokens (full-access, printed at startup).
const OPERATOR_PREFIX: &str = "sk-operator-";
/// Type-tag prefix for agent tokens (per-agent, injected into the PTY).
const AGENT_PREFIX: &str = "sk-agent-";
/// Entropy budget: 32 bytes = 256 bits.
const SECRET_BYTES: usize = 32;

/// Which kind of token to mint — selects the type-tag prefix (closed set).
#[derive(Debug, Clone, Copy)]
pub enum TokenKind {
    Operator,
    Agent,
}

/// SHA-256 of a token string. Holds a *hash*, not a secret — the only form stored
/// in long-lived daemon state. `Debug` is safe: a hash reveals nothing about the
/// token (preimage-infeasible), so logging it cannot leak the secret.
///
/// Derives `Eq`/`PartialEq`/`Hash` so it can key the agent `token_index` HashMap;
/// those structural compares are non-constant-time but operate over hashes (an
/// attacker cannot steer a SHA-256 output), so they leak nothing about any secret.
/// The single operator-secret comparison goes through [`TokenHash::ct_eq`] instead.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Hash a presented (header) or generated token string.
    pub fn of(token: &str) -> Self {
        let digest = Sha256::digest(token.as_bytes());
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }

    /// Constant-time equality, used for the single operator-secret comparison.
    /// (subtle 2.6 implements `ConstantTimeEq` for `[u8]`; the array coerces.)
    pub fn ct_eq(&self, other: &Self) -> bool {
        bool::from(self.0.ct_eq(&other.0))
    }
}

/// A freshly minted token: the plaintext secret paired with its in-memory hash,
/// bundled so they cannot drift apart. Fields are private; access via
/// [`secret`](Self::secret) and [`hash`](Self::hash). Not `Clone` — the secret
/// should not be duplicated casually.
pub struct MintedToken {
    secret: String,
    hash: TokenHash,
}

impl MintedToken {
    /// Generate a new 256-bit token of `kind` with its type-tag prefix.
    pub fn generate(kind: TokenKind) -> Self {
        let mut bytes = [0u8; SECRET_BYTES];
        // getrandom is the CSPRNG that backs `rand` and `uuid` v4; a failure here
        // means the system entropy source is unavailable, so panicking is correct.
        getrandom::fill(&mut bytes).expect("getrandom failed");
        let prefix = match kind {
            TokenKind::Operator => OPERATOR_PREFIX,
            TokenKind::Agent => AGENT_PREFIX,
        };
        let secret = format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes));
        Self {
            hash: TokenHash::of(&secret),
            secret,
        }
    }

    /// Wrap a caller-supplied secret (e.g. `JUST_AGENT_OPERATOR_TOKEN` from env).
    pub fn from_secret(secret: String) -> Self {
        Self {
            hash: TokenHash::of(&secret),
            secret,
        }
    }

    /// Plaintext secret — print it (operator) or inject into env (agent). Borrowed:
    /// mint sites read it then move the hash, so no consume-on-extract is needed.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// The in-memory comparison hash — store this, not the secret.
    pub fn hash(&self) -> &TokenHash {
        &self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_token_has_prefix_and_length() {
        let t = MintedToken::generate(TokenKind::Operator);
        let s = t.secret();
        assert!(s.starts_with(OPERATOR_PREFIX));
        // base64url NO_PAD of 32 bytes is exactly 43 chars.
        assert_eq!(s.len(), OPERATOR_PREFIX.len() + 43);
    }

    #[test]
    fn agent_token_has_prefix_and_length() {
        let t = MintedToken::generate(TokenKind::Agent);
        let s = t.secret();
        assert!(s.starts_with(AGENT_PREFIX));
        assert_eq!(s.len(), AGENT_PREFIX.len() + 43);
    }

    #[test]
    fn hash_matches_of_for_secret() {
        let t = MintedToken::from_secret("sk-operator-test".to_string());
        assert_eq!(t.hash(), &TokenHash::of("sk-operator-test"));
    }

    #[test]
    fn ct_eq_agrees_with_partial_eq() {
        let a = TokenHash::of("hello");
        let b = TokenHash::of("hello");
        let c = TokenHash::of("world");
        assert!(a.ct_eq(&b));
        assert!(!a.ct_eq(&c));
        // ct_eq must agree with derived ==.
        assert_eq!(a.ct_eq(&b), a == b);
        assert_eq!(a.ct_eq(&c), a == c);
    }

    #[test]
    fn two_generated_tokens_differ() {
        // Astronomically unlikely to collide; guards against a constant-seed bug.
        let a = MintedToken::generate(TokenKind::Agent);
        let b = MintedToken::generate(TokenKind::Agent);
        assert_ne!(a.hash(), b.hash());
        assert_ne!(a.secret(), b.secret());
    }

    #[test]
    fn kinds_never_share_prefix() {
        assert!(
            !MintedToken::generate(TokenKind::Operator)
                .secret()
                .starts_with(AGENT_PREFIX)
        );
        assert!(
            !MintedToken::generate(TokenKind::Agent)
                .secret()
                .starts_with(OPERATOR_PREFIX)
        );
    }

    #[test]
    fn empty_secret_is_hashable() {
        // Guard for the set-but-empty env-var edge case (rejected upstream in main.rs).
        let _ = TokenHash::of("");
    }
}
