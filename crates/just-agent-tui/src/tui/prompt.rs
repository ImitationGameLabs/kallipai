//! Outgoing prompt preparation: token-gated spill of oversized merged prompts to
//! a temp file, replacing the message with a read instruction.
//!
//! When the user queues several inputs while the agent is busy, the TUI merges
//! them into one prompt. A merged prompt that exceeds [`PROMPT_TOKEN_THRESHOLD`]
//! tokens is written to a temp file (the agent's bash tool reads `/tmp` under
//! `ReadPolicy::Broad`), and the sent message becomes an instruction telling the
//! agent where to find it and how big it is, so it can pick a suitable read tool.
//!
//! [`Outgoing`] carries three views of the same logical message:
//! - `raw`     the original merged text (used to re-stash on send failure);
//! - `display` what the TUI renders as the user's line;
//! - `payload` what is POSTed to the daemon.
//!
//! For under-threshold prompts all three are identical; for spilled prompts
//! `display == payload` is the read instruction while `raw` keeps the content.

use std::io::Write;

use tempfile::NamedTempFile;

/// Merged prompts above this estimated token count are spilled to a temp file.
///
/// `tokenx_rs` is an estimator (CJK ~1 token/char, less precise on code), not an
/// exact count; this gate only exists to keep a single gigantic message from
/// being inlined. The daemon's own `enforce_pre_call_budget` enforces the real
/// token budget, so this is a coarse pre-filter, not a precise limit.
const PROMPT_TOKEN_THRESHOLD: usize = 10_000;

/// A prompt about to be sent, in its three render/send/raw forms (see module docs).
pub(crate) struct Outgoing {
    /// Original merged text; used to re-stash into `pending` on send failure.
    pub(crate) raw: String,
    /// Text rendered as the user's chat line.
    pub(crate) display: String,
    /// Text POSTed to the daemon.
    pub(crate) payload: String,
}

impl Outgoing {
    /// Build an inline (non-spilled) outgoing: all three views are the text itself.
    fn inline(text: String) -> Self {
        Self {
            raw: text.clone(),
            display: text.clone(),
            payload: text,
        }
    }

    /// Log a warning if preparation took unexpectedly long.
    fn watch(this: Self, started: std::time::Instant) -> Self {
        let elapsed = started.elapsed();
        if elapsed > std::time::Duration::from_millis(5) {
            tracing::warn!("prepare_outgoing took {}ms", elapsed.as_millis());
        }
        this
    }
}

/// Prepare a merged prompt for sending, spilling to a temp file when it exceeds
/// the token threshold.
///
/// Runs synchronous file I/O. On tmpfs this is sub-millisecond for normal sizes;
/// a watchdog logs a warning if it ever exceeds 5ms (only a pathological paste
/// could). Kept synchronous on purpose: asyncifying would require stalling the
/// render loop on a join handle and would race on where the user line lands.
pub(crate) fn prepare_outgoing(merged: &str) -> Outgoing {
    let started = std::time::Instant::now();

    let tokens = tokenx_rs::estimate_token_count(merged);
    if tokens <= PROMPT_TOKEN_THRESHOLD {
        return Outgoing::watch(Outgoing::inline(merged.to_owned()), started);
    }

    match spill_to_file(merged, tokens) {
        Some(instruction) => Outgoing::watch(
            Outgoing {
                raw: merged.to_owned(),
                display: instruction.clone(),
                payload: instruction,
            },
            started,
        ),
        // Spill failed (disk error / keep() failed): fall back to inline so the
        // message is still delivered, and log loudly.
        None => {
            tracing::warn!("prompt spill failed; sending {} tokens inline", tokens);
            Outgoing::watch(Outgoing::inline(merged.to_owned()), started)
        }
    }
}

/// Write `content` to a persisted temp file and return the read instruction that
/// replaces the message. Returns `None` if the file cannot be created/persisted.
fn spill_to_file(content: &str, tokens: usize) -> Option<String> {
    let mut tmp = NamedTempFile::new_in(std::env::temp_dir()).ok()?;
    // Header makes the file self-describing if the agent inspects it directly.
    let body = format!("# Spilled user message (large)\n{content}");
    tmp.write_all(body.as_bytes()).ok()?;
    // `keep()` persists the file beyond the handle's drop.
    let (_file, path) = tmp.keep().ok()?;

    let lines = content.lines().count();
    let bytes = content.len();
    Some(format!(
        "The inline message exceeded the token budget ({tokens} tokens, {lines} lines, {bytes} \
bytes) and was written to:\n\n  {}\n\n\
Read it (choose a tool suited to its size, e.g. cat / sed -n ranges / head+tail) before continuing.",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_threshold_is_inline() {
        let text = "hello world";
        let o = prepare_outgoing(text);
        assert_eq!(o.raw, text);
        assert_eq!(o.display, text);
        assert_eq!(o.payload, text);
    }

    #[test]
    fn empty_stays_inline() {
        let o = prepare_outgoing("");
        assert_eq!(o.raw, "");
    }

    #[test]
    fn over_threshold_spills_to_file_with_raw_preserved() {
        // tokenx scores ASCII at roughly 1 token / 4 chars; 100k chars clears the
        // 10k threshold with wide margin regardless of estimator drift.
        let text = "a".repeat(100_000);
        let o = prepare_outgoing(&text);
        assert_eq!(o.raw, text, "raw must keep original content");
        assert_ne!(o.display, text, "display must be the read instruction");
        assert_eq!(o.display, o.payload);
        assert!(o.display.contains("token budget"));
        assert!(o.display.contains("Read it"));

        // The instruction references a real, readable file holding the content.
        let path_line = o
            .display
            .lines()
            .find(|l| l.trim_start().starts_with('/'))
            .expect("instruction lists a file path");
        let path = std::path::Path::new(path_line.trim());
        let on_disk = std::fs::read_to_string(path).expect("spilled file is readable");
        assert!(
            on_disk.contains(&text),
            "spilled file holds the original text"
        );
        let _ = std::fs::remove_file(path);
    }
}
