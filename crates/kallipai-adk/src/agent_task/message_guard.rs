//! What an external message is cut to before it enters the transcript.
//!
//! An oversized message is cut to a head-and-tail slice carrying a trailing
//! banner, and the untouched original spills to a temporary file the banner names.
//! The guard fails open: an unwritable spill root leaves the message whole rather
//! than dropping the delivery.

use crate::config::{DEFAULT_MESSAGE_FULL_TOKENS, DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS};
use crate::context::estimate_text;
use crate::text_slice::converge_under_cap;
use kallipai_shell::spill;
use kallipai_shell::spill::{MESSAGE_SPILL, spill_root};

#[cfg(test)]
static SPILL_ROOT_OVERRIDE: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// Test-only spill-root injection: message-guard unit tests must be
/// hermetic (not write the real /tmp/kallipai tree), and the fail-open
/// branch needs a root that deterministically fails.
#[cfg(test)]
pub(crate) fn set_message_spill_root(root: Option<std::path::PathBuf>) {
    *SPILL_ROOT_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = root;
}

/// Serialize tests that touch the process-wide spill-root override
/// (cargo runs unit tests on parallel threads). Async mutex: tests
/// hold it across awaits.
#[cfg(test)]
pub(crate) async fn message_spill_serial() -> tokio::sync::MutexGuard<'static, ()> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    SERIAL.lock().await
}

/// Prefer the test override (while one is pinned) over the real root.
fn message_spill_root() -> std::path::PathBuf {
    #[cfg(test)]
    {
        if let Some(root) = SPILL_ROOT_OVERRIDE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return root;
        }
    }
    spill_root()
}

/// Entry-cap transform shared by every external message entrance: text
/// within the cap passes through unchanged; an oversized message is
/// spilled and cut down to a head+tail slice with the banner. Fail-open
/// on spill errors — entry protection must never cost a message.
/// `inbox_backed` gates the banner's inbox pointer: true only for
/// entrances that persist the message in an inbox row.
pub(crate) fn cap_external_message(text: &str, inbox_backed: bool) -> String {
    if estimate_text(text) <= DEFAULT_MESSAGE_FULL_TOKENS {
        return text.to_owned();
    }
    match spill::spill_content(&message_spill_root(), &MESSAGE_SPILL, text) {
        Ok(path) => cut_message(text, &path, inbox_backed),
        Err(e) => {
            tracing::warn!("message spill failed, recording untruncated: {e:#}");
            text.to_owned()
        }
    }
}

/// Cut an oversized external message down under
/// DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS (mirroring the tool-result cap so
/// both faces share the wedge math) and append the message banner: what was
/// kept, where the full original lives, and the resend-in-parts advice.
/// Inbox-backed entrances add an inbox-copy pointer; the prompt-channel
/// drain keeps it off. The drain sees bare lines without producer context:
/// its rowless producers (notices, kicks) are short
/// texts that never reach the cap, and its heavy producer (peer
/// injections) does have an inbox row — so off can only under-claim,
/// never over-claim.
fn cut_message(text: &str, spill_path: &std::path::Path, inbox_backed: bool) -> String {
    let full_est = estimate_text(text);
    let total = text.chars().count();
    let chars_cap = DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS * total / full_est;
    let parts = converge_under_cap(text, chars_cap, DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS);
    let inbox_note = if inbox_backed {
        "; the full text is also in your inbox (kallip inbox read)"
    } else {
        ""
    };
    format!(
        "{}\n[... message truncated: kept first {} and last {} of {total} chars (~{} of ~{full_est} estimated tokens). Full original saved at {} (a temporary file — archive it yourself if you need it long-term){inbox_note}. If you need the rest, ask the sender to resend the content in smaller parts ...]",
        parts.2,
        parts.0,
        parts.1,
        estimate_text(&parts.2),
        spill_path.display(),
    )
}
