//! Rendering LLM errors into human-readable strings.
//!
//! Every site that turns an LLM error into a string for a human (the terminal
//! user-facing error, the failover reason, retry/stream-reset events, retry log
//! records) goes through [`render_error`]. It does two things the naive
//! `{e:#}` / `format!("{e}")` formatters get wrong:
//!
//! - **De-duplicates the `source()` chain.** The wrapping error types we consume
//!   (`BackendError::Provider`, `ProviderError::Transport`, `RequestFailure::Fatal`)
//!   interpolate their immediate source into their own `Display`, so walking the
//!   chain with `{:#}` prints each layer twice. [`dedup_chain`] skips a layer whose
//!   `Display` is already a suffix of the accumulated message.
//!
//! - **Surfaces the swallowed HTTP error body.** `TransportError::HttpStatus`
//!   captures the full response body but its `#[error("api returned {status}")]`
//!   omits it from `Display`. That body is where the API's actual reason lives
//!   (`context_length_exceeded`, a role-ordering rejection, ...). [`extract_http_body`]
//!   recovers it by walking the chain and downcasting.

/// Cap on how much of an HTTP error body we render inline. The upstream transport
/// caps error bodies at 8 MiB, so a raw fallback can be enormous; truncating keeps
/// the user-facing message (and daemon log line) readable.
const BODY_DISPLAY_LIMIT: usize = 512;

/// Render an LLM error into a single, human-readable string: the full `source()`
/// chain (de-duplicated) plus the captured HTTP response body when one is present.
pub(crate) fn render_error(e: &(dyn std::error::Error + 'static)) -> String {
    let chain = dedup_chain(e);
    match extract_http_body(e) {
        Some(body) => format!("{chain}: {}", format_body(&body)),
        None => chain,
    }
}

/// Walk `source()` joining each level with `": "`, but skip a level whose `Display`
/// is already a suffix of the accumulated message.
///
/// Why a walker at all: a plain `{e}` renders only the top level; for a deep
/// `reqwest::Error` the actionable root cause (hyper/h2/io) lives in the `source()`
/// chain and must be surfaced explicitly. (Rationale migrated from the former
/// `error_chain` doc — do not lose it.)
///
/// INVARIANT this relies on: every wrapper in the chain formats as `"<prefix>:
/// {source}"` with `source` last, so a wrapper that already inlined its source
/// leaves that source as an exact suffix of the message and `ends_with` skips it.
/// This is the natural thiserror pattern and holds for all current types
/// (`BackendError::Provider`, `ProviderError::Transport`, `RequestFailure::Fatal`).
/// A wrapper that appended a trailing suffix AFTER its source (e.g.
/// `"transport error: {0} (retryable)"`) would defeat the dedup and reintroduce
/// duplication — such a shape would also break anyhow `{:#}`, so it is not expected.
fn dedup_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut msg = e.to_string();
    let mut current = e.source();
    while let Some(source) = current {
        let layer = source.to_string();
        if !layer.is_empty() && !msg.ends_with(layer.as_str()) {
            msg.push_str(": ");
            msg.push_str(&layer);
        }
        current = source.source();
    }
    msg
}

/// Walk the source chain and downcast to [`just_llm_client::TransportError`],
/// returning the `HttpStatus.body` when one is present and non-empty.
///
/// Mirrors the pattern proven by just-common's own
/// `transport_error_reachable_via_source_chain` test, but carries it through the
/// `BackendError::Provider { source: BoxError }` boxing layer that just-common's
/// test does not exercise: each `source()` returns the concrete inner error behind
/// a trait object, so `downcast_ref` still resolves the concrete `TransportError`.
fn extract_http_body(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(error) = current {
        if let Some(transport) = error.downcast_ref::<just_llm_client::TransportError>()
            && let just_llm_client::TransportError::HttpStatus { body, .. } = transport
            && !body.is_empty()
        {
            return Some(body.clone());
        }
        current = error.source();
    }
    None
}

/// Reduce a raw HTTP error body to a short, single-line, human-readable diagnostic.
///
/// Prefers `error.message` (then top-level `message`) from an OpenAI-shaped JSON
/// body — GLM and most OpenAI-compatible providers use that shape; falls back to the
/// raw text otherwise (HTML gateway pages, plain text). Whitespace runs collapse to a
/// single space and the result truncates to [`BODY_DISPLAY_LIMIT`].
///
/// Parse before truncating: truncating first would split valid JSON mid-token and
/// silently degrade a structured body to the raw fallback, losing the `error.message`.
fn format_body(raw: &str) -> String {
    let extracted = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .or_else(|| value.get("message").and_then(|message| message.as_str()))
                .map(str::to_owned)
        });
    let text = extracted.unwrap_or_else(|| raw.to_owned());
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&single_line)
}

