//! The host-level polis registry: which platforms this host knows, and the
//! personal access token that authenticates each one.
//!
//! One file, `$XDG_CONFIG_HOME/kallipai/polis-registry.toml`, mode 0600.
//! `kallipctl polis` owns it outright: nothing else reads or writes it, and
//! the file is the only place `kallipctl polis` keeps one.
//!
//! Pure file handling: no daemon, no network. Everything here is exercised
//! against a tempdir.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use kallipai_common::authtoken::{TokenKind, mask_token};
use kallipai_daemon_common::wire::valid_slug;
use serde::{Deserialize, Serialize};

use crate::origin::Origin;

/// Minted personal access tokens carry this prefix, so a masked token keeps
/// the shape the mint printed (`sk-pat-abc***xyz`).
const PAT_KIND: TokenKind = TokenKind("sk-pat-");

/// An entry name is the same grammar a slug is: it becomes a `[poli.<name>]`
/// table key in this file, and the name every `polis` verb takes.
pub fn valid_name(name: &str) -> bool {
    valid_slug(name)
}

/// The stored form of a polis address: the shared origin rule (see
/// [`crate::origin`]), with the registry's wording so a bad line in the file
/// names the entry it came from. Normalizing means
/// `HTTP://API.example:8080/` and `http://api.example:8080` are one record.
pub fn normalize_origin(raw: &str) -> Result<String> {
    Origin::parse("a polis address", raw)
        .map(|origin| origin.as_str().to_owned())
        .map_err(|error| anyhow::anyhow!("{raw:?} does not parse: {error}"))
}

/// The registry file the CLI reads and writes.
pub fn registry_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("no config directory; set XDG_CONFIG_HOME or HOME"))?;
    Ok(dir.join("kallipai").join("polis-registry.toml"))
}

/// One platform: where it is, and the token that authenticates against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Normalized origin (see [`normalize_origin`]).
    pub url: String,
    /// The owner's personal access token. Absent means the record exists but
    /// cannot talk to the platform yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// The file body: `[poli.<name>]` tables, keyed by name.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    poli: BTreeMap<String, Entry>,
}

/// What [`Registry::add`] did, so the verb prints the sentence that matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addition {
    Added,
    Replaced,
    Unchanged,
}

/// The loaded registry. Mutating methods only touch memory; [`Registry::save`]
/// is the one write, so a failed verb leaves the file as it was.
#[derive(Debug)]
pub struct Registry {
    path: PathBuf,
    document: Document,
}

impl Registry {
    /// Load the host's registry. A missing file is an empty registry; a
    /// malformed or loosely-permissioned one is an error, never silence.
    pub fn load() -> Result<Self> {
        Self::load_from(&registry_path()?)
    }

