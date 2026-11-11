//! The `polis` verbs: the platforms this host knows, and what a platform says
//! about what its owner has registered.
//!
//! The register is a file the operator owns (`polis-registry.toml` under the
//! config home, 0600). Nothing here goes through the daemon, so every verb
//! works while the daemon is down: those are the moments an operator wants to
//! look at a platform or free a pending enrollment slot.
//!
//! Each verb returns the lines it would print; [`run`] is the only place
//! that writes to stdout, which keeps the sentences testable without
//! capturing a process's stdout. The one warning about the environment
//! rather than about the sentence — the plaintext origin notice in
//! [`settle_origin`] — goes to stderr, so it still reaches the operator
//! when the verb fails.

use std::path::Path;

use anyhow::Result;

use kallipai_archeion_common::tagmata::{TagmaState, TagmaSummary};

use crate::archeion::{self, Listing, Revoked};
use crate::cli::{PolisCommand, PolisTokenCommand};
use crate::origin::Origin;
use crate::registry::{Addition, Registry};
use crate::token::read_token_value;

/// What a verb has to say: the lines to print, and a failure to report after
/// them. A rejected token is worth stating in place (`reachable` with
/// `token=rejected`), and the command still exits non-zero.
#[derive(Debug)]
struct Outcome {
    lines: Vec<String>,
    error: Option<anyhow::Error>,
}

impl Outcome {
    fn said(lines: Vec<String>) -> Self {
        Self { lines, error: None }
    }
}

