//! The `polis` verbs: the platforms this host knows, and what a platform says
//! about what its owner has registered.
//!
//! The register is a file the operator owns (`polis-registry.toml` under the
//! config home, 0600). The register verbs read and write it without the
//! daemon, so they work while it is down: those are the moments an operator
//! wants to look at a platform or free a pending enrollment slot. The three
//! instance verbs are the exception: they ask the daemon where the instance
//! listens, which is what says where its operator token lives.
//!
//! Each verb returns the lines it would print; [`say`] is the only place
//! that writes to stdout, which keeps the sentences testable without
//! capturing a process's stdout. Two facts go to stderr instead, because
//! they outlive the run that would have reported them, whether or not it
//! fails: the plaintext origin notice in [`settle_origin`], and the
//! pending tagma a minted code leaves on the platform.
//!
//! The answering functions take their dependency as a parameter: a name
//! ending in `_in` (`list_platforms_in`, `set_token_in`, `entries_in`, ...)
//! is the verb handed the register or the instance instead of loading one,
//! so a test can drive it against a temporary file or a stub. A helper
//! that takes the dependency as a parameter carries no suffix either: it
//! is a piece of a verb, not another version of one.

use std::path::Path;

use anyhow::Result;

use kallipai_archeion_common::tagmata::{TagmaState, TagmaSummary};
use kallipai_daemon_client::DaemonClient;

use crate::archeion::{self, Listing, Revoked};
use crate::cli::{InstanceVerb, RegisterVerb};
use crate::instance::{self, Instance};
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

/// Run one register verb, printing what it has to say.
pub(crate) async fn run_register(verb: &RegisterVerb<'_>, allow_plaintext: bool) -> Result<()> {
    say(register_lines(verb, allow_plaintext).await?)
}

/// Run one of the verbs that speaks to a running instance. The daemon has
/// already said where it is, so there are two hops: ask the instance, then
/// print.
pub(crate) async fn run_instance(
    verb: &InstanceVerb<'_>,
    allow_plaintext: bool,
    daemon: &DaemonClient,
) -> Result<()> {
    say(instance_lines(verb, allow_plaintext, daemon).await?)
}

