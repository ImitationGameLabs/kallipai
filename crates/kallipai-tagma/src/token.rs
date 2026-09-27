//! Tagma-specific auth-token prefixes (operator/agent), built on the shared
//! [`kallipai_common::authtoken`] core.
//!
//! The CSPRNG minting, SHA-256 hashing, and constant-time comparison live in
//! [`kallipai_common::authtoken`]; this module pins only this crate's two
//! type-tag prefixes. Import the shared `MintedToken`/`TokenHash` types
//! directly from [`kallipai_common::authtoken`].

use kallipai_common::authtoken::TokenKind;

/// Full-access operator token. Minted tokens are persisted 0600 to the
/// credentials dir at startup; only the hash is retained on [`crate::state::AppState`].
pub const OPERATOR: TokenKind = TokenKind("sk-operator-");

/// Per-agent token, injected into the agent shell as `KALLIPAI_AUTH_TOKEN` and
/// indexed by hash in the registry.
pub const AGENT: TokenKind = TokenKind("sk-agent-");

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_common::authtoken::MintedToken;

    #[test]
    fn operator_and_agent_prefixes_are_distinct() {
        assert_ne!(OPERATOR.0, AGENT.0);
        assert!(
            MintedToken::generate(OPERATOR)
                .secret()
                .starts_with(OPERATOR.0)
        );
        assert!(MintedToken::generate(AGENT).secret().starts_with(AGENT.0));
    }
}
