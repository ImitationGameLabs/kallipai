//! The instance side of `kallipctl polis`: one running tagma's own `/polis`
//! faces, which are about the entries inside that instance rather than about
//! the platforms this host knows.
//!
//! Reaching it takes two hops. The daemon answers where the instance is (the
//! port it listens on, the data directory it was launched into); the instance
//! then answers the rest over loopback HTTP carrying its operator token. Both
//! hops stay inside one user's trust: the daemon socket is 0600, and the
//! operator token is a file only that user reads. Nothing here is a second
//! authorization story to keep in step with the tagma's own.

use std::time::Duration;

use anyhow::{Context as _, Result};
use kallipai_common::authtoken::OPERATOR_TOKEN_ENV_KEY;
use kallipai_daemon_client::DaemonClient;
use kallipai_daemon_common::wire::{InstanceState, OkPayload, RequestBody, ResponseBody};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// How long one instance call may take. A write can wait behind three things:
/// another writer's turn at the entries file, the platform's enrollment (its
/// archeion client caps a request at 30 s), and a displaced relay's reply
/// (30 s). A shorter deadline would report failure while the entry was still
/// landing.
const DEADLINE: Duration = Duration::from_secs(120);

/// A tagma instance this CLI can talk to: where it listens and the token its
/// operator faces accept.
pub(crate) struct Instance {
    slug: String,
    base: String,
    token: String,
    deadline: Duration,
}

