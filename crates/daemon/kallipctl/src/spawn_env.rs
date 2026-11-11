//! Spawn-side enrollment: mint a code with the operator's access token and
//! fold the two relay entries into the env the daemon launches with.
//!
//! The mint runs in this process. The daemon sees only the single-use code,
//! never the token: the code is what the instance needs to enroll itself.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use crate::archeion;
use crate::origin::Origin;

/// The env to launch with, plus the code minted for it (if any) so a failed
/// daemon call can still name the pending tagma it left behind.
pub(crate) async fn prepare(
    env: Vec<String>,
    polis: Option<String>,
    token_file: Option<String>,
    allow_plaintext: bool,
) -> Result<(Vec<String>, Option<archeion::MintedCode>)> {
    let Some(polis) = polis else {
        if token_file.is_some() {
            anyhow::bail!("--token-file only means something together with --polis");
        }
        // The flag is ignored here: a plaintext polisUrl host puts it on PATH
        // for every invocation, so an offline spawn has to keep working with it
        // present but meaningless.
        return Ok((env, None));
    };
    // Both sources write the same two variables. Picking a winner silently
    // would let the polis that signed the code and the polis the instance
    // enrolls against drift apart.
    if env.iter().any(|pair| {
        pair.starts_with("KALLIPAI_POLIS_URL=")
            || pair.starts_with("KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=")
    }) {
        anyhow::bail!(
            "pass either --polis or -e KALLIPAI_POLIS_URL / -e KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE, not both"
        );
    }
    let origin = Origin::parse("--polis", &polis)?;
    // The mint spends a long-lived access token and the instance keeps the
    // code it mints; over plain HTTP either one crosses the network in the
    // clear, so a non-loopback origin needs the operator's say-so.
    origin.check_plaintext("--polis", allow_plaintext)?;
    if origin.is_plaintext() {
        eprintln!(
            "plaintext origin {}: the access token and the enrollment code cross the network in the clear",
            origin.as_str()
        );
    }
    // The instance keeps this string for the rest of its life, so it gets the
    // normalized origin rather than what was typed.
    let origin = origin.as_str().to_owned();
    let path = match token_file {
        Some(path) => PathBuf::from(path),
        None => default_token_path()?,
    };
    let token = archeion::read_token(&path)?;
    let minted = archeion::mint_enrollment(&origin, &token).await?;
    let mut env = env;
    env.push(format!("KALLIPAI_POLIS_URL={origin}"));
    env.push(format!(
        "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE={}",
        minted.code
    ));
    Ok((env, Some(minted)))
}

/// `$XDG_CONFIG_HOME/kallipai/access-token`, the platform's per-user config
/// root (the same shape the tagma uses for its profiles directory).
fn default_token_path() -> Result<PathBuf> {
    let config = dirs::config_dir().context("no config directory for this user")?;
    Ok(config.join("kallipai").join("access-token"))
}

#[cfg(test)]
mod tests {
    use super::prepare;

    #[tokio::test]
    async fn without_polis_the_env_passes_through() {
        let env = vec!["KALLIPAI_FOO=1".to_string()];
        let (out, minted) = prepare(env.clone(), None, None, false)
            .await
            .expect("pass-through");
        assert_eq!(out, env);
        assert!(minted.is_none());
    }