    /// Load from an explicit path (the tests' entry point).
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: path.to_owned(),
                    document: Document::default(),
                });
            }
            Err(error) => {
                return Err(anyhow::anyhow!("reading {}: {error}", path.display()));
            }
        };
        refuse_loose_mode(path)?;
        let document: Document = toml::from_str(&raw)
            .map_err(|error| anyhow::anyhow!("{} does not parse: {error}", path.display()))?;
        // Two names for one platform would make the owner of an origin depend
        // on map order, so a file that holds one is unusable as read.
        let mut owners: BTreeMap<String, &str> = BTreeMap::new();
        for (name, entry) in &document.poli {
            if !valid_name(name) {
                anyhow::bail!(
                    "{} holds the name {name:?}, which does not match [a-z0-9][a-z0-9-]* (max 64 chars)",
                    path.display()
                );
            }
            let stored = normalize_origin(&entry.url).map_err(|error| {
                anyhow::anyhow!(
                    "{}: the entry {name} has an unusable url: {error}",
                    path.display()
                )
            })?;
            if stored != entry.url {
                anyhow::bail!(
                    "{}: the entry {name} stores {} unnormalized; write it as {stored}",
                    path.display(),
                    entry.url
                );
            }
            if let Some(other) = owners.insert(stored.clone(), name.as_str()) {
                anyhow::bail!(
                    "{} records {stored} twice, as {other} and {name}; one platform, one name",
                    path.display()
                );
            }
        }
        Ok(Self {
            path: path.to_owned(),
            document,
        })
    }

    /// The file this registry reads and writes. The tests use it to prove
    /// what a verb did to the bytes.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_empty(&self) -> bool {
        self.document.poli.is_empty()
    }

    /// Names in the file, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.document.poli.keys().map(String::as_str)
    }

    /// `(name, record)` pairs in file order, for the callers that need both
    /// halves: completion help lines, the listing, and anything that can
    /// answer "which name is that" from the same walk.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.document
            .poli
            .iter()
            .map(|(name, entry)| (name.as_str(), entry))
    }

    /// `<name>, <name>` or `none`, for the messages that have to say what
    /// the registry does hold.
    pub fn known_names(&self) -> String {
        if self.is_empty() {
            return "none".to_owned();
        }
        self.names().collect::<Vec<_>>().join(", ")
    }

    pub fn record(&self, name: &str) -> Option<&Entry> {
        self.document.poli.get(name)
    }

    /// The record, or the sentence that says how to make one. The read-side
    /// twin of [`Registry::set_secret`], so a verb that needs the address
    /// before it can store a token answers the same way.
    pub fn entry(&self, name: &str) -> Result<&Entry> {
        self.document.poli.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no polis named {name}; add it with `kallipctl polis add {name} <url>`, or pick one of: {}",
                self.known_names()
            )
        })
    }

    /// The name holding `origin`, if any (an origin is stored already
    /// normalized, as [`normalize_origin`] produces it).
    pub fn origin_owner(&self, origin: &str) -> Option<&str> {
        self.document
            .poli
            .iter()
            .find(|(_, entry)| entry.url == origin)
            .map(|(name, _)| name.as_str())
    }

    /// The record's token in masked form; `None` when it has none.
    pub fn masked(&self, name: &str) -> Option<String> {
        self.record(name)?
            .secret
            .as_deref()
            .map(|secret| mask_token(secret, PAT_KIND))
    }

    /// Record `name` at `url`. Re-adding the same pair changes nothing; a
    /// different address for a known name needs `replace`, because attached
    /// instances keep their own copy until they are re-added.
    pub fn add(&mut self, name: &str, url: &str, replace: bool) -> Result<Addition> {
        if !valid_name(name) {
            anyhow::bail!("name {name:?} does not match [a-z0-9][a-z0-9-]* (max 64 chars)");
        }
        let origin = normalize_origin(url)?;
        if let Some(owner) = self.origin_owner(&origin)
            && owner != name
        {
            anyhow::bail!("{origin} is already recorded as {owner}; one platform, one name");
        }
        match self.document.poli.get(name).map(|entry| entry.url.clone()) {
            None => {
                self.document.poli.insert(
                    name.to_owned(),
                    Entry {
                        url: origin,
                        secret: None,
                    },
                );
                Ok(Addition::Added)
            }
            Some(current) if current == origin => Ok(Addition::Unchanged),
            Some(current) if !replace => anyhow::bail!(
                "{name} already points at {current}; pass --replace to point it at {origin} (attached instances keep their copy until they are re-added)"
            ),
            Some(_) => {
                if let Some(entry) = self.document.poli.get_mut(name) {
                    entry.url = origin;
                }
                Ok(Addition::Replaced)
            }
        }
    }

    /// Drop one record and nothing else.
    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.document.poli.remove(name).is_none() {
            anyhow::bail!("no polis named {name}; known: {}", self.known_names());
        }
        Ok(())
    }

    /// Store the platform's access token on a record that already exists.
    pub fn set_secret(&mut self, name: &str, secret: &str) -> Result<()> {
        if secret.trim().is_empty() {
            anyhow::bail!("an empty token is not a token");
        }
        let entry = self.existing_mut(name)?;
        entry.secret = Some(secret.to_owned());
        Ok(())
    }

    fn existing_mut(&mut self, name: &str) -> Result<&mut Entry> {
        let known = self.known_names();
        self.document.poli.get_mut(name).ok_or_else(|| {
            anyhow::anyhow!(
                "no polis named {name}; add it with `kallipctl polis add {name} <url>`, or pick one of: {known}"
            )
        })
    }

    /// Write the file: temp 0600 + rename inside its own directory, so the
    /// tokens are never world-readable and a half-written file never lands.
    ///
    /// The directory is tightened to 0700 before the temporary file exists, so
    /// the token never sits in a directory group or others can enter, not even
    /// for the moment between the write and the rename.
    pub fn save(&self) -> Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", self.path.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(
                || format!("chmod 700 on the registry's directory {}", dir.display()),
            )?;
        }
        let body = toml::to_string(&self.document).context("serializing the polis registry")?;
        // pid and a counter keep two concurrent kallipctl runs from staging
        // over each other; the rename still makes the swap atomic.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let staged = format!(".polis-registry.toml.{}.{seq}.tmp", std::process::id());
        let tmp = dir.join(staged);
        std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|error| {
            let _ = std::fs::remove_file(&tmp);
            anyhow::Error::from(error).context(format!(
                "renaming the registry into {}",
                self.path.display()
            ))
        })
    }
}

