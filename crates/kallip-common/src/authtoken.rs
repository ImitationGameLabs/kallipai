//! Generic auth-token minting and hashing: a 256-bit CSPRNG secret with a
//! type-tag prefix, retained at rest only as a SHA-256 hash.
//!
//! This is the shared core used by every kallip component that mints bearer
//! tokens (`kallip-daemon`'s operator/agent tokens, `kallip-agora`'s
//! user/team/enrollment tokens). Each crate defines its own closed set of
//! [`TokenKind`] prefixes as `const`s; this module is purpose-agnostic and holds
//! no component-specific enum.
//!
//! Honest scope: hashing is as much for *consistency* — one uniform
//! [`TokenHash::of`] comparison path across every token kind — as for secrecy.
//! The real hardening here is a centralized 256-bit entropy budget, type-tagged
//! prefixes, and constant-time comparison for the single high-value secret.
//! Plaintext is handed out exactly once (to be printed or injected into env) and
//! never retained; long-lived state holds only [`TokenHash`].

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Entropy budget: 32 bytes = 256 bits.
const SECRET_BYTES: usize = 32;

/// A token's type-tag prefix (e.g. `sk-operator-`, `sk-user-`). Each crate
/// defines its own `const` kinds so the closed set of purposes lives where it is
/// used, keeping this module free of any component-specific enum.
#[derive(Debug, Clone, Copy)]
pub struct TokenKind(pub &'static str);

/// SHA-256 of a token string. Holds a *hash*, not a secret — the only form to
/// retain in long-lived state. `Debug` is safe: a hash reveals nothing about the
/// token (preimage-infeasible), so logging it cannot leak the secret.
///
/// Derives `Eq`/`PartialEq`/`Hash` so it can key a HashMap index; those
/// structural compares are non-constant-time but operate over hashes (an
/// attacker cannot steer a SHA-256 output), so they leak nothing about any
/// secret. The single high-value-secret comparison goes through [`TokenHash::ct_eq`].
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

    /// Constant-time equality, used for the single high-value secret comparison.
    /// (subtle implements `ConstantTimeEq` for `[u8]`; the array coerces.)
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
        // getrandom is the CSPRNG that backs `rand` and `uuid` v4; a failure
        // means the system entropy source is unavailable, so panicking is correct.
        getrandom::fill(&mut bytes).expect("getrandom failed");
        let secret = format!("{}{}", kind.0, URL_SAFE_NO_PAD.encode(bytes));
        Self {
            hash: TokenHash::of(&secret),
            secret,
        }
    }

    /// Wrap a caller-supplied secret (e.g. an operator token loaded from env).
    pub fn from_secret(secret: String) -> Self {
        Self {
            hash: TokenHash::of(&secret),
            secret,
        }
    }

    /// Plaintext secret — print it or inject into env. Borrowed: mint sites read
    /// it then move the hash, so no consume-on-extract is needed.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// The comparison hash — store this, not the secret.
    pub fn hash(&self) -> &TokenHash {
        &self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KIND: TokenKind = TokenKind("sk-test-");

    #[test]
    fn generated_token_has_prefix_and_length() {
        let t = MintedToken::generate(TEST_KIND);
        let s = t.secret();
        assert!(s.starts_with("sk-test-"));
        // base64url NO_PAD of 32 bytes is exactly 43 chars.
        assert_eq!(s.len(), "sk-test-".len() + 43);
    }

    #[test]
    fn hash_matches_of_for_secret() {
        let t = MintedToken::from_secret("sk-test-sample".to_string());
        assert_eq!(t.hash(), &TokenHash::of("sk-test-sample"));
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
        let a = MintedToken::generate(TEST_KIND);
        let b = MintedToken::generate(TEST_KIND);
        assert_ne!(a.hash(), b.hash());
        assert_ne!(a.secret(), b.secret());
    }

    #[test]
    fn distinct_kinds_have_distinct_prefixes() {
        let other = TokenKind("sk-other-");
        assert!(
            !MintedToken::generate(TEST_KIND)
                .secret()
                .starts_with(other.0)
        );
    }

    #[test]
    fn empty_secret_is_hashable() {
        // Guard for the set-but-empty env-var edge case (rejected upstream).
        let _ = TokenHash::of("");
    }
}