/// One row of `GET /polis`, as the instance reports it. `enrolled` is what
/// separates an entry with credentials from one still waiting for them.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Row {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) enrolled: bool,
    #[serde(default)]
    pub(crate) tagma_id: Option<String>,
    pub(crate) relay: bool,
    pub(crate) gateway: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Listing {
    pub(crate) entries: Vec<Row>,
    /// True when the entries come from the instance environment: reads work,
    /// writes are refused by the instance.
    #[serde(default)]
    pub(crate) sugar: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Added {
    pub(crate) entry: Row,
    pub(crate) note: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Removed {
    pub(crate) name: String,
    pub(crate) relay: String,
    pub(crate) credentials: String,
    pub(crate) note: String,
}

/// The tagma's error body: the wire format wraps the operator's sentence in an
/// `error` object, the envelope every other tagma route sends. A parser
/// reading the top level would degrade every refusal to raw JSON.
#[derive(Debug, Deserialize)]
struct Refusal {
    error: RefusalBody,
}

#[derive(Debug, Deserialize)]
struct RefusalBody {
    message: String,
    /// Machine-readable reason, absent where the status line says enough.
    #[serde(default)]
    code: Option<String>,
}

/// Find the instance and the credential that talks to it.
pub(crate) async fn locate(client: &DaemonClient, slug: &str) -> Result<Instance> {
    let listed = call(client, RequestBody::List, slug).await?;
    let OkPayload::List { instances } = listed else {
        return Err(unexpected(slug, &listed));
    };
    let Some(info) = instances.iter().find(|info| info.slug == slug) else {
        anyhow::bail!(
            "no instance named {slug}; `kallipctl list` shows the ones this daemon manages"
        );
    };
    if info.state != InstanceState::Running {
        anyhow::bail!(
            "{slug} is {}; start it with `kallipctl start {slug}` and try again",
            info.state.as_str()
        );
    }
    let Some(port) = info.port else {
        anyhow::bail!(
            "{slug} is running, but the daemon published no port for it. `kallipctl restart {slug}` relaunches it."
        );
    };
    let token = match pinned_token(client, slug).await? {
        Some(token) => token,
        None => crate::token::read_operator_token_file(
            &crate::token::operator_token_path(client, slug).await?,
        )?,
    };
    Ok(Instance {
        slug: slug.to_owned(),
        base: format!("http://127.0.0.1:{port}"),
        token,
        deadline: DEADLINE,
    })
}

/// The token the instance was started with, when it was pinned in the
/// environment: a pin makes the token file go ignored, so the file would be
/// the wrong answer.
async fn pinned_token(client: &DaemonClient, slug: &str) -> Result<Option<String>> {
    let Ok(payload) = call(
        client,
        RequestBody::EnvGet {
            slug: slug.to_owned(),
        },
        slug,
    )
    .await
    else {
        // An instance whose env the daemon cannot read still has a token
        // file; the file read is the one that reports a real failure.
        return Ok(None);
    };
    let OkPayload::EnvGet { env, .. } = payload else {
        return Ok(None);
    };
    let prefix = format!("{OPERATOR_TOKEN_ENV_KEY}=");
    Ok(env
        .iter()
        .find_map(|pair| pair.strip_prefix(prefix.as_str()))
        .filter(|value| !value.is_empty())
        .map(str::to_owned))
}

/// One daemon question, with the answers that are not the payload turned into
/// the daemon's own sentence.
async fn call(client: &DaemonClient, request: RequestBody, slug: &str) -> Result<OkPayload> {
    let response = client
        .call(request)
        .await
        .context("talking to the kallipai daemon")?;
    match response.body {
        ResponseBody::Ok { payload } => Ok(payload),
        ResponseBody::Err { message, .. } => anyhow::bail!("cannot resolve {slug}: {message}"),
    }
}

fn unexpected(slug: &str, payload: &OkPayload) -> anyhow::Error {
    anyhow::anyhow!("unexpected daemon answer while resolving {slug}: {payload:?}")
}

impl Instance {
    /// The shared client, so these requests carry the same telemetry as the
    /// rest of the CLI, with the instance's own deadline.
    fn http(&self) -> Result<kallipai_common::telemetry::HttpClient> {
        kallipai_common::telemetry::http_client(reqwest::Client::builder().timeout(self.deadline))
            .context("building the HTTP client")
    }

    pub(crate) fn slug(&self) -> &str {
        &self.slug
    }

    pub(crate) async fn entries(&self) -> Result<Listing> {
        let url = format!("{}/polis", self.base);
        let response = self
            .http()?
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|err| self.transport(err, &url, CHECK_FIRST))?;
        answer(&self.slug, CHECK_FIRST, response).await
    }

    /// One write against the instance's entry file. `replace` is the one
    /// case where a code may arrive for an entry that already holds
    /// credentials: the instance then enrolls again and swaps the stored
    /// pair, instead of refusing the code as spent on a live registration.
    pub(crate) async fn add(
        &self,
        name: &str,
        url: &str,
        code: Option<&str>,
        replace: bool,
    ) -> Result<Added> {
        let endpoint = format!("{}/polis", self.base);
        let mut body = serde_json::json!({ "name": name, "url": url });
        if let Some(code) = code {
            body["code"] = serde_json::Value::String(code.to_owned());
        }
        if replace {
            body["replace"] = serde_json::Value::Bool(true);
        }
        let response = self
            .http()?
            .post(&endpoint)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|err| self.transport(err, &endpoint, MINT_FIRST))?;
        answer(&self.slug, MINT_FIRST, response).await
    }

    pub(crate) async fn remove(&self, name: &str, forget: bool) -> Result<Removed> {
        let endpoint = self.endpoint(&["polis", name])?;
        let response = self
            .http()?
            .delete(endpoint.clone())
            .query(&[("forget", forget)])
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|err| self.transport(err, endpoint.as_str(), CHECK_FIRST))?;
        answer(&self.slug, CHECK_FIRST, response).await
    }

    /// `{base}` with the segments pushed rather than concatenated, so a name
    /// carrying a slash or a dot cannot address a different route.
    fn endpoint(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.base)
            .with_context(|| format!("reading the address {}", self.base))?;
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("the address {} cannot take a path", self.base))?
            .extend(segments);
        Ok(url)
    }

    /// A deadline that fires says what to do next, because the instance may
    /// be finishing the very request the CLI gave up on. A connection that
    /// never opened says only that, since nothing was sent.
    fn transport(&self, err: reqwest_middleware::Error, url: &str, next: &str) -> anyhow::Error {
        let inner = match &err {
            reqwest_middleware::Error::Reqwest(inner) => Some(inner),
            reqwest_middleware::Error::Middleware(_) => None,
        };
        if inner.is_some_and(|inner| inner.is_timeout()) {
            anyhow::anyhow!(
                "{} did not answer within {} s; it may still be finishing the request. {} Then try again.",
                self.slug,
                self.deadline.as_secs(),
                next.replace("{slug}", &self.slug)
            )
        } else if inner.is_some_and(|inner| inner.is_connect()) {
            anyhow::anyhow!("nothing is listening at {url}: {err}")
        } else {
            anyhow::anyhow!("cannot reach {} at {url}: {err}", self.slug)
        }
    }
}