/// A registry readable by group or others is refused rather than used: it
/// holds platform tokens, and the operator has to decide who can see them.
fn refuse_loose_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} is readable by group or others; run chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(dir: &tempfile::TempDir) -> Registry {
        Registry::load_from(&dir.path().join("kallipai").join("polis-registry.toml"))
            .expect("empty registry")
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    /// A file the operator wrote by hand: 0600, which is what the reader
    /// requires of any registry it loads.
    fn write_file(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = registry(&dir);
        assert!(registry.is_empty());
        assert_eq!(registry.known_names(), "none");
        assert!(registry.record("local").is_none());
    }

    #[test]
    fn an_added_record_is_written_owner_only_and_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        assert_eq!(
            registry
                .add("local", "http://api.kallipai.lan:8080", false)
                .expect("add"),
            Addition::Added
        );
        registry.save().expect("save");
        #[cfg(unix)]
        assert_eq!(mode_of(registry.path()), 0o600);
        let reloaded = Registry::load_from(registry.path()).expect("reload");
        assert_eq!(reloaded.known_names(), "local");
        assert_eq!(
            reloaded.record("local").map(|entry| entry.url.as_str()),
            Some("http://api.kallipai.lan:8080")
        );
        assert_eq!(reloaded.masked("local"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_file_is_refused_on_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("polis-registry.toml");
        std::fs::write(&path, "[poli.local]\nurl = \"https://api.example.com\"\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let error = Registry::load_from(&path).expect_err("loose mode");
        let message = error.to_string();
        assert!(message.contains("chmod 600"), "{message}");
    }

    #[test]
    fn an_origin_is_stored_normalized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        registry
            .add("local", "HTTP://API.Example:8080/", false)
            .expect("add");
        assert_eq!(
            registry.record("local").map(|entry| entry.url.as_str()),
            Some("http://api.example:8080")
        );
        assert_eq!(
            registry.origin_owner("http://api.example:8080"),
            Some("local")
        );
    }

    #[test]
    fn a_second_name_for_one_origin_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        registry
            .add("local", "https://api.example.com", false)
            .expect("add");
        let error = registry
            .add("other", "https://api.example.com/", false)
            .expect_err("duplicate origin");
        assert!(error.to_string().contains("already recorded as local"));
    }

    #[test]
    fn a_new_address_for_a_known_name_needs_replace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        registry
            .add("local", "https://one.example.com", false)
            .expect("add");
        registry
            .set_secret("local", "sk-pat-abcdefgh")
            .expect("token");
        let error = registry
            .add("local", "https://two.example.com", false)
            .expect_err("needs replace");
        let message = error.to_string();
        assert!(message.contains("--replace"), "{message}");
        assert!(message.contains("https://one.example.com"), "{message}");
        assert_eq!(
            registry
                .add("local", "https://two.example.com", true)
                .expect("replace"),
            Addition::Replaced
        );
        assert_eq!(
            registry.record("local").map(|entry| entry.url.as_str()),
            Some("https://two.example.com")
        );
        assert!(registry.masked("local").is_some());
    }

    #[test]
    fn readding_the_same_pair_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        registry
            .add("local", "https://api.example.com", false)
            .expect("add");
        registry
            .set_secret("local", "sk-pat-abcdefgh")
            .expect("token");
        assert_eq!(
            registry
                .add("local", "https://api.example.com/", false)
                .expect("again"),
            Addition::Unchanged
        );
        assert!(registry.masked("local").is_some());
    }

    #[test]
    fn remove_drops_that_record_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        registry
            .add("local", "https://local.example.com", false)
            .expect("add");
        registry
            .add("cloud", "https://cloud.example.com", false)
            .expect("add");
        registry.remove("local").expect("remove");
        assert_eq!(registry.known_names(), "cloud");
        let error = registry.remove("local").expect_err("gone");
        assert!(error.to_string().contains("known: cloud"));
    }

    #[test]
    fn the_secret_masks_and_a_blank_one_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        registry
            .add("local", "https://api.example.com", false)
            .expect("add");
        registry
            .set_secret("local", "sk-pat-abcdefghijkl")
            .expect("token");
        assert_eq!(
            registry.masked("local").as_deref(),
            Some("sk-pat-abc***jkl")
        );
        let error = registry.set_secret("local", "  ").expect_err("blank");
        assert!(error.to_string().contains("empty token"));
    }

    #[test]
    fn entries_walk_the_file_in_name_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("polis-registry.toml");
        write_file(
            &path,
            "[poli.local]\nurl = \"https://local.example.com\"\n\n[poli.cloud]\nurl = \"https://cloud.example.com\"\n",
        );
        let registry = Registry::load_from(&path).expect("load");
        let pairs = registry
            .entries()
            .map(|(name, entry)| (name, entry.url.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            vec![
                ("cloud", "https://cloud.example.com"),
                ("local", "https://local.example.com"),
            ]
        );
    }

    #[test]
    fn a_token_for_an_unknown_name_names_the_verb_that_creates_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        let error = registry
            .set_secret("ghost", "sk-pat-abcdefgh")
            .expect_err("missing");
        let message = error.to_string();
        assert!(message.contains("kallipctl polis add ghost"), "{message}");
    }

    #[test]
    fn a_name_outside_the_grammar_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        let long = "a".repeat(65);
        for name in ["Local", "-local", "local_name", long.as_str()] {
            let error = registry
                .add(name, "https://api.example.com", false)
                .expect_err("bad name");
            assert!(error.to_string().contains("does not match"), "{name}");
        }
    }

    #[test]
    fn a_bad_origin_keeps_the_shared_sentence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = registry(&dir);
        registry
            .add("local", "api.example.com:8080", false)
            .expect_err("relative");
        let error = registry
            .add("local", "https://api.example.com/v1/archeion", false)
            .expect_err("path");
        assert!(error.to_string().contains("path"), "{error}");
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_dropped_on_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("polis-registry.toml");
        write_file(
            &path,
            "[poli.local]\nurl = \"https://api.example.com\"\nsecret = \"sk-pat-abcdefgh\"\nextra = 1\n",
        );
        let error = Registry::load_from(&path).expect_err("unknown key");
        assert!(error.to_string().contains("does not parse"), "{error}");
    }

    #[test]
    fn a_hand_written_file_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("polis-registry.toml");
        write_file(
            &path,
            "[poli.local]\nurl = \"http://api.kallipai.lan:8080\"\n\n[poli.cloud]\nurl = \"https://api.kallipai.com\"\n",
        );
        let registry = Registry::load_from(&path).expect("load");
        assert_eq!(registry.known_names(), "cloud, local");
        registry.save().expect("save");
        let again = Registry::load_from(&path).expect("reload");
        assert_eq!(again.known_names(), "cloud, local");
        assert_eq!(
            again.record("cloud").map(|entry| entry.secret.is_none()),
            Some(true)
        );
    }

    #[test]
    fn an_unnormalized_url_in_the_file_is_named_not_repaired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("polis-registry.toml");
        write_file(&path, "[poli.local]\nurl = \"https://api.example.com/\"\n");
        let error = Registry::load_from(&path).expect_err("trailing slash");
        assert!(error.to_string().contains("unnormalized"), "{error}");
    }

    #[test]
    fn two_names_for_one_origin_in_the_file_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("polis-registry.toml");
        write_file(
            &path,
            "[poli.a]\nurl = \"https://one.example.com\"\n\n[poli.b]\nurl = \"https://one.example.com\"\n",
        );
        let error = Registry::load_from(&path).expect_err("duplicate origin");
        assert!(error.to_string().contains("twice"), "{error}");
    }
}
