//! One credential shared by every outbound caller that presents it, with a
//! read-again step for the moment the platform retires it.
//!
//! A process holds its bearer for as long as it runs, and the platform can
//! retire that bearer at any time: the replacement is written to the
//! credential file on disk, where nothing reads it back once boot has taken
//! its snapshot. The result is a run of unauthorized answers that ends only
//! at the next start.
//!
//! This handle is the single copy every such caller reads, so a caller that
//! observes the retirement can read the file once, publish a value that
//! changed, and let every other caller see it on its next request. The
//! reload step is injected as a closure: the handle carries no knowledge of
//! where the credential lives or how it is recorded.
//!
//! The retry decision belongs to the caller, not here: compare the value a
//! failed request carried against [`Credential::current`] and retry when it
//! moved. Asking this type whether the last [`refresh`](Credential::refresh)
//! changed anything would mis-read a concurrent reload — the request that
//! lost the race holds a retired value and must retry too.

use std::sync::{Arc, RwLock};

/// A bearer every outbound call reads, with an injected reload source.
#[derive(Clone)]
pub struct Credential(Arc<Inner>);

struct Inner {
    value: RwLock<String>,
    reload: Option<Box<dyn Fn() -> Option<String> + Send + Sync>>,
}

impl Credential {
    /// A value that never changes: no file stands behind it. A blank value is
    /// not checked here: the store reader decides what counts as a stored
    /// credential and refuses a blank, and the handle presents whatever it
    /// holds.
    pub fn fixed(value: impl Into<String>) -> Self {
        let value = value.into();
        Self(Arc::new(Inner {
            value: RwLock::new(value),
            reload: None,
        }))
    }

    /// A value with a reload source. `reload` re-reads the backing store and
    /// returns the value it found; `None` means there is no usable value
    /// right now (a missing or blank file, a read failure), which never
    /// replaces what the callers hold.
    ///
    /// A blank starting value is held the same way: the non-blank rule lives
    /// where the store is read, which refuses a blank, and the proxy source
    /// refuses to build without a stored credential, so the handle repeats no
    /// check of its own.
    ///
    /// `reload` runs under the handle's own write lock, so it must not call
    /// back into the same handle (that deadlocks) and should stay cheap
    /// enough to run on every read.
    pub fn watching(
        value: impl Into<String>,
        reload: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        let value = value.into();
        Self(Arc::new(Inner {
            value: RwLock::new(value),
            reload: Some(Box::new(reload)),
        }))
    }

    /// The value to present on one outbound call.
    pub fn current(&self) -> String {
        self.read().clone()
    }

    /// Read the backing store once and publish the value when it differs
    /// from the one held. A value that is blank, unchanged, or unreadable
    /// leaves the held value alone.
    ///
    /// Every call reads the store. The read is cheap (a small local file),
    /// so reading on each call is simpler than a single-flight state machine;
    /// the retry decision does not depend on it either way, since a caller
    /// compares the value it sent against [`current`](Credential::current).
    /// The read, the comparison and the swap happen under one write lock, so
    /// no caller observes a half-applied value, and a reload cannot write an
    /// older value back over one published before it.
    pub fn refresh(&self) {
        let Some(reload) = self.0.reload.as_ref() else {
            return;
        };
        let mut held = self.0.value.write().unwrap_or_else(|e| e.into_inner());
        let Some(found) = reload() else {
            return;
        };
        if found.trim().is_empty() {
            return;
        }
        if *held != found {
            tracing::info!(
                "the stored credential was replaced; the handle now presents the new value"
            );
            *held = found;
        }
    }

