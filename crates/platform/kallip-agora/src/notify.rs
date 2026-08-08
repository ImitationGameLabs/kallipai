//! Outbound notification transport (today: only email verification).
//!
//! A seam so the email-management routes can be written against a stable
//! interface before a real provider (SMTP / SES / etc.) is wired. The default
//! [`LoggingTransport`] emits the would-be message at `info` level -- enough to
//! complete a verification flow end-to-end in dev (the operator reads the token
//! from the log). Swapping in a real provider is a new `impl` and a one-line
//! change at construction; no route code changes.

use async_trait::async_trait;

/// Delivers an email-verification link to `to_address`. The link embeds the
/// single-use `token` (its hash is stored server-side, not the plaintext).
#[async_trait]
pub trait MailTransport: Send + Sync {
    async fn send_verification(&self, to_address: &str, token: &str);
}

/// Dev/null-ish transport: logs the verification link instead of sending it.
/// Used until a real provider is configured.
pub struct LoggingTransport;

#[async_trait]
impl MailTransport for LoggingTransport {
    async fn send_verification(&self, to_address: &str, token: &str) {
        tracing::info!(
            address = %to_address,
            token = %token,
            "email verification (logging transport; no SMTP wired)",
        );
    }
}
