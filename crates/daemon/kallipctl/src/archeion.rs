//! Enrollment-code minting against the platform edge (archeion).
//!
//! A spawned instance enrolls with a single-use code. Minting one writes to
//! the owner's tagma set, so it runs on the platform access token the operator
//! holds: the credential never enters the daemon or the instance.
//!
//! Every request goes through `kallipai-archeion-client`, the same crate the
//! admin tool uses, so the URL, the bearer, and the error envelope have one
//! definition rather than a second copy here.

use std::path::Path;

use anyhow::{Context as _, Result};

use kallipai_archeion_client::ArcheionClient;
use kallipai_common::protocol::ApiError;
use kallipai_common::protocol::error::{FORBIDDEN_PERM_MISSING, FORBIDDEN_USER_REQUIRED};

/// The single-use plaintext, the pending tagma's id, and the instant it stops
/// working.
pub(crate) struct MintedCode {
    pub(crate) code: String,
    /// The pending tagma this code enrolls. The handle to revoke an unused
    /// code, and what a failed spawn names so the operator can find it.
    pub(crate) id: String,
    pub(crate) expires_at: String,
}
/// The code is a credential until it is redeemed: `Debug` keeps it out of
/// logs and panic messages while still naming the value's shape.
impl std::fmt::Debug for MintedCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedCode")
            .field("code", &"<redacted>")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Read the token file, with the two checks that turn a bad file into a