/// Run one `polis` verb, printing what it has to say.
pub(crate) async fn run(command: &PolisCommand, allow_plaintext: bool) -> Result<()> {
    let outcome = lines(command, allow_plaintext).await?;
    for line in &outcome.lines {
        println!("{line}");
    }
    match outcome.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// What a verb says. A failure with nothing to print comes back as `Err`.
async fn lines(command: &PolisCommand, allow_plaintext: bool) -> Result<Outcome> {
    Ok(match command {
        PolisCommand::Add { name, url, replace } => {
            Outcome::said(add(name, url, *replace, allow_plaintext)?)
        }
        PolisCommand::Remove { name } => Outcome::said(remove(name)?),
        PolisCommand::List => Outcome::said(list()?),
        PolisCommand::Token { command } => match command {
            PolisTokenCommand::Set { name, token_file } => {
                Outcome::said(token_set(name, token_file.as_deref(), allow_plaintext).await?)
            }
        },
        PolisCommand::Status { name } => status(name, allow_plaintext).await?,
        PolisCommand::Revoke { name, tagma } => {
            Outcome::said(revoke(name, tagma, allow_plaintext).await?)
        }
    })
}

/// The address written down once, and a token that will meet it later: the
/// plaintext question is answered when the record is made, not at the first
/// request that spends the token.
fn settle_origin(name: &str, raw: &str, allow_plaintext: bool) -> Result<Origin> {
    let label = format!("polis {name}");
    let origin = Origin::parse(&label, raw)?;
    origin.check_plaintext(&label, allow_plaintext)?;
    if origin.is_plaintext() {
        eprintln!(
            "plaintext origin {}: the access token crosses the network in the clear",
            origin.as_str()
        );
    }
    Ok(origin)
}

fn add(name: &str, url: &str, replace: bool, allow_plaintext: bool) -> Result<Vec<String>> {
    let origin = settle_origin(name, url, allow_plaintext)?;
    let mut registry = Registry::load()?;
    let line = record(&mut registry, name, origin.as_str(), replace)?;
    registry.save()?;
    Ok(vec![line])
}

/// [`add`] without the file: the caller owns the register (and its path, when
/// a test needs one).
fn record(registry: &mut Registry, name: &str, origin: &str, replace: bool) -> Result<String> {
    let addition = registry.add(name, origin, replace)?;
    // The stored form, not the argument: that is what the next verb reads.
    let stored = registry.entry(name)?.url.clone();
    Ok(match addition {
        Addition::Added => format!("added {name} {stored}"),
        Addition::Replaced => format!("replaced {name}: now {stored}"),
        Addition::Unchanged => format!("{name} is already at {stored}"),
    })
}

fn remove(name: &str) -> Result<Vec<String>> {
    let mut registry = Registry::load()?;
    registry.remove(name)?;
    registry.save()?;
    Ok(vec![format!("removed {name}")])
}

fn list() -> Result<Vec<String>> {
    list_in(&Registry::load()?)
}

/// [`list`] against a register the caller already holds.
fn list_in(registry: &Registry) -> Result<Vec<String>> {
    if registry.is_empty() {
        return Ok(vec![
            "no polis is registered yet; add one with kallipctl polis add <name> <url>".to_owned(),
        ]);
    }
    let lines = registry
        .entries()
        .map(|(name, entry)| {
            let token = registry
                .masked(name)
                .unwrap_or_else(|| "missing".to_owned());
            format!("{name}  {}  token={token}", entry.url)
        })
        .collect();
    Ok(lines)
}

async fn token_set(
    name: &str,
    token_file: Option<&str>,
    allow_plaintext: bool,
) -> Result<Vec<String>> {
    let mut registry = Registry::load()?;
    token_set_in(&mut registry, name, token_file, allow_plaintext).await
}

/// [`token_set`] against a register the caller already holds. The record and
/// the address are resolved before the secret is read: a name that does not
/// exist, or an address the plaintext rule refuses, must not cost the operator
/// a paste of the real token.
async fn token_set_in(
    registry: &mut Registry,
    name: &str,
    token_file: Option<&str>,
    allow_plaintext: bool,
) -> Result<Vec<String>> {
    let address = registry.entry(name)?.url.clone();
    let origin = settle_origin(name, &address, allow_plaintext)?;
    let token = match token_file {
        Some(path) => archeion::read_token(Path::new(path))?,
        None => {
            let prompt = format!("Enter the access token for {name} (input hidden): ");
            read_token_value(&prompt).await?
        }
    };
    store(registry, name, origin.as_str(), &token).await
}

/// Store a token only after the platform accepts it for one list call: the
/// platform is the only judge of whether a token works, and a record that
/// cannot talk is worse than no record.
async fn store(
    registry: &mut Registry,
    name: &str,
    origin: &str,
    token: &str,
) -> Result<Vec<String>> {
    match archeion::list_tagmata(origin, token).await {
        Listing::Rows(_) => {}
        Listing::TokenRejected(err) | Listing::Failed(err) => return Err(err),
    }
    registry.set_secret(name, token)?;
    registry.save()?;
    Ok(vec![format!("stored token for {name}")])
}

async fn status(name: &str, allow_plaintext: bool) -> Result<Outcome> {
    let registry = Registry::load()?;
    report(&registry, name, allow_plaintext).await
}

/// [`status`] against a register the caller already holds.
async fn report(registry: &Registry, name: &str, allow_plaintext: bool) -> Result<Outcome> {
    let origin = settle_origin(name, &address(registry, name)?, allow_plaintext)?;
    let secret = token(registry, name)?;
    match archeion::list_tagmata(origin.as_str(), &secret).await {
        Listing::Rows(rows) => {
            let mut lines = vec![format!(
                "{name}  {}  reachable  token=accepted",
                origin.as_str()
            )];
            if rows.is_empty() {
                lines.push("no tagma is registered for this token".to_owned());
            }
            lines.extend(rows.iter().map(row_line));
            Ok(Outcome::said(lines))
        }
        Listing::TokenRejected(err) => {
            // The platform answered, so it is reachable; the token is what
            // needs fixing, and the error says how.
            Ok(Outcome {
                lines: vec![format!(
                    "{name}  {}  reachable  token=rejected",
                    origin.as_str()
                )],
                error: Some(err),
            })
        }
        Listing::Failed(err) => Err(err),
    }
}

async fn revoke(name: &str, tagma: &str, allow_plaintext: bool) -> Result<Vec<String>> {
    revoke_in(&Registry::load()?, name, tagma, allow_plaintext).await
}

/// [`revoke`] against a register the caller already holds.
async fn revoke_in(
    registry: &Registry,
    name: &str,
    tagma: &str,
    allow_plaintext: bool,
) -> Result<Vec<String>> {
    if !valid_tagma_id(tagma) {
        anyhow::bail!("{tagma:?} is not a tagma id; copy one from `kallipctl polis status {name}`");
    }
    let origin = settle_origin(name, &address(registry, name)?, allow_plaintext)?;
    let secret = token(registry, name)?;
    Ok(
        match archeion::revoke_tagma(origin.as_str(), &secret, tagma).await? {
            Revoked::Done => vec![format!("revoked {tagma} on {name}")],
            // The platform does not hold the id, which is the outcome this verb
            // asked for; a repeated revoke lands here too.
            Revoked::Absent => vec![format!("{tagma} is not registered on {name}")],
        },
    )
}

/// A tagma id travels into a request path unchanged, so a value that could
/// move the request somewhere else (`x/../users`) is refused here rather than
/// escaped later.
fn valid_tagma_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// The record's address, or the sentence that says how to make one.
fn address(registry: &Registry, name: &str) -> Result<String> {
    Ok(registry.entry(name)?.url.clone())
}

/// The record's token, or the sentence that says how to store one. Callers
/// read it after [`address`] has passed the plaintext rule: a token the
/// operator has just minted in a browser should not be thrown away over a
/// missing flag.
fn token(registry: &Registry, name: &str) -> Result<String> {
    registry.entry(name)?.secret.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "no token stored for {name}; store one with `kallipctl polis token set {name}`"
        )
    })
}

