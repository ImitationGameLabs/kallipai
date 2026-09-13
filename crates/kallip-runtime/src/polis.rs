//! The platform API edge origin (`KALLIP_POLIS_URL`) and the per-service
//! base derivation shared by every Rust-side client.
//!
//! One origin fronts every backend service: a client base is always
//! `{origin}/v1/{service}` (the edge strips the service segment, files
//! excepted -- its name is the resource segment and passes through), and
//! the client path templates are pure tails.

/// Derive the platform edge origin from an optional explicit value,
/// trimming a trailing slash so base joins stay single-slash.
///
/// An unset or blank origin is an error, never a fallback: every caller
/// either carries credentials (an enrollment code, a files token) or
/// configures a relay, and sending those to an assumed deployment --
/// one the operator never chose -- would leak them.
pub fn polis_origin(explicit: Option<String>) -> anyhow::Result<String> {
    let raw = explicit
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the platform edge origin is not configured: set KALLIP_POLIS_URL \
                 to the origin every backend service is reached through (for \
                 example https://api.kallipai.com); credentials are never sent \
                 to an assumed deployment"
            )
        })?;
    Ok(raw.trim_end_matches('/').to_owned())
}

/// Read `KALLIP_POLIS_URL` from the environment and derive the origin
/// (see [`polis_origin`]: unset and blank both fail).
pub fn polis_origin_from_env() -> anyhow::Result<String> {
    polis_origin(std::env::var("KALLIP_POLIS_URL").ok())
}

/// The client base for one backend service: `{origin}/v1/{service}`.
pub fn service_base(origin: &str, service: &str) -> String {
    format!("{origin}/v1/{service}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_origin_is_an_error() {
        let error = polis_origin(None).unwrap_err().to_string();
        assert!(error.contains("KALLIP_POLIS_URL"));
    }

    #[test]
    fn blank_origin_is_an_error() {
        for blank in ["", "   "] {
            assert!(polis_origin(Some(blank.to_owned())).is_err());
        }
    }

    #[test]
    fn explicit_value_passes_through() {
        assert_eq!(
            polis_origin(Some("http://127.0.0.1:8080".to_owned())).unwrap(),
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn trailing_slash_is_trimmed() {
        assert_eq!(
            polis_origin(Some("https://api.example.com/".to_owned())).unwrap(),
            "https://api.example.com"
        );
    }

    #[test]
    fn multiple_trailing_slashes_are_trimmed() {
        assert_eq!(
            polis_origin(Some("https://api.example.com///".to_owned())).unwrap(),
            "https://api.example.com"
        );
    }

    #[test]
    fn service_base_joins_one_slash() {
        assert_eq!(
            service_base("https://api.example.com", "archeion"),
            "https://api.example.com/v1/archeion"
        );
    }

    #[test]
    fn env_reader_unset_blank_and_set() {
        // SAFETY: test-only env edits; no other test in this binary reads
        // KALLIP_POLIS_URL (verified by rg at authoring time), so the
        // process-global edits cannot race a reader here. The three legs
        // share one test because they mutate the same process-global.
        unsafe {
            std::env::remove_var("KALLIP_POLIS_URL");
        }
        assert!(polis_origin_from_env().is_err());
        unsafe {
            std::env::set_var("KALLIP_POLIS_URL", "   ");
        }
        assert!(polis_origin_from_env().is_err());
        unsafe {
            std::env::set_var("KALLIP_POLIS_URL", "https://api.example.com/");
        }
        assert_eq!(polis_origin_from_env().unwrap(), "https://api.example.com");
    }
}