    /// Replace the held value without touching the backing store: the write
    /// that produced this value has already landed on disk.
    pub fn replace(&self, value: String) {
        let mut held = self.0.value.write().unwrap_or_else(|e| e.into_inner());
        if *held == value {
            return;
        }
        *held = value;
        tracing::info!("the credential handle was moved to the value the write produced");
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, String> {
        self.0.value.read().unwrap_or_else(|e| e.into_inner())
    }
}

/// What a request should carry when it retries after an unauthorized
/// answer, or `None` when there is nothing new to send.
///
/// The order is fixed: read the store first, compare second. Comparing
/// first would miss a replacement that another caller has not published
/// yet; every caller shares this one function so no copy can drift into a
/// different order.
pub fn retry_value_after_401(credential: &Credential, sent: &str, surface: &str) -> Option<String> {
    if credential.0.reload.is_none() {
        tracing::debug!(
            surface,
            "credential has no reload source; a rejected credential stays until the next start"
        );
        return None;
    }
    credential.refresh();
    let current = credential.current();
    if current == sent {
        tracing::debug!(
            surface,
            "credential was rejected; the value on disk did not change"
        );
        return None;
    }
    tracing::debug!(
        surface,
        "credential was rejected; the retry carries the value the store now holds"
    );
    Some(current)
}

/// The value is credential material: `Debug` prints it as `[REDACTED]`
/// (the `PlatformFace`/`GatewayParams` precedent).
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Credential").field(&"[REDACTED]").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_fixed_value_never_moves() {
        let credential = Credential::fixed("token-1");
        assert_eq!(credential.current(), "token-1");
        credential.refresh();
        assert_eq!(credential.current(), "token-1");
    }

    #[test]
    fn a_reload_publishes_the_value_the_store_now_holds() {
        let store = RwLock::new(Some("token-2".to_owned()));
        let credential = Credential::watching("token-1", move || {
            store.read().unwrap_or_else(|e| e.into_inner()).clone()
        });
        assert_eq!(credential.current(), "token-1");
        credential.refresh();
        assert_eq!(credential.current(), "token-2");
    }

    #[test]
    fn a_store_with_no_value_leaves_the_held_one_alone() {
        let credential = Credential::watching("token-1", || None);
        credential.refresh();
        assert_eq!(credential.current(), "token-1");
    }

    #[test]
    fn a_blank_value_is_not_a_replacement() {
        let credential = Credential::watching("token-1", || Some("  ".to_owned()));
        credential.refresh();
        assert_eq!(credential.current(), "token-1");
    }

    /// A blank value is not refused at construction: the file reader decides
    /// what counts as a stored credential, and a handle presents what it holds.
    #[test]
    fn a_blank_value_is_held_and_presented_as_is() {
        let credential = Credential::fixed("  ");
        assert_eq!(credential.current(), "  ");
    }

    #[test]
    fn an_unchanged_value_is_held_as_is() {
        let credential = Credential::watching("token-1", || Some("token-1".to_owned()));
        credential.refresh();
        assert_eq!(credential.current(), "token-1");
    }

    #[test]
    fn replace_takes_effect_without_a_reload() {
        let credential = Credential::fixed("token-1");
        credential.replace("token-9".to_owned());
        assert_eq!(credential.current(), "token-9");
    }

    /// The reload runs on every call.
    ///
    /// Reading on each call is simpler than a single-flight state machine:
    /// the file is small and the retry decision does not depend on it.
    #[test]
    fn every_refresh_reads_the_store() {
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reads);
        let credential = Credential::watching("token-1", move || {
            counted.fetch_add(1, Ordering::SeqCst);
            None
        });
        credential.refresh();
        credential.refresh();
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_retry_carries_the_value_the_store_now_holds() {
        let credential = Credential::watching("token-1", || Some("token-2".to_owned()));
        assert_eq!(
            retry_value_after_401(&credential, "token-1", "tests"),
            Some("token-2".to_owned())
        );
        assert_eq!(credential.current(), "token-2");
    }

    #[test]
    fn a_retry_reports_nothing_when_the_store_did_not_move() {
        let credential = Credential::watching("token-1", || None);
        assert_eq!(retry_value_after_401(&credential, "token-1", "tests"), None);
    }

    /// A value that is already held does not count as a replacement.
    #[test]
    fn a_retry_reports_nothing_when_the_store_holds_what_was_sent() {
        let credential = Credential::watching("token-1", || Some("token-1".to_owned()));
        assert_eq!(retry_value_after_401(&credential, "token-1", "tests"), None);
    }

    /// The comparison happens after the store is read: a value that only
    /// the store knows about is still a reason to retry.
    #[test]
    fn a_retry_reads_before_it_compares() {
        let credential = Credential::watching("token-1", || Some("token-2".to_owned()));
        // The handle still holds token-1: comparing before the read would
        // answer None here.
        assert_eq!(credential.current(), "token-1");
        assert!(retry_value_after_401(&credential, "token-1", "tests").is_some());
    }

    #[test]
    fn a_fixed_value_has_nothing_to_retry_with() {
        let credential = Credential::fixed("token-1");
        assert_eq!(retry_value_after_401(&credential, "token-1", "tests"), None);
    }

    #[test]
    fn debug_redacts_the_value() {
        let credential = Credential::fixed("sk-tagma-secret");
        let printed = format!("{credential:?}");
        assert!(!printed.contains("sk-tagma-secret"), "got: {printed}");
        assert!(printed.contains("[REDACTED]"), "got: {printed}");
    }
}
