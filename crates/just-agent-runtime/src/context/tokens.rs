//! Crate-private token-estimation seam.
//!
//! All token counting for context management routes through here. Backed by `tokenx-rs` — a
//! single-pass character scanner, zero dependencies, no vocabulary files. Accuracy is ~96% on
//! English prose (tokenx's benchmark vs `cl100k_base`), CJK-aware at ~1 token/char, and less
//! precise on JSON/structural text — it is an *estimate* for budget gates and compaction
//! triggers, not an exact count. This replaces the prior `chars/4` heuristic, which
//! underestimated CJK text ~4× (a Chinese character is ~1 real token, not 0.25) and so caused
//! budget/compaction gates to fire far too late for CJK-heavy agents.

use just_llm_client::types::chat::ChatMessage;

/// Estimate tokens for rendered text via `tokenx`.
pub(crate) fn estimate_text(text: &str) -> usize {
    tokenx_rs::estimate_token_count(text)
}

/// Estimate tokens for one message by rendering its wire-format JSON — the role tag, content
/// key, and tool-call structure are all in the JSON, so the structural overhead is counted
/// directly and no per-message / per-tool-call magic constants are needed.
///
/// `ChatMessage` derives `Serialize`; its JSON matches `render_messages` closely for the
/// OpenAI-compatible providers, so this cached per-message estimate stays consistent with the
/// live full-render estimate in `estimate.rs`. Deliberately separate from that path so the
/// cache needs no `ChatClient`.
pub(crate) fn estimate_message_tokens(message: &ChatMessage) -> usize {
    match serde_json::to_string(message) {
        Ok(json) => estimate_text(&json),
        // Unreachable for a derive(Serialize) enum of String/Vec/Option fields; fall back to
        // content-only if serialization ever fails so estimation never panics.
        Err(_) => estimate_text(message.content().unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_text_is_positive_for_nonempty() {
        assert!(estimate_text("hello world") > 0);
    }

    #[test]
    fn cjk_estimates_near_one_token_per_char() {
        // tokenx scores CJK at 1 token/char (vs char/4's 0.25) — the core reason for the swap.
        let cjk = estimate_text("你好世界测试"); // 6 CJK chars
        assert!(cjk >= 6, "CJK should estimate ~1 token/char: got {cjk}");
        // And CJK estimates higher than equal-length Latin, which packs into fewer word-tokens.
        let latin = estimate_text("abcdef"); // 6 latin chars → ~1 word-token
        assert!(
            cjk > latin,
            "CJK ({cjk}) should exceed same-length Latin ({latin})"
        );
    }

    #[test]
    fn estimate_message_tokens_includes_envelope() {
        // A bare content string estimates fewer tokens than the same content wrapped in a
        // message (the JSON envelope adds the role/structure tokens).
        let content = "hello world";
        let bare = estimate_text(content);
        let wrapped = estimate_message_tokens(&ChatMessage::user(content));
        assert!(
            wrapped > bare,
            "message estimate ({wrapped}) should exceed bare content ({bare})"
        );
    }
}
