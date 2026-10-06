//! Boot-time environment governance.
//!
//! Agent shells inherit this process environment wholesale: the shell
//! backend spawns `bash` without an env clear, so anything still present
//! here reaches every agent. This module is the single place those
//! exclusions live. Two tables:
//!
//! - `SCRUB_SECRETS`: secrets the tagma consumes once at boot (or never
//!   consumes at all) and then drops from the environment. Adding an
//!   exclusion is one line in that table.
//! - the retired relay pair ([`ensure_no_retired_env`]): variables from a
//!   released shape that no longer exists. A deployment (or a persisted
//!   instance record replayed on restart) that still carries one is a
//!   configuration error and fails the boot, exactly like a misspelled
//!   name — never a silent local-only boot with the relay dropped.
//!
//! Everything here runs on the single main thread before the async
//! runtime is built (see `main`): edition 2024 marks `remove_var` unsafe
//! precisely because concurrent `getenv` on another thread is undefined,
//! and the pre-runtime window is the one point where "no other thread
//! exists" is provable rather than assumed.
//!
//! The scrub closes the inheritance face only: child processes and
//! agent shells. A same-UID reader can still see the exec-time copy in
//! `/proc/<pid>/environ` or in process memory — the same-UID boundary
//! already allows ptrace, so those faces stay out of scope.

use anyhow::Result;

/// Secrets to drop from the process environment during boot:
///
/// - `KALLIPAI_OPERATOR_TOKEN`: the env pin, captured by
///   [`capture_operator_pin`] before the scrub; AppState keeps only its
///   SHA-256, and the plaintext never reaches a log.
/// - `KALLIPAI_FILES_TOKEN`: a legacy shape the tagma never reads (the
///   token source is the registered credential).
/// - `KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE`: single-use; clap has already
///   parsed it into `Args` by the time the scrub runs.
const SCRUB_SECRETS: &[&str] = &[
    "KALLIPAI_OPERATOR_TOKEN",
    "KALLIPAI_FILES_TOKEN",
    "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE",
];

/// Retired relay variables: the per-service relay env from a released
/// shape, replaced by the single `KALLIPAI_POLIS_URL` origin.
const RETIRED_RELAY: &[&str] = &[
    "KALLIPAI_TAGMA_RELAY_ARCHEION_URL",
    "KALLIPAI_TAGMA_RELAY_LESCHE_URL",
];

/// Capture the operator token pin before [`scrub_boot_secrets`] removes
/// it: the value travels on to `resolve_operator_token`, which keeps the
/// env-pin > token-file > fresh-mint priority.
pub fn capture_operator_pin() -> Option<String> {
    std::env::var("KALLIPAI_OPERATOR_TOKEN").ok()
}

/// Remove every listed secret still present, returning the names removed
/// (names only — values never reach a log).
pub fn scrub_boot_secrets() -> Vec<&'static str> {
    SCRUB_SECRETS
        .iter()
        .copied()
        .filter(|key| std::env::var_os(key).is_some())
        .inspect(|&key| {
            // SAFETY: single-threaded boot window — the main thread is the
            // only thread and the async runtime does not exist yet, so no
            // concurrent getenv can race this mutation.
            unsafe { std::env::remove_var(key) };
        })
        .collect()
}

/// Fail the boot when a retired relay variable is set: the old values
/// were per-service URLs and do not mechanically derive the new origin,
/// so the recoverable move is a fresh enroll against the configured
/// `KALLIPAI_POLIS_URL`, and this fails loudly pointing there.
pub fn ensure_no_retired_env() -> Result<()> {
    let found: Vec<&str> = RETIRED_RELAY
        .iter()
        .copied()
        .filter(|key| std::env::var_os(key).is_some())
        .collect();
    anyhow::ensure!(
        found.is_empty(),
        concat!(
            "retired relay environment variable(s) {} present; the per-service",
            " relay env was replaced by the single KALLIPAI_POLIS_URL origin --",
            " remove the retired variables and re-enroll the relay against",
            " KALLIPAI_POLIS_URL"
        ),
        found.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_removes_each_secret_exactly_once_and_leaves_neighbors() {
        kallipai_testkit::with_env(
            &[
                ("KALLIPAI_FILES_TOKEN", Some("legacy-token")),
                ("KALLIPAI_TOKEN_BUDGET", Some("1000")),
                // The ambient test-process env may carry the other
                // secrets (it is itself a tagma agent env); pin them
                // absent so the expected scrub set is exact.
                ("KALLIPAI_OPERATOR_TOKEN", None),
                ("KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE", None),
            ],
            || {
                assert_eq!(scrub_boot_secrets(), vec!["KALLIPAI_FILES_TOKEN"]);
                assert!(std::env::var_os("KALLIPAI_FILES_TOKEN").is_none());
                // Idempotent: a second sweep finds nothing.
                assert!(scrub_boot_secrets().is_empty());
                // An unlisted key is untouched.
                assert_eq!(std::env::var("KALLIPAI_TOKEN_BUDGET"), Ok("1000".into()));
            },
        );
    }

    #[test]
    fn capture_takes_the_pin_before_the_scrub_clears_it() {
        kallipai_testkit::with_env(
            &[
                ("KALLIPAI_OPERATOR_TOKEN", Some("pin-value")),
                ("KALLIPAI_FILES_TOKEN", None),
                ("KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE", None),
            ],
            || {
                assert_eq!(capture_operator_pin(), Some("pin-value".into()));
                assert_eq!(scrub_boot_secrets(), vec!["KALLIPAI_OPERATOR_TOKEN"]);
                assert!(capture_operator_pin().is_none());
            },
        );
    }

    #[test]
    fn a_retired_relay_variable_fails_the_boot_naming_it() {
        kallipai_testkit::with_env(
            &[("KALLIPAI_TAGMA_RELAY_ARCHEION_URL", Some("https://old"))],
            || {
                let err = ensure_no_retired_env().unwrap_err();
                let text = format!("{err:#}");
                assert!(text.contains("KALLIPAI_TAGMA_RELAY_ARCHEION_URL"), "{text}");
                assert!(text.contains("KALLIPAI_POLIS_URL"), "{text}");
            },
        );
    }

    #[test]
    fn retired_variables_are_listed_together() {
        kallipai_testkit::with_env(
            &[
                ("KALLIPAI_TAGMA_RELAY_ARCHEION_URL", Some("https://a")),
                ("KALLIPAI_TAGMA_RELAY_LESCHE_URL", Some("https://l")),
            ],
            || {
                let err = ensure_no_retired_env().unwrap_err();
                let text = format!("{err:#}");
                assert!(text.contains("KALLIPAI_TAGMA_RELAY_ARCHEION_URL"), "{text}");
                assert!(text.contains("KALLIPAI_TAGMA_RELAY_LESCHE_URL"), "{text}");
            },
        );
    }

    #[test]
    fn no_retired_variable_set_is_clean() {
        kallipai_testkit::with_env(
            &[
                ("KALLIPAI_TAGMA_RELAY_ARCHEION_URL", None),
                ("KALLIPAI_TAGMA_RELAY_LESCHE_URL", None),
            ],
            || {
                assert!(ensure_no_retired_env().is_ok());
            },
        );
    }
}
