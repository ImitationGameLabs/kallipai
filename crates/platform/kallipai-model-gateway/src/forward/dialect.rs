//! The per-family forwarding dialect: the one place that knows how each
//! upstream wire family shapes URLs, credential headers, and hand-built
//! error envelopes.
//!
//! The gateway is wire-agnostic between resolution and the upstream call:
//! a request arrives at a generation endpoint, resolves to a profile, and
//! the profile's `family` picks the dialect that finishes the request --
//! which wire path the endpoint prefix gets, how the credential stamps
//! itself, and which wire vocabulary the gateway's own errors speak.
//! Everything else (the visibility contract, the raw body passthrough)
//! is family-independent and lives in the caller.
//!
//! The family strings are the just-agent-libs spelling (`just_llm_client`
//! dispatches on exactly these); a dev-dependency test pins this module's
//! constants to that crate's `family` module, so a rename upstream breaks
//! the gateway's tests instead of silently orphaning profiles.

/// The `just_llm_client` family names this gateway can drive.
pub(crate) const DEEPSEEK: &str = "deepseek";
pub(crate) const OPENAI_COMPATIBLE: &str = "openai-compatible";
pub(crate) const OPENAI_RESPONSES: &str = "openai-responses";
pub(crate) const ANTHROPIC: &str = "anthropic";

/// The wire families the registry admits (the management face's
/// validation set): every family a backend in the client stack can
/// dispatch. A profile outside this list must not enter the registry --
/// no backend could serve it.
pub(crate) const WIRE_FAMILIES: [&str; 4] =
    [DEEPSEEK, OPENAI_COMPATIBLE, OPENAI_RESPONSES, ANTHROPIC];

/// A generation endpoint the forwarding face exposes, and the wire
/// families it can serve: `/chat/completions` speaks the chat
/// completions wire, `/responses` the responses wire, and
/// `/messages` the anthropic wire. Entry paths carry no `/v1` prefix:
/// that segment belongs to the platform edge, consumed before the
/// gateway sees the request (the wire_path below keeps the `/v1`
/// the anthropic upstream convention requires). A profile whose family
/// is not in the endpoint's set is a 400 (the family gate), never a
/// best-effort forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Endpoint {
    ChatCompletions,
    Responses,
    Messages,
}

impl Endpoint {
    /// The wire path appended to the credential's endpoint prefix. The
    /// two families disagree about the prefix by convention: the OpenAI
    /// families' prefix carries the `/v1` root and the dialect appends
    /// only the resource path; the anthropic prefix is the bare host and
    /// the dialect appends the full `/v1/messages` (an anthropic prefix
    /// carrying `/v1` produces `/v1/v1/messages` -- keep it off).
    pub(crate) fn wire_path(self) -> &'static str {
        match self {
            Endpoint::ChatCompletions => "/chat/completions",
            Endpoint::Responses => "/responses",
            Endpoint::Messages => "/v1/messages",
        }
    }

    /// The families this endpoint can serve.
    pub(crate) fn families(self) -> &'static [&'static str] {
        match self {
            Endpoint::ChatCompletions => &[DEEPSEEK, OPENAI_COMPATIBLE],
            Endpoint::Responses => &[OPENAI_RESPONSES],
            Endpoint::Messages => &[ANTHROPIC],
        }
    }
}

/// How a family's credential stamps the upstream request (the wire's
/// authentication header shape).
pub(crate) fn auth_style(family: &str) -> crate::secret::AuthStyle {
    match family {
        ANTHROPIC => crate::secret::AuthStyle::XApiKey,
        _ => crate::secret::AuthStyle::Bearer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gateway's family constants are pinned to the client stack's
    /// dispatch set: `just_llm_client` routes on exactly these strings,
    /// and a drift here would register profiles no backend can serve (or
    /// orphan profiles the registry already holds).
    #[test]
    fn family_constants_match_the_client_stack_spelling() {
        assert_eq!(DEEPSEEK, just_llm_client::family::DEEPSEEK);
        assert_eq!(
            OPENAI_COMPATIBLE,
            just_llm_client::family::OPENAI_COMPATIBLE
        );
        assert_eq!(OPENAI_RESPONSES, just_llm_client::family::OPENAI_RESPONSES);
        assert_eq!(ANTHROPIC, just_llm_client::family::ANTHROPIC);
        assert_eq!(
            WIRE_FAMILIES,
            [DEEPSEEK, OPENAI_COMPATIBLE, OPENAI_RESPONSES, ANTHROPIC]
        );
    }

    #[test]
    fn endpoints_carry_their_wire_path_and_families() {
        assert_eq!(Endpoint::ChatCompletions.wire_path(), "/chat/completions");
        assert_eq!(Endpoint::Responses.wire_path(), "/responses");
        assert_eq!(Endpoint::Messages.wire_path(), "/v1/messages");
        assert_eq!(
            Endpoint::ChatCompletions.families(),
            &["deepseek", "openai-compatible"]
        );
        assert_eq!(Endpoint::Responses.families(), &["openai-responses"]);
        assert_eq!(Endpoint::Messages.families(), &["anthropic"]);
    }

    #[test]
    fn auth_style_follows_the_family() {
        assert_eq!(auth_style(ANTHROPIC), crate::secret::AuthStyle::XApiKey);
        assert_eq!(auth_style(DEEPSEEK), crate::secret::AuthStyle::Bearer);
        assert_eq!(
            auth_style(OPENAI_RESPONSES),
            crate::secret::AuthStyle::Bearer
        );
    }
}
