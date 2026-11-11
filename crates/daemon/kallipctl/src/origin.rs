//! The one place a polis address is judged.
//!
//! Two callers need the same answer: `spawn` carries a `--polis` address into
//! an instance's environment, and `polis add` records one for later platform
//! calls. Both require an http or https origin (no path, query, fragment or
//! userinfo) and both care whether using it puts a token on the wire in the
//! clear. Keeping the rule here means the two cannot drift apart.

use anyhow::{Context as _, Result};

/// An http or https origin: scheme, host, optional port, nothing else.
///
/// [`Origin::as_str`] is the stored and printed form, with no trailing slash,
/// so `HTTP://API.example:8080/` and `http://api.example:8080` are one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    display: String,
    plaintext: bool,
}

impl Origin {
    /// Parse `raw`, naming `label` (a flag or a setting) in every refusal so
    /// the sentence points at what the operator typed.
    pub fn parse(label: &str, raw: &str) -> Result<Self> {
        let parsed = url::Url::parse(raw.trim())
            .with_context(|| format!("{label} {raw:?} is not an absolute URL"))?;
        // The calls that follow speak HTTP; any other scheme parses as a URL
        // and then fails as a transport error, pointing the diagnosis at the
        // network instead of at the address.
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("{label} must be an http or https URL");
        }
        // Every base join assumes an origin: a path would be carried into the
        // instance's environment and joined again by its own consumers, and a
        // query, fragment or userinfo would travel with it.
        if !matches!(parsed.path(), "" | "/") {
            anyhow::bail!("{label} must be an origin, without a path");
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            anyhow::bail!("{label} must be an origin, without a query or fragment");
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            anyhow::bail!("{label} must not carry a user name or password");
        }
        let plaintext = parsed.scheme() == "http" && !is_loopback(&parsed);
        Ok(Self {
            display: parsed.origin().ascii_serialization(),
            plaintext,
        })
    }

    /// The origin as it is stored, printed and passed on.
    pub fn as_str(&self) -> &str {
        &self.display
    }

    /// Whether every byte to this origin crosses the network in the clear.
    pub fn is_plaintext(&self) -> bool {
        self.plaintext
    }

    /// Refuse a plaintext origin unless the operator said otherwise: the token
    /// this address will carry is long-lived and travels in the clear.
    pub fn check_plaintext(&self, label: &str, allow: bool) -> Result<()> {
        if !self.plaintext || allow {
            return Ok(());
        }
        anyhow::bail!(
            "{label} {} is not loopback and not https; the access token would cross the network in the clear; pass --allow-plaintext on a network you trust",
            self.display
        )
    }
}

/// Whether the origin names this host, and so whether the traffic stays on it.
/// The addresses answer that themselves. For names the rule is the RFC 6761
/// convention rather than the kernel's: `localhost` and every name under it are
/// left to resolve to loopback, which is what resolvers do with them and not
/// something this check can observe.
fn is_loopback(origin: &url::Url) -> bool {
    match origin.host() {
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
        Some(url::Host::Domain(name)) => {
            let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
        None => false,
    }
}