/// readable failure instead of a 401 from the far end.
pub(crate) fn read_token(path: &Path) -> Result<String> {
    if !path.is_file() {
        anyhow::bail!(
            "no access token at {}; create one in the web UI with archeion:tagma:write, then write it to that file (chmod 600)",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("reading the metadata of {}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "token file {} is readable by other users (mode {mode:04o}); chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let token = raw.trim();
    if token.is_empty() {
        anyhow::bail!(
            "token file {} is empty; write the access token into it",
            path.display()
        );
    }
    Ok(token.to_owned())
}

/// A terminal command cannot wait on a socket the far end forgot: a mint
/// that trickles gets its own failure text, distinct from a refused
/// connection (which fails in milliseconds).
const MINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `POST {polis}/v1/archeion/tagmata`, the access token as the bearer.
///
/// `polis` is the platform edge origin -- the same string the instance gets
/// as `KALLIPAI_POLIS_URL` -- so both sides derive their service base from
/// one value: the instance `{origin}/v1/lesche`, this mint `{origin}/v1/archeion`.
pub(crate) async fn mint_enrollment(polis: &str, token: &str) -> Result<MintedCode> {
    mint_within(MINT_TIMEOUT, polis, token).await
}

/// The mint with an explicit deadline, so the timeout arm is testable
/// without waiting half a minute.
async fn mint_within(timeout: std::time::Duration, polis: &str, token: &str) -> Result<MintedCode> {
    let base = kallipai_common::polis::service_base(polis, "archeion");
    // A terminal command must end. The client's own default carries a
    // deadline, but passing one keeps the failure text below tied to the
    // number it names; redirects are off because a bearer-bearing POST has no
    // business following one.
    let http = kallipai_common::telemetry::http_client(
        reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none()),
    )
    .context("building the HTTP client")?;
    let client = ArcheionClient::builder(&base)
        .access_token(token)
        .http_client(http)
        .build()?;
    let minted = client
        .mint_enrollment_code()
        .await
        .map_err(|err| describe(err, polis, timeout))?;
    if minted.code.trim().is_empty() {
        anyhow::bail!("archeion answered without an enrollment code");
    }
    Ok(MintedCode {
        code: minted.code,
        id: minted.id,
        expires_at: minted
            .expires_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| minted.expires_at.to_string()),
    })
}

/// One failure per recovery. The server tags the two 403s with a code, which
/// is the only way to tell "add the string" from "this token is not a user":
/// both read as `403` on the status line.
fn describe(err: anyhow::Error, polis: &str, timeout: std::time::Duration) -> anyhow::Error {
    if let Some(api) = err.downcast_ref::<ApiError>() {
        return match (api.status, api.code.as_deref()) {
            (401, _) => {
                anyhow::anyhow!("archeion rejected the access token (invalid, expired, or revoked)")
            }
            (403, Some(FORBIDDEN_PERM_MISSING)) => anyhow::anyhow!(
                "the access token lacks archeion:tagma:write; add the string to the token ({})",
                api.message
            ),
            (403, Some(FORBIDDEN_USER_REQUIRED)) => anyhow::anyhow!(
                "this token belongs to the admin console or a tagma, not to a user; \
                 kallipctl needs a personal access token ({})",
                api.message
            ),
            (403, _) => anyhow::anyhow!("archeion refused the request: 403 {}", api.message),
            (429, _) => anyhow::anyhow!(
                "too many live pending enrollment codes; revoke the ones you no longer \
                 need in the web UI, then retry ({})",
                api.message
            ),
            _ => anyhow::anyhow!(
                "archeion refused to mint an enrollment code: {} {}",
                api.status,
                api.message
            ),
        };
    }
    if let Some(http) = transport_of(&err) {
        if http.is_timeout() {
            return anyhow::anyhow!(
                "archeion at {polis} did not answer within {}s",
                timeout.as_secs()
            );
        }
        // Only a failure to send or connect is the network's fault. A decode
        // failure means the far end answered in a shape this CLI cannot read,
        // and blaming reachability would send the operator to the wrong place.
        if http.is_connect() || http.is_request() {
            return anyhow::anyhow!("cannot reach archeion at {polis}: {err:#}");
        }
        return anyhow::anyhow!("minting an enrollment code: {err:#}");
    }
    // Anything else reached the far end and came back unreadable.
    anyhow::anyhow!("minting an enrollment code: {err:#}")
}

/// The transport failure an error carries, wherever it sits: the mint goes
/// through a middleware client, so a failure below that chain arrives one
/// level down.
fn transport_of(err: &anyhow::Error) -> Option<&reqwest::Error> {
    if let Some(http) = err.downcast_ref::<reqwest::Error>() {
        return Some(http);
    }
    match err.downcast_ref::<reqwest_middleware::Error>() {
        Some(reqwest_middleware::Error::Reqwest(http)) => Some(http),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::path::PathBuf;

    use super::{mint_enrollment, mint_within, read_token};

    /// One-shot HTTP server answering a fixed status and body, and handing back
    /// the request it read. No HTTP test dependency: the client only needs a
    /// socket, and the captured request is what pins the path and the bearer.
    fn serve_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).expect("read the request");
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).expect("write");
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        (format!("http://{addr}"), handle)
    }

    /// A server that accepts the connection and then says nothing, so the
    /// client's own deadline is what fails.
    fn serve_silently() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            std::thread::sleep(std::time::Duration::from_secs(2));
            drop(stream);
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn the_mint_carries_the_id_and_expiry_and_goes_through_the_edge_path() {
        let (polis, request) = serve_once(
            200,
            r#"{"code":"sk-enroll-abc","id":"t1","created_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:10:00Z"}"#,
        );
        let minted = mint_enrollment(&polis, "sk-pat-token").await.expect("mint");
        assert_eq!(minted.code, "sk-enroll-abc");
        assert_eq!(minted.id, "t1");
        assert_eq!(minted.expires_at, "2026-01-01T00:10:00Z");

        // The edge serves `{origin}/v1/{service}`; a bare `/tagmata` only exists
        // behind the strip, so this assertion is what keeps the two dialects
        // from drifting apart again.
        let request = request.join().expect("server thread");
        assert!(
            request.starts_with("POST /v1/archeion/tagmata "),
            "{request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer sk-pat-token"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_silent_polis_fails_on_the_client_deadline() {
        let polis = serve_silently();
        let err = mint_within(
            std::time::Duration::from_millis(200),
            &polis,
            "sk-pat-token",
        )
        .await
        .expect_err("no answer");
        let text = format!("{err:#}");
        assert!(text.contains("did not answer within"), "{text}");
        assert!(!text.contains("cannot reach archeion"), "{text}");
    }

    #[tokio::test]
    async fn a_token_without_the_write_string_names_the_string() {
        let (polis, request) = serve_once(
            403,
            r#"{"error":{"message":"this credential does not carry archeion:tagma:write","code":"perm_missing"}}"#,
        );
        let err = mint_enrollment(&polis, "sk-pat-token")
            .await
            .expect_err("a token without the string");
        let text = format!("{err:#}");
        assert!(text.contains("lacks archeion:tagma:write"), "{text}");
        assert!(
            text.contains("this credential does not carry"),
            "the server sentence travels with the failure: {text}"
        );
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn a_non_user_token_is_told_which_kind_it_needs() {
        let (polis, request) = serve_once(
            403,
            r#"{"error":{"message":"user access required","code":"user_required"}}"#,
        );
        let err = mint_enrollment(&polis, "sk-admin-token")
            .await
            .expect_err("an admin bearer");
        let text = format!("{err:#}");
        assert!(text.contains("personal access token"), "{text}");
        assert!(
            !text.contains("lacks archeion:tagma:write"),
            "a user-principal rejection is not a missing string: {text}"
        );
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn a_403_without_a_code_keeps_the_server_sentence() {
        let (polis, request) = serve_once(403, r#"{"error":{"message":"forbidden by policy"}}"#);
        let err = mint_enrollment(&polis, "sk-pat-token")
            .await
            .expect_err("an unlabelled 403");
        let text = format!("{err:#}");
        assert!(text.contains("refused the request: 403"), "{text}");
        assert!(text.contains("forbidden by policy"), "{text}");
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn an_expired_token_is_not_reported_as_an_unreachable_polis() {
        let (polis, request) = serve_once(
            401,
            r#"{"error":{"message":"invalid token","code":"token_invalid"}}"#,
        );
        let err = mint_enrollment(&polis, "sk-pat-token")
            .await
            .expect_err("an expired token");
        let text = format!("{err:#}");
        assert!(text.contains("rejected the access token"), "{text}");
        assert!(!text.contains("cannot reach"), "{text}");
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn a_full_pending_slot_is_its_own_failure() {
        let (polis, request) = serve_once(
            429,
            r#"{"error":{"message":"too many live enrollment codes"}}"#,
        );
        let err = mint_enrollment(&polis, "sk-pat-token")
            .await
            .expect_err("no free slot");
        let text = format!("{err:#}");
        assert!(text.contains("too many live pending"), "{text}");
        assert!(text.contains("revoke the ones"), "{text}");
        request.join().expect("server thread");
    }

    /// A status the branches above do not name: the edge answered, so the
    /// failure is the far end's, not the network's.
    #[tokio::test]
    async fn an_unlabelled_failure_is_not_blamed_on_the_network() {
        let (polis, request) = serve_once(
            500,
            r#"{"error":{"message":"database error: switched off"}}"#,
        );
        let err = mint_enrollment(&polis, "sk-pat-token")
            .await
            .expect_err("500");
        let text = format!("{err:#}");
        assert!(text.contains("refused to mint"), "{text}");
        assert!(text.contains("database error: switched off"), "{text}");
        assert!(!text.contains("cannot reach"), "{text}");
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn a_closed_port_reads_as_unreachable() {
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr");
        let polis = format!("http://{addr}");
        let err = mint_enrollment(&polis, "sk-pat-token")
            .await
            .expect_err("nothing listens");
        assert!(
            format!("{err:#}").contains("cannot reach archeion"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_reply_without_a_code_is_a_read_failure() {
        let (polis, request) = serve_once(
            200,
            r#"{"id":"t1","created_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:10:00Z"}"#,
        );
        let err = mint_enrollment(&polis, "sk-pat-token")
            .await
            .expect_err("no code in the body");
        let text = format!("{err:#}");
        assert!(text.contains("minting an enrollment code"), "{text}");
        assert!(!text.contains("cannot reach"), "{text}");
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn an_empty_code_is_refused() {
        let (polis, request) = serve_once(
            200,
            r#"{"code":"  ","id":"t1","created_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:10:00Z"}"#,
        );
        let err = mint_enrollment(&polis, "sk-pat-token")
            .await
            .expect_err("a blank code");
        assert!(
            format!("{err:#}").contains("without an enrollment code"),
            "{err:#}"
        );
        request.join().expect("server thread");
    }

    fn token_file(dir: &tempfile::TempDir, contents: &str) -> PathBuf {
        let path = dir.path().join("access-token");
        std::fs::write(&path, contents).expect("write token file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict mode");
        }
        path
    }

    #[test]
    fn a_missing_token_file_names_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent");
        let err = read_token(&missing).expect_err("missing file");
        assert!(format!("{err:#}").contains("no access token at"), "{err:#}");
    }

    #[test]
    fn an_empty_token_file_says_it_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = token_file(&dir, "   \n");
        let err = read_token(&empty).expect_err("empty file");
        assert!(format!("{err:#}").contains("is empty"), "{err:#}");
    }

    #[test]
    fn a_good_token_file_is_trimmed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = token_file(&dir, "sk-pat-token\n");
        assert_eq!(read_token(&good).expect("trimmed token"), "sk-pat-token");
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_token_file_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let loose = token_file(&dir, "sk-pat-token\n");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644))
            .expect("loosen mode");
        let err = read_token(&loose).expect_err("world-readable file");
        let text = format!("{err:#}");
        assert!(text.contains("readable by other users"), "{text}");
        assert!(text.contains("chmod 600"), "{text}");
    }
}