/// What to do when a read or a removal hit the deadline. `{slug}` is filled
/// in by the caller.
const CHECK_FIRST: &str = "Run `kallipctl tagma polis list {slug}` before anything else.";

/// The same, for the one call that may have spent an enrollment code.
const MINT_FIRST: &str = "Run `kallipctl tagma polis list {slug}` before anything else, and do not mint another code first: this one may already be spent.";

/// The instance's answer, or the sentence its refusal deserves. Every failure
/// after the request went out names what to check first, because it may have
/// landed.
async fn answer<T: DeserializeOwned>(
    slug: &str,
    next: &str,
    response: reqwest::Response,
) -> Result<T> {
    let next = next.replace("{slug}", slug);
    let status = response.status();
    let text = response.text().await.map_err(|err| {
        anyhow::anyhow!(
            "{slug}'s answer could not be read: {err}. It may still be finishing the request. {next}"
        )
    })?;
    if status.is_success() {
        return serde_json::from_str(&text)
            .with_context(|| format!("{slug}'s answer did not parse: {text}. {next}"));
    }
    let (message, code) = match serde_json::from_str::<Refusal>(&text) {
        Ok(refusal) => (refusal.error.message, refusal.error.code),
        Err(_) => (text.trim().to_owned(), None),
    };
    Err(match status.as_u16() {
        401 | 403 => anyhow::anyhow!(
            "the instance {slug} rejected the operator token: {message}. `kallipctl operator-token show {slug}` prints where the token comes from; an instance started with {OPERATOR_TOKEN_ENV_KEY} takes that pinned value and ignores the file"
        ),
        409 => anyhow::anyhow!("the instance refused: {message}"),
        _ => match code {
            Some(code) => anyhow::anyhow!("the instance answered {status} ({code}): {message}"),
            None => anyhow::anyhow!("the instance answered {status}: {message}"),
        },
    })
}