/// The one place that writes to stdout.
fn say(outcome: Outcome) -> Result<()> {
    for line in &outcome.lines {
        println!("{line}");
    }
    match outcome.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// What a register verb says. A failure with nothing to print comes back
/// as `Err`.
async fn register_lines(verb: &RegisterVerb<'_>, allow_plaintext: bool) -> Result<Outcome> {
    Ok(match verb {
        RegisterVerb::Add { name, url, replace } => {
            Outcome::said(add_platform(name, url, *replace, allow_plaintext)?)
        }
        RegisterVerb::Remove { name } => Outcome::said(remove_platform(name)?),
        RegisterVerb::List => Outcome::said(list_platforms()?),
        RegisterVerb::SetToken { name, token_file } => {
            Outcome::said(set_token(name, *token_file, allow_plaintext).await?)
        }
        RegisterVerb::Status { name } => platform_status(name, allow_plaintext).await?,
        RegisterVerb::Revoke { name, tagma } => {
            Outcome::said(revoke_tagma(name, tagma, allow_plaintext).await?)
        }
        RegisterVerb::MintCode { platform } => {
            Outcome::said(mint_code(platform, allow_plaintext).await?)
        }
    })
}

/// What one of the instance verbs says. The instance is located once, from
/// the verb's own slug: the daemon knows its port and its data directory,
/// which is where the operator token lives.
async fn instance_lines(
    verb: &InstanceVerb<'_>,
    allow_plaintext: bool,
    daemon: &DaemonClient,
) -> Result<Outcome> {
    // The cheap check the other verbs get in `main.rs`: the same grammar the
    // daemon enforces, caught here because this path never reaches it.
    if !kallipai_daemon_common::wire::valid_slug(verb.slug()) {
        anyhow::bail!(
            "slug {:?} does not match [a-z0-9][a-z0-9-]* (max 64 chars)",
            verb.slug()
        );
    }
    let instance = instance::locate(daemon, verb.slug()).await?;
    Ok(match verb {
        InstanceVerb::Entries { .. } => Outcome::said(entries_in(&instance).await?),
        InstanceVerb::AddEntry {
            name,
            polis,
            url,
            code,
            token_file,
            ..
        } => Outcome::said(
            add_entry_in(
                &Registry::load()?,
                &instance,
                name,
                Target::pick(*polis, *url)?,
                Ask {
                    code: *code,
                    token_file: *token_file,
                },
                allow_plaintext,
            )
            .await?,
        ),
        InstanceVerb::RemoveEntry { name, forget, .. } => {
            Outcome::said(remove_entry_in(&instance, name, *forget).await?)
        }
    })
}

/// The entries inside the instance, one line each, the name first so it can
/// be copied into `remove-entry`.
async fn entries_in(instance: &Instance) -> Result<Vec<String>> {
    let slug = instance.slug();
    let listing = instance.entries().await?;
    let mut lines = Vec::new();
    if listing.sugar {
        lines.push(format!(
            "{slug} takes its entries from the environment, so they cannot be added or removed here"
        ));
    }
    if listing.entries.is_empty() {
        lines.push(format!(
            "no entries in {slug}; add one with `kallipctl polis add-entry {slug} <name> --polis <platform>`"
        ));
        return Ok(lines);
    }
    lines.extend(listing.entries.iter().map(entry_line));
    Ok(lines)
}

/// One entry: what it is, whether it holds credentials, and the two things
/// that can be down independently of each other.
fn entry_line(row: &instance::Row) -> String {
    format!(
        "{}  relay={}  gateway={}",
        entry_head(row),
        on_off(row.relay),
        on_off(row.gateway)
    )
}

/// A run state as a word: the relay is on or off, and `relay=true` reads
/// like the value of something rather than a state.
fn on_off(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

/// One entry without the two run states: the head of an add-entry report.
fn entry_head(row: &instance::Row) -> String {
    format!(
        "{}  {}  enrolled={}  tagma={}",
        row.name,
        row.url,
        on_off(row.enrolled),
        row.tagma_id.as_deref().unwrap_or("-")
    )
}

/// The cheap check both entry verbs make: an entry name is a slug, the grammar
/// the instance enforces, and a name that is not one addresses a different
/// path than the caller means.
///
/// The authority is the tagma's `valid_entry_name`; this copy may only be
/// looser than it, so an instance that relaxes its grammar leaves this check
/// passing what the instance then accepts. Tightening here without the tagma
/// would refuse a name the instance would have taken.
fn check_entry_name(name: &str) -> Result<()> {
    if !kallipai_daemon_common::wire::valid_slug(name) {
        anyhow::bail!("entry name {name:?} does not match [a-z0-9][a-z0-9-]* (max 64 chars)");
    }
    Ok(())
}

/// Where the entry points: a platform from the register, or an origin given
/// outright.
#[derive(Debug)]
enum Target<'a> {
    Platform(&'a str),
    Url(&'a str),
}

impl<'a> Target<'a> {
    /// The one reading of `--polis` / `--url`: clap refuses both at once, and
    /// neither is a mistake worth a round trip.
    fn pick(polis: Option<&'a str>, url: Option<&'a str>) -> Result<Self> {
        match (polis, url) {
            (Some(platform), _) => Ok(Self::Platform(platform)),
            (None, Some(url)) => Ok(Self::Url(url)),
            (None, None) => anyhow::bail!(
                "say which platform this entry is for: --polis <name> from the register, or --url <origin>"
            ),
        }
    }

    fn platform(&self) -> Option<&'a str> {
        match self {
            Self::Platform(platform) => Some(platform),
            Self::Url(_) => None,
        }
    }
}

/// What the caller brought to spend: a code to redeem, or the token file to
/// mint with.
struct Ask<'a> {
    code: Option<&'a str>,
    token_file: Option<&'a str>,
}

/// Add one entry: settle the target, make the checks the instance would make,
/// mint a code unless one arrives, and let the instance enroll and start the
/// relay in that one request. A code minted here never reaches a shell
/// history, and its thirty-minute window is spent inside this call rather
/// than becoming the operator's problem; a code the caller brings in is one
/// they have already carried.
async fn add_entry_in(
    registry: &Registry,
    instance: &Instance,
    name: &str,
    target: Target<'_>,
    ask: Ask<'_>,
    allow_plaintext: bool,
) -> Result<Vec<String>> {
    check_entry_name(name)?;
    let from_register = target.platform();
    let (origin, stored) = match target {
        Target::Platform(platform) => (
            settle_origin(
                platform,
                &platform_address(registry, platform)?,
                allow_plaintext,
            )?,
            registry.entry(platform)?.secret.clone(),
        ),
        // The judgment a recorded address gets: a long-lived token travels
        // with this origin, so a plaintext one needs the flag.
        Target::Url(url) => (settle_origin("--url", url, allow_plaintext)?, None),
    };
    let code = match ask.code {
        Some(code) => code.to_owned(),
        None => {
            // The token is read before the instance is asked, so a bad path
            // costs no round trip.
            let token = match ask.token_file {
                Some(path) => Some(archeion::read_token(Path::new(path))?),
                None => stored,
            };
            // The instance validates before it makes a network call. The
            // checks it can make without one happen here too, so a typo or a
            // name it already holds costs no enrollment. This holds only
            // while the instance refuses a stored entry before it redeems
            // the code; an external code is not checked here at all.
            let listing = instance.entries().await?;
            refuse_a_hopeless_add(instance, name, &listing)?;
            let Some(token) = token else {
                match from_register {
                    Some(platform) => anyhow::bail!(
                        "no token to mint a code for {} with: pass --code, store one with `kallipctl polis token set {platform}`, or pass --token-file",
                        origin.as_str()
                    ),
                    None => anyhow::bail!(
                        "no token to mint a code for {} with: pass --code or --token-file",
                        origin.as_str()
                    ),
                }
            };
            let minted = archeion::mint_enrollment(origin.as_str(), &token).await?;
            // The code enrolls a pending tagma the platform has already made.
            // The read above and the write below are two calls, and another
            // writer can take the name in between; its id is what a failure
            // then leaves the operator to revoke.
            eprintln!(
                "pending tagma {} (code expires at {})",
                minted.id, minted.expires_at
            );
            minted.code
        }
    };
    let added = instance.add(name, origin.as_str(), Some(&code)).await?;
    Ok(vec![entry_head(&added.entry), added.note])
}

/// The refusals the instance would answer with after the request, made before
/// one: an environment-configured instance takes no writes, and a name it
/// already holds credentials for is a refusal.
fn refuse_a_hopeless_add(
    instance: &Instance,
    name: &str,
    listing: &instance::Listing,
) -> Result<()> {
    let slug = instance.slug();
    if listing.sugar {
        anyhow::bail!(
            "{slug} takes its entries from the environment, so they cannot be added here"
        );
    }
    // Only an entry that holds credentials is a dead end: one without them
    // is exactly what a code repairs, and the instance updates it in place.
    if listing
        .entries
        .iter()
        .any(|row| row.name == name && row.enrolled)
    {
        anyhow::bail!(
            "{slug} already has an entry named {name}; `kallipctl polis remove-entry {slug} {name}` drops it first"
        );
    }
    Ok(())
}

async fn remove_entry_in(instance: &Instance, name: &str, forget: bool) -> Result<Vec<String>> {
    check_entry_name(name)?;
    let removed = instance.remove(name, forget).await?;
    Ok(vec![
        format!("{} removed", removed.name),
        format!("relay={}", removed.relay),
        format!("credentials={}", removed.credentials),
        removed.note,
    ])
}

async fn mint_code(platform: &str, allow_plaintext: bool) -> Result<Vec<String>> {
    mint_code_in(&Registry::load()?, platform, allow_plaintext).await
}

/// Mint a code here and print it with its expiry: the hand-carried path,
/// for an instance this CLI cannot reach.
async fn mint_code_in(
    registry: &Registry,
    platform: &str,
    allow_plaintext: bool,
) -> Result<Vec<String>> {
    let origin = settle_origin(
        platform,
        &platform_address(registry, platform)?,
        allow_plaintext,
    )?;
    let token = stored_token(registry, platform)?;
    let minted = archeion::mint_enrollment(origin.as_str(), &token).await?;
    Ok(vec![
        format!("code {}  expires {}", minted.code, minted.expires_at),
        format!(
            "pending tagma {}: `kallipctl polis revoke {platform} {}` frees it while the code is unused",
            minted.id, minted.id
        ),
        format!(
            "redeem it on the instance within that lifetime: `kallipctl polis add-entry <slug> <name> --polis {platform} --code <code>`"
        ),
    ])
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

fn add_platform(
    name: &str,
    url: &str,
    replace: bool,
    allow_plaintext: bool,
) -> Result<Vec<String>> {
    let origin = settle_origin(name, url, allow_plaintext)?;
    let mut registry = Registry::load()?;
    let line = add_platform_in(&mut registry, name, origin.as_str(), replace)?;
    registry.save()?;
    Ok(vec![line])
}

/// [`add_platform`] without the file: the caller owns the register (and its path, when
/// a test needs one).
fn add_platform_in(
    registry: &mut Registry,
    name: &str,
    origin: &str,
    replace: bool,
) -> Result<String> {
    let addition = registry.add(name, origin, replace)?;
    // The stored form, not the argument: that is what the next verb reads.
    let stored = registry.entry(name)?.url.clone();
    Ok(match addition {
        Addition::Added => format!("added {name} {stored}"),
        Addition::Replaced => format!("replaced {name}: now {stored}"),
        Addition::Unchanged => format!("{name} is already at {stored}"),
    })
}

fn remove_platform(name: &str) -> Result<Vec<String>> {
    let mut registry = Registry::load()?;
    registry.remove(name)?;
    registry.save()?;
    Ok(vec![format!("removed {name}")])
}

fn list_platforms() -> Result<Vec<String>> {
    list_platforms_in(&Registry::load()?)
}

/// [`list_platforms`] against a register the caller already holds.
fn list_platforms_in(registry: &Registry) -> Result<Vec<String>> {
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

async fn set_token(
    name: &str,
    token_file: Option<&str>,
    allow_plaintext: bool,
) -> Result<Vec<String>> {
    let mut registry = Registry::load()?;
    set_token_in(&mut registry, name, token_file, allow_plaintext).await
}

/// [`set_token`] against a register the caller already holds. The record and
/// the address are resolved before the secret is read: a name that does not
/// exist, or an address the plaintext rule refuses, must not cost the operator
/// a paste of the real token.
async fn set_token_in(
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
    store_token(registry, name, origin.as_str(), &token).await
}

/// Store a token only after the platform accepts it for one list call: the
/// platform is the only judge of whether a token works, and a record that
/// cannot talk is worse than no record.
async fn store_token(
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

async fn platform_status(name: &str, allow_plaintext: bool) -> Result<Outcome> {
    let registry = Registry::load()?;
    platform_status_in(&registry, name, allow_plaintext).await
}

/// [`platform_status`] against a register the caller already holds.
async fn platform_status_in(
    registry: &Registry,
    name: &str,
    allow_plaintext: bool,
) -> Result<Outcome> {
    let origin = settle_origin(name, &platform_address(registry, name)?, allow_plaintext)?;
    let secret = stored_token(registry, name)?;
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

async fn revoke_tagma(name: &str, tagma: &str, allow_plaintext: bool) -> Result<Vec<String>> {
    revoke_tagma_in(&Registry::load()?, name, tagma, allow_plaintext).await
}

/// [`revoke_tagma`] against a register the caller already holds.
async fn revoke_tagma_in(
    registry: &Registry,
    name: &str,
    tagma: &str,
    allow_plaintext: bool,
) -> Result<Vec<String>> {
    if !valid_tagma_id(tagma) {
        anyhow::bail!("{tagma:?} is not a tagma id; copy one from `kallipctl polis status {name}`");
    }
    let origin = settle_origin(name, &platform_address(registry, name)?, allow_plaintext)?;
    let secret = stored_token(registry, name)?;
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
fn platform_address(registry: &Registry, name: &str) -> Result<String> {
    Ok(registry.entry(name)?.url.clone())
}

/// The record's token, or the sentence that says how to store one. Callers
/// read it after [`platform_address`] has passed the plaintext rule: a token the
/// operator has just minted in a browser should not be thrown away over a
/// missing flag.
fn stored_token(registry: &Registry, name: &str) -> Result<String> {
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

    /// Answers in order, and every request read: the pre-mint read and the add
    /// that follows are two calls to the same instance.
    fn serve_answers(
        answers: Vec<(u16, &'static str)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in answers {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut request = [0u8; 4096];
                let read = stream.read(&mut request).expect("read the request");
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).expect("write");
                requests.push(String::from_utf8_lossy(&request[..read]).into_owned());
            }
            requests
        });
        (format!("http://{addr}"), handle)
    }

    fn instance_answers(
        slug: &str,
        answers: Vec<(u16, &'static str)>,
    ) -> (Instance, std::thread::JoinHandle<Vec<String>>) {
        let (base, handle) = serve_answers(answers);
        let instance = instance::stub(slug, base, "op-secret", std::time::Duration::from_secs(5));
        (instance, handle)
    }

    #[test]
    fn add_records_the_normalized_address() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = Registry::load_from(&dir.path().join("polis-registry.toml"))
            .expect("an empty register");
        let line = add_platform_in(&mut registry, "local", "HTTP://API.example:8080/", false)
            .expect("recorded");
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
        let line = add_platform_in(&mut registry, "local", "http://127.0.0.1:8080", false)
            .expect("unchanged");
        assert_eq!(line, "local is already at http://127.0.0.1:8080");
    }

    #[test]
    fn a_new_address_for_a_known_name_needs_replace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = register(&dir, "local", "http://127.0.0.1:8080", None);
        let error = add_platform_in(&mut registry, "local", "https://api.example.com", false)
            .expect_err("refused without the flag");
        assert!(format!("{error:#}").contains("--replace"), "{error:#}");
        let line = add_platform_in(&mut registry, "local", "https://api.example.com", true)
            .expect("replaced");
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
        let lines = list_platforms_in(&registry).expect("listed");
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
        let lines = list_platforms_in(&bare).expect("listed");
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
        let lines = list_platforms_in(&registry).expect("listed");
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
        let outcome = platform_status_in(&registry, "local", true)
            .await
            .expect("status");
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
        let outcome = platform_status_in(&registry, "local", true)
            .await
            .expect("status");
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
        let error = platform_status_in(&registry, "local", false)
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
        let error = store_token(&mut registry, "local", &polis, "sk-pat-bad")
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
        let lines = store_token(&mut registry, "local", &polis, "sk-pat-good")
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
        let lines = revoke_tagma_in(&registry, "local", "tagma-9", true)
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
        let outcome = platform_status_in(&registry, "local", true)
            .await
            .expect("status");
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
        let lines = revoke_tagma_in(&registry, "local", "tagma-7", true)
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
        let outcome = platform_status_in(&registry, "local", true)
            .await
            .expect("status");
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
        let error = set_token_in(&mut registry, "ghost", Some(missing), false)
            .await
            .expect_err("an unknown name");
        assert!(
            format!("{error:#}").contains("no polis named ghost"),
            "{error:#}"
        );

        let mut registry = register(&dir, "plain", "http://api.example.com", None);
        let error = set_token_in(&mut registry, "plain", Some(missing), false)
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
        let error = revoke_tagma_in(&registry, "local", "x/../users", true)
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
        let error = platform_status_in(&registry, "plain", false)
            .await
            .expect_err("a plaintext origin and no token");
        assert!(
            format!("{error:#}").contains("--allow-plaintext"),
            "{error:#}"
        );

        let error = revoke_tagma_in(&registry, "plain", "tagma-1", false)
            .await
            .expect_err("a plaintext origin and no token");
        assert!(
            format!("{error:#}").contains("--allow-plaintext"),
            "{error:#}"
        );
    }

    /// An instance at a listener the test owns: the three instance verbs need
    /// nothing else.
    fn instance_at(
        slug: &str,
        status: u16,
        body: &'static str,
    ) -> (Instance, std::thread::JoinHandle<String>) {
        let (base, handle) = serve_once(status, body);
        let instance = instance::stub(slug, base, "op-secret", std::time::Duration::from_secs(5));
        (instance, handle)
    }

    #[tokio::test]
    async fn entries_lists_what_the_instance_holds() {
        let body = r#"{"entries":[{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-1","relay":true,"gateway":false}],"sugar":false}"#;
        let (instance, handle) = instance_at("alpha", 200, body);
        let lines = entries_in(&instance).await.expect("entries");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("local  http://127.0.0.1:8080  enrolled=on  tagma=t-1"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("relay=on"), "{}", lines[0]);
        assert!(lines[0].contains("gateway=off"), "{}", lines[0]);
        handle.join().expect("stub");
    }

    #[tokio::test]
    async fn an_empty_instance_names_the_verb_that_adds_one() {
        let (instance, handle) = instance_at("alpha", 200, r#"{"entries":[]}"#);
        let lines = entries_in(&instance).await.expect("entries");
        assert_eq!(
            lines,
            vec![
                "no entries in alpha; add one with `kallipctl polis add-entry alpha <name> --polis <platform>`".to_owned()
            ]
        );
        handle.join().expect("stub");
    }

    #[tokio::test]
    async fn an_environment_configured_instance_says_writes_are_refused() {
        let (instance, handle) = instance_at("alpha", 200, r#"{"entries":[],"sugar":true}"#);
        let lines = entries_in(&instance).await.expect("entries");
        assert!(
            lines[0].contains("takes its entries from the environment"),
            "{}",
            lines[0]
        );
        handle.join().expect("stub");
    }

    /// The code path reports the entry and the note, and it makes one request
    /// only: a code the caller already holds is not checked against a pre-read,
    /// since the instance refuses a stored name before it redeems the code.
    #[tokio::test]
    async fn add_entry_with_a_code_reports_the_entry_and_the_note() {
        let body = r#"{"entry":{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-1","relay":false,"gateway":true},"note":"the relay starts on the next boot"}"#;
        let (instance, handle) = instance_at("alpha", 201, body);
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::load_from(&dir.path().join("polis-registry.toml")).expect("empty");
        let lines = add_entry_in(
            &registry,
            &instance,
            "local",
            Target::Url("http://127.0.0.1:8080"),
            Ask {
                code: Some("code-1"),
                token_file: None,
            },
            false,
        )
        .await
        .expect("added");
        assert!(lines[0].ends_with("tagma=t-1"), "{}", lines[0]);
        assert!(
            !lines[0].contains("relay="),
            "the head carries no run states: {}",
            lines[0]
        );
        assert_eq!(lines.len(), 2, "the head and the note: {lines:?}");
        assert_eq!(lines[1], "the relay starts on the next boot");
        let request = handle.join().expect("stub");
        assert!(
            request.contains(r#"{"name":"local","url":"http://127.0.0.1:8080","code":"code-1"}"#),
            "{request}"
        );
    }

    #[tokio::test]
    async fn add_entry_mints_with_the_platform_token_the_register_holds() {
        let (platform, minted) = serve_once(
            200,
            r#"{"code":"sk-enroll-abc","id":"t1","created_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:10:00Z"}"#,
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", &platform, Some("sk-pat"));
        let body = r#"{"entry":{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-1","relay":true,"gateway":true},"note":"enrolled"}"#;
        let (instance, handle) =
            instance_answers("alpha", vec![(200, r#"{"entries":[]}"#), (201, body)]);
        add_entry_in(
            &registry,
            &instance,
            "local",
            Target::Platform("local"),
            Ask {
                code: None,
                token_file: None,
            },
            false,
        )
        .await
        .expect("added");
        let mint = minted.join().expect("mint stub");
        assert!(mint.starts_with("POST /v1/archeion/tagmata "), "{mint}");
        let requests = handle.join().expect("stub");
        let request = &requests[1];
        assert!(
            !request.contains("sk-pat"),
            "the register token stays here: {request}"
        );
    }

    #[tokio::test]
    async fn add_entry_without_a_code_or_a_token_names_both_ways_out() {
        let (instance, _handle) = instance_answers("alpha", vec![(200, r#"{"entries":[]}"#)]);
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::load_from(&dir.path().join("polis-registry.toml")).expect("empty");
        let error = add_entry_in(
            &registry,
            &instance,
            "local",
            Target::Url("http://127.0.0.1:8080"),
            Ask {
                code: None,
                token_file: None,
            },
            false,
        )
        .await
        .expect_err("nothing to mint with");
        let text = format!("{error:#}");
        assert!(text.contains("--code"), "{text}");
        assert!(!text.contains("polis token set"), "{text}");
        assert!(text.contains("--token-file"), "{text}");
    }
    #[tokio::test]
    async fn add_entry_without_a_token_names_the_register_verb_that_stores_one() {
        let (instance, _handle) = instance_answers("alpha", vec![(200, r#"{"entries":[]}"#)]);
        let dir = tempfile::tempdir().expect("tempdir");
        // Registered, but holding no secret: the register is where that is fixed.
        let registry = register(&dir, "local", "https://edge.example.com", None);
        let error = add_entry_in(
            &registry,
            &instance,
            "local",
            Target::Platform("local"),
            Ask {
                code: None,
                token_file: None,
            },
            false,
        )
        .await
        .expect_err("no token to mint with");
        let text = format!("{error:#}");
        assert!(text.contains("polis token set local"), "{text}");
        assert!(text.contains("--token-file"), "{text}");
    }

    #[test]
    fn add_entry_needs_to_know_which_platform_it_is_for() {
        let error = Target::pick(None, None).expect_err("no platform");
        let text = format!("{error:#}");
        assert!(text.contains("--polis"), "{text}");
        assert!(text.contains("--url"), "{text}");
    }

    #[tokio::test]
    async fn add_entry_judges_a_given_url_like_a_recorded_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::load_from(&dir.path().join("polis-registry.toml")).expect("empty");
        let instance = instance::stub(
            "alpha",
            "http://127.0.0.1:1".to_owned(),
            "op-secret",
            std::time::Duration::from_secs(5),
        );
        let error = add_entry_in(
            &registry,
            &instance,
            "local",
            Target::Url("http://edge.example.com"),
            Ask {
                code: Some("code-1"),
                token_file: None,
            },
            false,
        )
        .await
        .expect_err("plain http off loopback");
        assert!(
            format!("{error:#}").contains("--allow-plaintext"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn add_entry_refuses_a_name_the_instance_already_holds() {
        let body = r#"{"entries":[{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-1","relay":true,"gateway":false}],"sugar":false}"#;
        let (instance, handle) = instance_at("alpha", 200, body);
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", "http://127.0.0.1:9", Some("sk-pat"));
        let error = add_entry_in(
            &registry,
            &instance,
            "local",
            Target::Platform("local"),
            Ask {
                code: None,
                token_file: None,
            },
            false,
        )
        .await
        .expect_err("the name is taken");
        let text = format!("{error:#}");
        assert!(text.contains("remove-entry alpha local"), "{text}");
        handle.join().expect("stub");
    }

    /// A monitor with no listener: the pre-read helpers never reach it.
    fn an_instance_called(slug: &str) -> Instance {
        instance::stub(
            slug,
            "http://127.0.0.1:1".to_owned(),
            "op-secret",
            std::time::Duration::from_secs(5),
        )
    }

    fn a_listing(entries: Vec<instance::Row>, sugar: bool) -> instance::Listing {
        instance::Listing { entries, sugar }
    }

    fn a_row(name: &str, enrolled: bool) -> instance::Row {
        instance::Row {
            name: name.to_owned(),
            url: "http://127.0.0.1:8080".to_owned(),
            enrolled,
            tagma_id: enrolled.then(|| "t-1".to_owned()),
            relay: true,
            gateway: false,
        }
    }

    /// An entry without credentials is what a code repairs, so the pre-read
    /// lets it through; only an entry that holds them is a dead end.
    #[test]
    fn a_name_without_credentials_is_not_a_hopeless_add() {
        let instance = an_instance_called("alpha");
        let listing = a_listing(vec![a_row("local", false)], false);
        refuse_a_hopeless_add(&instance, "local", &listing).expect("repairable");
    }

    /// An environment-configured instance takes no writes, so the pre-read
    /// answers what the write would have: no code is minted for it.
    #[test]
    fn an_environment_configured_instance_is_a_hopeless_add() {
        let instance = an_instance_called("alpha");
        let listing = a_listing(Vec::new(), true);
        let error =
            refuse_a_hopeless_add(&instance, "local", &listing).expect_err("environment entries");
        let text = format!("{error:#}");
        assert!(
            text.contains("takes its entries from the environment"),
            "{text}"
        );
    }

    /// The token file is not decoration: the value inside it is what the
    /// platform sees, and the code it answers with is what the add carries.
    #[tokio::test]
    async fn add_entry_mints_with_the_token_from_the_file() {
        let (base, handle) = serve_answers(vec![
            (200, r#"{"entries":[],"sugar":false}"#),
            (
                200,
                r#"{"code":"sk-enroll-from-file","id":"t-9","created_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:10:00Z"}"#,
            ),
            (
                201,
                r#"{"entry":{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-9","relay":true,"gateway":false},"note":"enrolled"}"#,
            ),
        ]);
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, "sk-file-token\n").expect("write the token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 600");
        }
        let registry = register(&dir, "edge", &base, None);
        let instance = instance::stub(
            "alpha",
            base.clone(),
            "op-secret",
            std::time::Duration::from_secs(5),
        );
        let lines = add_entry_in(
            &registry,
            &instance,
            "local",
            Target::Platform("edge"),
            Ask {
                code: None,
                token_file: Some(token_path.to_str().expect("a utf-8 path")),
            },
            false,
        )
        .await
        .expect("added");
        let requests = handle.join().expect("stub");
        assert!(
            requests[1]
                .to_lowercase()
                .contains("authorization: bearer sk-file-token"),
            "{}",
            requests[1]
        );
        assert!(
            requests[2].contains("sk-enroll-from-file"),
            "{}",
            requests[2]
        );
        assert!(lines[0].starts_with("local  "), "{}", lines[0]);
    }

    /// A gateway's HTML is not an answer this CLI can read; the operator still
    /// gets the page's own words rather than a parse failure about JSON.
    #[tokio::test]
    async fn an_answer_that_is_not_json_keeps_its_words() {
        let (instance, handle) = instance_at("alpha", 502, "<html>bad gateway</html>");
        let error = instance.entries().await.expect_err("no readable answer");
        let text = format!("{error:#}");
        assert!(text.contains("bad gateway"), "{text}");
        assert!(text.contains("answered 502"), "{text}");
        handle.join().expect("stub");
    }

    #[tokio::test]
    async fn add_entry_normalizes_a_given_url() {
        let body = r#"{"entry":{"name":"local","url":"http://127.0.0.1:8080","enrolled":true,"tagma_id":"t-1","relay":true,"gateway":true},"note":"enrolled"}"#;
        let (instance, handle) = instance_at("alpha", 201, body);
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Registry::load_from(&dir.path().join("polis-registry.toml")).expect("empty");
        add_entry_in(
            &registry,
            &instance,
            "local",
            Target::Url("http://127.0.0.1:8080/"),
            Ask {
                code: Some("code-1"),
                token_file: None,
            },
            false,
        )
        .await
        .expect("added");
        let request = handle.join().expect("stub");
        assert!(
            request.contains(r#""url":"http://127.0.0.1:8080""#),
            "{request}"
        );
    }

    #[tokio::test]
    async fn remove_entry_refuses_a_name_that_is_not_a_slug() {
        let (instance, _handle) = instance_at("alpha", 200, "{}");
        let error = remove_entry_in(&instance, "../agents/x", false)
            .await
            .expect_err("not a slug");
        assert!(format!("{error:#}").contains("does not match"), "{error:#}");
    }

    #[tokio::test]
    async fn mint_code_prints_the_code_and_its_expiry() {
        let (platform, handle) = serve_once(
            200,
            r#"{"code":"sk-enroll-abc","id":"t1","created_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:10:00Z"}"#,
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = register(&dir, "local", &platform, Some("sk-pat"));
        let lines = mint_code_in(&registry, "local", false)
            .await
            .expect("minted");
        assert_eq!(lines[0], "code sk-enroll-abc  expires 2026-01-01T00:10:00Z");
        assert!(lines[1].contains("pending tagma t1"), "{:?}", lines);
        assert!(lines[1].contains("revoke local t1"), "{:?}", lines);
        assert!(lines[2].contains("--code <code>"), "{}", lines[2]);
        handle.join().expect("stub");
    }

    #[tokio::test]
    async fn remove_entry_reports_the_relay_and_the_credentials() {
        let body = r#"{"name":"local","removed":true,"relay":"stopped","credentials":"kept","note":"the platform registration stays"}"#;
        let (instance, handle) = instance_at("alpha", 200, body);
        let lines = remove_entry_in(&instance, "local", false)
            .await
            .expect("removed");
        assert_eq!(lines[0], "local removed");
        assert_eq!(lines[1], "relay=stopped");
        assert_eq!(lines[2], "credentials=kept");
        assert_eq!(lines[3], "the platform registration stays");
        let request = handle.join().expect("stub");
        assert!(
            request.starts_with("DELETE /polis/local?forget=false "),
            "{request}"
        );
    }
}