/// Truncate to [`BODY_DISPLAY_LIMIT`] characters, marking a truncation with an
/// ellipsis. Counts by `char` (not bytes) so it never splits a multi-byte sequence.
fn truncate(text: &str) -> String {
    if text.chars().count() <= BODY_DISPLAY_LIMIT {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(BODY_DISPLAY_LIMIT).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use just_llm_client::{BackendError, ProviderError, TransportError};

    /// Build the exact production chain that reaches `agent_task`'s terminal error
    /// arm: `runner` strips `RequestFailure::Fatal(BackendError)` and converts the
    /// inner `BackendError` to `anyhow::Error` (runner.rs `Fatal(e) => ... e.into()`),
    /// so the head reaching `render_error` is the `BackendError`, not `RequestFailure`.
    fn fatal_chain(body: &str) -> anyhow::Error {
        let transport = TransportError::HttpStatus {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: body.to_owned(),
        };
        let provider = ProviderError::Transport(transport);
        let backend = BackendError::provider("openai-compatible", provider);
        backend.into()
    }

    #[test]
    fn render_error_dedups_and_surfaces_body() {
        let body = r#"{"error":{"message":"This model's maximum context length is 128000 tokens","type":"context_length_exceeded"}}"#;
        let rendered = render_error(fatal_chain(body).as_ref());

        // The duplicated middle layers appear exactly once.
        assert_eq!(
            rendered.matches("api returned 400 Bad Request").count(),
            1,
            "no duplication: {rendered}"
        );
        assert_eq!(
            rendered.matches("transport error:").count(),
            1,
            "no duplication: {rendered}"
        );
        // The BackendError head prefix is retained (RequestFailure is stripped in
        // production before the error reaches `render_error`).
        assert!(
            rendered.starts_with("openai-compatible backend error:"),
            "missing head prefix: {rendered}"
        );
        // The body's structured message surfaces, without JSON noise.
        assert!(
            rendered.contains("maximum context length is 128000 tokens"),
            "body message not surfaced: {rendered}"
        );
        assert!(
            !rendered.contains('"'),
            "raw JSON leaked into output: {rendered}"
        );
    }

    #[test]
    fn render_error_without_body_is_just_the_chain() {
        // An error with no HttpStatus in its chain renders as the plain chain.
        let rendered = render_error(&std::io::Error::other("boom"));
        assert_eq!(rendered, "boom");
    }

    #[test]
    fn dedup_chain_appends_non_inlined_sources() {
        // A wrapper whose Display does NOT interpolate its source must still surface it.
        #[derive(thiserror::Error, Debug)]
        #[error("outer")]
        struct Outer(#[source] Inner);
        #[derive(thiserror::Error, Debug)]
        #[error("inner")]
        struct Inner;

        assert_eq!(dedup_chain(&Outer(Inner)), "outer: inner");
    }

    #[test]
    fn extract_http_body_reaches_through_boxing() {
        let body = r#"{"error":{"message":"no"}}"#;
        assert_eq!(
            extract_http_body(fatal_chain(body).as_ref()).as_deref(),
            Some(body)
        );
    }

    #[test]
    fn format_body_prefers_error_message() {
        let body = r#"{"error":{"message":"ctx too long","type":"context_length_exceeded"}}"#;
        assert_eq!(format_body(body), "ctx too long");
    }

    #[test]
    fn format_body_falls_back_to_top_level_message() {
        assert_eq!(format_body(r#"{"message":"plain"}"#), "plain");
    }

    #[test]
    fn format_body_falls_back_to_raw_single_lined() {
        assert_eq!(
            format_body("<html>\n  bad gateway\n</html>"),
            "<html> bad gateway </html>"
        );
    }

    #[test]
    fn format_body_parses_before_truncating() {
        // A large but valid JSON body must still yield its structured message,
        // truncated only after extraction (not before parsing).
        let long = "a".repeat(1024);
        let body = format!(r#"{{"error":{{"message":"{long}"}}}}"#);
        let rendered = format_body(&body);

        assert_eq!(rendered.chars().count(), BODY_DISPLAY_LIMIT + 1); // + ellipsis
        assert!(rendered.ends_with('\u{2026}'));
        assert!(rendered.starts_with(&"a".repeat(BODY_DISPLAY_LIMIT)));
    }
}