/// One platform row: the id first, so it can be copied straight into
/// `polis revoke`, then the state and the timestamp that state makes matter.
fn row_line(row: &TagmaSummary) -> String {
    let state = row.state.as_str();
    match row.state {
        TagmaState::Pending => {
            let expires = row
                .expires_at
                .as_ref()
                .map(stamp)
                .unwrap_or_else(|| "-".to_owned());
            format!(
                "{}  {state}  created={}  expires={expires}",
                row.tagma_id,
                stamp(&row.created_at)
            )
        }
        TagmaState::Enrolled | TagmaState::Unknown => format!(
            "{}  {state}  created={}",
            row.tagma_id,
            stamp(&row.created_at)
        ),
    }
}

/// The platform's own timestamp format, so a printed value can be pasted into
/// a request or compared with one.
fn stamp(at: &time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    /// A register holding `name` at `url`, with a token when one is given, on
    /// a path the test owns.
    fn register(dir: &tempfile::TempDir, name: &str, url: &str, token: Option<&str>) -> Registry {
        let path = dir.path().join("polis-registry.toml");
        let mut registry = Registry::load_from(&path).expect("an empty register");
        registry.add(name, url, false).expect("add");
        if let Some(token) = token {
            registry.set_secret(name, token).expect("token");
        }
        registry.save().expect("save");
        registry
    }

    /// One-shot HTTP server answering a fixed status and body, and handing back
    /// the request it read: the client only needs a socket, and the captured
    /// request pins the path and the bearer.
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

    #[test]
    fn add_records_the_normalized_address() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = Registry::load_from(&dir.path().join("polis-registry.toml"))
            .expect("an empty register");
        let line =
            record(&mut registry, "local", "HTTP://API.example:8080/", false).expect("recorded");
        assert_eq!(line, "added local http://api.example:8080");
        registry.save().expect("save");
        let reloaded = Registry::load_from(registry.path()).expect("reloaded");
        assert_eq!(
            reloaded.entry("local").expect("record").url,
            "http://api.example:8080"
        );
    }

    #[test]
    fn an_unchanged_pair_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = register(&dir, "local", "http://127.0.0.1:8080", None);
        let line =
            record(&mut registry, "local", "http://127.0.0.1:8080", false).expect("unchanged");
        assert_eq!(line, "local is already at http://127.0.0.1:8080");
    }

    #[test]
    fn a_new_address_for_a_known_name_needs_replace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = register(&dir, "local", "http://127.0.0.1:8080", None);
        let error = record(&mut registry, "local", "https://api.example.com", false)
            .expect_err("refused without the flag");
        assert!(format!("{error:#}").contains("--replace"), "{error:#}");
        let line =
            record(&mut registry, "local", "https://api.example.com", true).expect("replaced");
        assert_eq!(line, "replaced local: now https://api.example.com");
    }

    #[test]
    fn a_plaintext_address_needs_the_flag() {
        let refused = settle_origin("local", "http://api.example.com", false)
            .expect_err("off loopback, plain http");
        assert!(
            format!("{refused:#}").contains("--allow-plaintext"),
            "{refused:#}"
        );
        let allowed = settle_origin("local", "http://api.example.com", true).expect("allowed");
        assert!(allowed.is_plaintext());
        // Loopback is not plaintext traffic in the sense that matters: the
        // bytes never leave the host.
        let local = settle_origin("local", "http://127.0.0.1:8080", false).expect("loopback");
        assert!(!local.is_plaintext());
    }

    #[test]
    fn list_masks_a_token_and_says_when_there_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(
            &dir,
            "local",
            "http://127.0.0.1:8080",
            Some("sk-pat-abcdefghijkl"),
        );
        let lines = list_in(&registry).expect("listed");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("local  http://127.0.0.1:8080  token="),
            "{lines:?}"
        );
        assert!(
            !lines[0].contains("abcdefghijkl"),
            "the secret must not print: {lines:?}"
        );

        let bare = register(&dir, "cloud", "https://api.example.com", None);
        let lines = list_in(&bare).expect("listed");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("cloud  https://api.example.com  token=missing")),
            "{lines:?}"
        );
    }

    #[test]
    fn an_empty_register_says_how_to_add_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::load_from(&dir.path().join("none.toml")).expect("empty");
        let lines = list_in(&registry).expect("listed");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("kallipctl polis add"), "{lines:?}");
    }
    #[tokio::test]
    async fn status_numbers_the_platform_rows() {
        let (polis, request) = serve_once(
            200,
            r#"[{"tagma_id":"tagma-1","state":"enrolled","created_at":"2026-01-01T00:00:00Z"},{"tagma_id":"tagma-2","state":"pending","created_at":"2026-01-02T00:00:00Z","code_masked":"sk-enroll-abc***xyz","expires_at":"2026-01-02T00:10:00Z"}]"#,
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", &polis, Some("sk-pat-token"));
        let outcome = report(&registry, "local", true).await.expect("status");
        assert!(
            outcome.error.is_none(),
            "the rows came back, so nothing failed"
        );
        let lines = outcome.lines;
        assert_eq!(
            lines[0],
            format!("local  {polis}  reachable  token=accepted")
        );
        assert_eq!(lines[1], "tagma-1  enrolled  created=2026-01-01T00:00:00Z");
        assert_eq!(
            lines[2],
            "tagma-2  pending  created=2026-01-02T00:00:00Z  expires=2026-01-02T00:10:00Z"
        );
        let request = request.join().expect("server thread");
        assert!(
            request.starts_with("GET /v1/archeion/tagmata "),
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
    async fn a_rejected_token_reads_as_reachable() {
        let (polis, request) = serve_once(401, r#"{"error":{"message":"the token was revoked"}}"#);
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", &polis, Some("sk-pat-token"));
        let outcome = report(&registry, "local", true).await.expect("status");
        assert_eq!(
            outcome.lines[0],
            format!("local  {polis}  reachable  token=rejected")
        );
        let error = outcome.error.expect("a rejected token is a failure");
        assert!(
            format!("{error:#}").contains("rejected the access token"),
            "{error:#}"
        );
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn status_without_a_token_names_the_verb_that_stores_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", "http://127.0.0.1:9", None);
        let error = report(&registry, "local", false)
            .await
            .expect_err("no token");
        assert!(
            format!("{error:#}").contains("kallipctl polis token set local"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_refused_token_is_not_stored() {
        let (polis, request) = serve_once(401, r#"{"error":{"message":"the token was revoked"}}"#);
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = register(&dir, "local", &polis, None);
        let before = std::fs::read_to_string(registry.path()).expect("read");
        let error = store(&mut registry, "local", &polis, "sk-pat-bad")
            .await
            .expect_err("refused");
        assert!(
            format!("{error:#}").contains("rejected the access token"),
            "{error:#}"
        );
        assert_eq!(registry.entry("local").expect("record").secret, None);
        let after = std::fs::read_to_string(registry.path()).expect("read");
        assert_eq!(before, after, "a refused token leaves the file alone");
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn an_accepted_token_is_stored() {
        let (polis, request) = serve_once(200, "[]");
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = register(&dir, "local", &polis, None);
        let lines = store(&mut registry, "local", &polis, "sk-pat-good")
            .await
            .expect("stored");
        assert_eq!(lines, vec!["stored token for local".to_owned()]);
        let reloaded = Registry::load_from(registry.path()).expect("reloaded");
        assert_eq!(
            reloaded.entry("local").expect("record").secret.as_deref(),
            Some("sk-pat-good")
        );
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn an_unknown_id_reads_as_not_registered() {
        let (polis, request) = serve_once(404, r#"{"error":{"message":"unknown tagma"}}"#);
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", &polis, Some("sk-pat-token"));
        let lines = revoke_in(&registry, "local", "tagma-9", true)
            .await
            .expect("an unknown id is not a failure");
        assert_eq!(lines, vec!["tagma-9 is not registered on local".to_owned()]);
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn status_prints_an_empty_table_without_failing() {
        let (polis, request) = serve_once(200, "[]");
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", &polis, Some("sk-pat-token"));
        let outcome = report(&registry, "local", true).await.expect("status");
        assert!(outcome.error.is_none(), "an empty list is not a failure");
        assert_eq!(outcome.lines.len(), 2, "{:?}", outcome.lines);
        assert_eq!(outcome.lines[1], "no tagma is registered for this token");
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn a_revoke_names_the_id_it_deleted() {
        let (polis, request) = serve_once(204, "");
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", &polis, Some("sk-pat-token"));
        let lines = revoke_in(&registry, "local", "tagma-7", true)
            .await
            .expect("revoked");
        assert_eq!(lines, vec!["revoked tagma-7 on local".to_owned()]);
        let request = request.join().expect("server thread");
        assert!(
            request.starts_with("DELETE /v1/archeion/tagmata/tagma-7 "),
            "{request}"
        );
    }

    #[tokio::test]
    async fn an_unrecognized_state_still_prints_the_row() {
        let (polis, request) = serve_once(
            200,
            r#"[{"tagma_id":"tagma-3","state":"draining","created_at":"2026-01-03T00:00:00Z"}]"#,
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", &polis, Some("sk-pat-token"));
        let outcome = report(&registry, "local", true).await.expect("status");
        assert!(
            outcome.error.is_none(),
            "one unknown word is not a decode failure"
        );
        assert_eq!(
            outcome.lines[1],
            "tagma-3  unknown  created=2026-01-03T00:00:00Z"
        );
        request.join().expect("server thread");
    }

    #[tokio::test]
    async fn a_bad_name_or_address_is_refused_before_the_token_file_is_opened() {
        let missing = "/nonexistent/kallipctl-token";
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = register(&dir, "cloud", "https://api.example.com", None);
        let error = token_set_in(&mut registry, "ghost", Some(missing), false)
            .await
            .expect_err("an unknown name");
        assert!(
            format!("{error:#}").contains("no polis named ghost"),
            "{error:#}"
        );

        let mut registry = register(&dir, "plain", "http://api.example.com", None);
        let error = token_set_in(&mut registry, "plain", Some(missing), false)
            .await
            .expect_err("a plaintext origin off loopback");
        assert!(
            format!("{error:#}").contains("--allow-plaintext"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_tagma_id_that_could_move_the_request_path_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", "http://127.0.0.1:9", Some("sk-pat-token"));
        let error = revoke_in(&registry, "local", "x/../users", true)
            .await
            .expect_err("a path, not an id");
        assert!(
            format!("{error:#}").contains("is not a tagma id"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn the_address_rule_speaks_before_a_missing_token_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "plain", "http://api.example.com", None);
        let error = report(&registry, "plain", false)
            .await
            .expect_err("a plaintext origin and no token");
        assert!(
            format!("{error:#}").contains("--allow-plaintext"),
            "{error:#}"
        );

        let error = revoke_in(&registry, "plain", "tagma-1", false)
            .await
            .expect_err("a plaintext origin and no token");
        assert!(
            format!("{error:#}").contains("--allow-plaintext"),
            "{error:#}"
        );
    }
}