    #[tokio::test]
    async fn a_token_file_without_polis_is_a_usage_error() {
        let err = prepare(Vec::new(), None, Some("/tmp/token".to_string()), false)
            .await
            .expect_err("token without polis");
        assert!(
            format!("{err:#}").contains("together with --polis"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn both_sources_for_the_same_variable_are_refused() {
        for pair in [
            "KALLIPAI_POLIS_URL=https://example.test",
            "KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE=sk-enroll-x",
        ] {
            let err = prepare(
                vec![pair.to_string()],
                Some("https://a.test".to_string()),
                None,
                false,
            )
            .await
            .expect_err("two sources");
            assert!(format!("{err:#}").contains("not both"), "{err:#}");
        }
    }

    #[tokio::test]
    async fn a_polis_that_is_not_a_url_is_named_as_such() {
        let err = prepare(Vec::new(), Some("a.test".to_string()), None, false)
            .await
            .expect_err("not a URL");
        assert!(
            format!("{err:#}").contains("not an absolute URL"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_polis_on_another_scheme_is_refused_before_the_token() {
        let err = prepare(
            Vec::new(),
            Some("file:///tmp/polis".to_string()),
            None,
            false,
        )
        .await
        .expect_err("not http");
        assert!(
            format!("{err:#}").contains("must be an http or https URL"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_polis_with_a_query_or_fragment_is_refused() {
        for polis in ["https://api.test?x=1", "https://api.test#frag"] {
            let err = prepare(Vec::new(), Some(polis.to_string()), None, false)
                .await
                .expect_err("not an origin");
            assert!(
                format!("{err:#}").contains("without a query or fragment"),
                "{polis}: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn a_polis_with_userinfo_is_refused() {
        let err = prepare(
            Vec::new(),
            Some("https://user:pw@api.test".to_string()),
            None,
            false,
        )
        .await
        .expect_err("userinfo");
        assert!(
            format!("{err:#}").contains("must not carry a user name"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_polis_with_a_path_is_refused() {
        // A path survives into KALLIPAI_POLIS_URL, and every consumer
        // appends `/v1/<service>` to it.
        let err = prepare(
            Vec::new(),
            Some("https://api.test/polis".to_string()),
            None,
            false,
        )
        .await
        .expect_err("a path is not an origin");
        assert!(format!("{err:#}").contains("without a path"), "{err:#}");
    }

    #[tokio::test]
    async fn a_plaintext_origin_off_this_host_is_refused() {
        let err = prepare(Vec::new(), Some("http://api.test".to_string()), None, false)
            .await
            .expect_err("plaintext");
        assert!(
            format!("{err:#}").contains("would cross the network in the clear"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_plaintext_loopback_origin_reaches_the_token_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent").to_string_lossy().into_owned();
        let err = prepare(
            Vec::new(),
            Some("http://127.0.0.1:7100".to_string()),
            Some(missing),
            false,
        )
        .await
        .expect_err("no token file");
        // The origin checks passed: the failure is the token file, not the
        // scheme.
        assert!(format!("{err:#}").contains("no access token at"), "{err:#}");
    }

    #[tokio::test]
    async fn a_plaintext_name_under_localhost_is_loopback() {
        // RFC 6761: every name under .localhost resolves to loopback.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent").to_string_lossy().into_owned();
        let err = prepare(
            Vec::new(),
            Some("http://api.localhost:8080".to_string()),
            Some(missing),
            false,
        )
        .await
        .expect_err("no token file");
        assert!(format!("{err:#}").contains("no access token at"), "{err:#}");
    }

    #[tokio::test]
    async fn a_localhost_name_with_a_trailing_dot_is_loopback() {
        // One trailing dot still names the same host.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent").to_string_lossy().into_owned();
        let err = prepare(
            Vec::new(),
            Some("http://localhost.:8080".to_string()),
            Some(missing),
            false,
        )
        .await
        .expect_err("no token file");
        assert!(format!("{err:#}").contains("no access token at"), "{err:#}");
    }

    #[tokio::test]
    async fn the_plaintext_flag_lets_a_named_origin_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent").to_string_lossy().into_owned();
        let err = prepare(
            Vec::new(),
            Some("http://kallipai.lan:8080".to_string()),
            Some(missing),
            true,
        )
        .await
        .expect_err("no token file");
        assert!(format!("{err:#}").contains("no access token at"), "{err:#}");
    }

    #[tokio::test]
    async fn the_plaintext_flag_without_polis_changes_nothing() {
        let env = vec!["KALLIPAI_FOO=1".to_string()];
        let (out, minted) = prepare(env.clone(), None, None, true)
            .await
            .expect("ignored flag");
        assert_eq!(out, env);
        assert!(minted.is_none());
    }

    #[tokio::test]
    async fn the_plaintext_flag_does_not_admit_another_scheme() {
        let err = prepare(
            Vec::new(),
            Some("file:///tmp/polis".to_string()),
            None,
            true,
        )
        .await
        .expect_err("not http");
        assert!(
            format!("{err:#}").contains("must be an http or https URL"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn the_plaintext_flag_keeps_the_origin_shape_checks() {
        let err = prepare(
            Vec::new(),
            Some("http://kallipai.lan:8080/path".to_string()),
            None,
            true,
        )
        .await
        .expect_err("a path is not an origin");
        assert!(format!("{err:#}").contains("without a path"), "{err:#}");
    }
}