/// An instance whose base points at a listener a test owns.
#[cfg(test)]
pub(crate) fn stub(slug: &str, base: String, token: &str, deadline: Duration) -> Instance {
    Instance {
        slug: slug.to_owned(),
        base,
        token: token.to_owned(),
        deadline,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    /// One HTTP answer from a local socket, and the request it was given.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let answer = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            stream.write_all(answer.as_bytes()).expect("write");
            stream.flush().expect("flush");
            request
        });
        (base, handle)
    }

    /// Accept the connection and then say nothing: the shape a deadline that
    /// fires actually meets.
    fn serve_silently() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept");
            std::thread::sleep(Duration::from_secs(5));
        });
        base
    }

    #[tokio::test]
    async fn entries_reads_the_rows_and_carries_the_operator_token() {
        let body = r#"{"entries":[{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-1","relay":true,"gateway":false}],"sugar":false}"#;
        let (base, handle) = serve_once("200 OK", body);
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        let listing = instance.entries().await.expect("listing");
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "local");
        assert!(listing.entries[0].enrolled);
        assert_eq!(listing.entries[0].tagma_id.as_deref(), Some("t-1"));
        assert!(!listing.sugar);
        let request = handle.join().expect("stub");
        assert!(request.starts_with("GET /polis "), "{request}");
        assert!(
            request.contains("authorization: Bearer op-secret"),
            "the operator token rides the request: {request}"
        );
    }

    #[tokio::test]
    async fn add_sends_the_code_only_when_given() {
        let body = r#"{"entry":{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-1","relay":true,"gateway":true},"note":"enrolled"}"#;
        let (base, handle) = serve_once("201 Created", body);
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        let added = instance
            .add("local", "http://127.0.0.1:8080", Some("abc123"), false)
            .await
            .expect("added");
        assert_eq!(added.entry.name, "local");
        assert!(added.entry.gateway);
        let request = handle.join().expect("stub");
        assert!(request.starts_with("POST /polis "), "{request}");
        assert!(request.contains("\"code\":\"abc123\""), "{request}");
    }

    #[tokio::test]
    async fn a_replacing_write_says_so_in_the_body() {
        let body = r#"{"entry":{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-2","relay":true,"gateway":false},"note":"enrolled"}"#;
        let (base, handle) = serve_once("201 Created", body);
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        instance
            .add("local", "http://127.0.0.1:8080", Some("abc123"), true)
            .await
            .expect("replaced");
        let request = handle.join().expect("stub");
        assert!(request.contains("\"replace\":true"), "{request}");
    }

    #[tokio::test]
    async fn a_plain_write_leaves_the_replace_key_out() {
        let body = r#"{"entry":{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-1","relay":true,"gateway":false},"note":"enrolled"}"#;
        let (base, handle) = serve_once("201 Created", body);
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        instance
            .add("local", "http://127.0.0.1:8080", None, false)
            .await
            .expect("added");
        let request = handle.join().expect("stub");
        assert!(!request.contains("replace"), "{request}");
    }

    #[tokio::test]
    async fn a_rejected_token_names_the_ways_out() {
        let (base, handle) = serve_once(
            "401 Unauthorized",
            r#"{"error":{"message":"operator token required"}}"#,
        );
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        let err = instance.entries().await.expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("rejected the operator token"), "{text}");
        assert!(text.contains("operator-token show alpha"), "{text}");
        assert!(text.contains(OPERATOR_TOKEN_ENV_KEY), "{text}");
        handle.join().expect("stub");
    }

    #[tokio::test]
    async fn a_refusal_keeps_the_instance_s_own_sentence() {
        let body =
            r#"{"error":{"message":"the entry is created from the environment","code":"sugar"}}"#;
        let (base, handle) = serve_once("409 Conflict", body);
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        let err = instance.remove("local", false).await.expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("the instance refused"), "{text}");
        assert!(text.contains("created from the environment"), "{text}");
        let request = handle.join().expect("stub");
        assert!(
            request.starts_with("DELETE /polis/local?forget=false "),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_deadline_that_fires_says_what_to_do_next() {
        let base = serve_silently();
        let instance = stub("alpha", base, "op-secret", Duration::from_millis(150));
        let err = instance
            .add("local", "http://127.0.0.1:8080", None, false)
            .await
            .expect_err("deadline");
        let text = err.to_string();
        assert!(text.contains("did not answer within"), "{text}");
        assert!(text.contains("kallipctl tagma polis list alpha"), "{text}");
        assert!(text.contains("do not mint another code first"), "{text}");
    }

    #[tokio::test]
    async fn a_refusal_code_reaches_the_operator() {
        let body = r#"{"error":{"message":"the platform refused the enrollment","code":"polis_enrollment_not_stored"}}"#;
        let (base, handle) = serve_once("500 Internal Server Error", body);
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        let err = instance
            .add("local", "http://127.0.0.1:8080", Some("abc123"), false)
            .await
            .expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("polis_enrollment_not_stored"), "{text}");
        assert!(
            text.contains("the platform refused the enrollment"),
            "{text}"
        );
        handle.join().expect("stub");
    }

    #[tokio::test]
    async fn a_connection_that_never_opened_promises_no_landing() {
        // Bind and drop, so the port is free and nothing is listening.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        drop(listener);
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        let err = instance.entries().await.expect_err("unreachable");
        let text = err.to_string();
        assert!(text.contains("nothing is listening"), "{text}");
        assert!(!text.contains("may still be finishing"), "{text}");
    }

    #[tokio::test]
    async fn the_name_is_one_path_segment() {
        let (base, handle) = serve_once(
            "200 OK",
            r#"{"name":"a/b","removed":true,"relay":"stopped","credentials":"kept","note":""}"#,
        );
        let instance = stub("alpha", base, "op-secret", Duration::from_secs(5));
        instance.remove("a/b", false).await.expect("removed");
        let request = handle.join().expect("stub");
        assert!(request.starts_with("DELETE /polis/a%2Fb"), "{request}");
    }

    #[test]
    fn a_read_deadline_does_not_talk_about_minting() {
        assert!(!CHECK_FIRST.contains("mint"));
        assert!(MINT_FIRST.contains("do not mint another code first"));
    }
}
