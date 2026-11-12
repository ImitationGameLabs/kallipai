//! The three per-instance roots and the one rule that resolves them.
//!
//! An instance's data, config and state roots each resolve the same way, in a
//! fixed order: its own key (the explicit escape hatch), else the XDG variable,
//! else the canonical default under `HOME`. An empty value names nothing at
//! every level; a relative XDG value is ignored (the XDG specification: a
//! relative value names no location), while a relative own-key value is a
//! refusal — it would resolve against whatever working directory the launcher
//! happened to have.
//!
//! There is one implementation because two processes need it and must agree:
//! the tagma resolves its own process environment, and the daemon resolves the
//! exact environment it is about to hand an instance. The daemon then creates,
//! chdirs into, records and scans the data root it resolved here, and the
//! instance resolves the same value from the same keys, because it is the same
//! function over the same key-value set.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::wire::valid_slug;

/// Which instance root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    /// The agent trees and `skills/`.
    Data,
    /// The declared configuration (`profiles.toml`, `exec_hooks.toml`).
    Config,
    /// Output residue that outlives restarts (instance logs).
    State,
}

impl RootKind {
    /// The own key: the explicit escape hatch for this root.
    pub fn own_key(self) -> &'static str {
        match self {
            RootKind::Data => "KALLIPAI_TAGMA_DATA_DIR",
            RootKind::Config => "KALLIPAI_TAGMA_CONFIG_DIR",
            RootKind::State => "KALLIPAI_TAGMA_STATE_DIR",
        }
    }

    /// The standard variable that steers this root.
    pub fn xdg_var(self) -> &'static str {
        match self {
            RootKind::Data => "XDG_DATA_HOME",
            RootKind::Config => "XDG_CONFIG_HOME",
            RootKind::State => "XDG_STATE_HOME",
        }
    }

    /// The root's noun, for the error text and for a reader that has to
    /// name the tree it cannot find.
    pub fn noun(self) -> &'static str {
        match self {
            RootKind::Data => "data",
            RootKind::Config => "config",
            RootKind::State => "state",
        }
    }

    /// Where the canonical default lives under `HOME`.
    fn home_dir(self) -> &'static str {
        match self {
            RootKind::Data => ".local/share",
            RootKind::Config => ".config",
            RootKind::State => ".local/state",
        }
    }

    /// The tree this root appends to a level-2 or level-3 base. The data and
    /// config roots are slug-namespaced; the state root holds every instance's
    /// log leaves, and the slug is appended per leaf by the log placement.
    fn under(self, base: &std::path::Path, slug: &str) -> PathBuf {
        match self {
            RootKind::Data | RootKind::Config => base.join("kallipai").join("tagmata").join(slug),
            RootKind::State => base.join("kallipai"),
        }
    }

    /// Every root, in the order the composition names them.
    pub fn all() -> [RootKind; 3] {
        [RootKind::Data, RootKind::Config, RootKind::State]
    }

    /// The two keys that may name this root: the own key and the XDG base.
    pub fn keys(self) -> [&'static str; 2] {
        [self.own_key(), self.xdg_var()]
    }

    /// Whether `key` is one of the six root keys, either form.
    pub fn names_root(key: &str) -> bool {
        RootKind::all()
            .iter()
            .any(|kind| kind.keys().contains(&key))
    }

    /// The three levels that can name this root, in order: the own key, the
    /// XDG variable and `HOME`. A refusal that has to say what to set takes
    /// the list from here, so the keys it prints are the keys the resolver
    /// reads.
    pub fn remedies(self) -> [&'static str; 3] {
        [self.own_key(), self.xdg_var(), "HOME"]
    }
}

/// The three per-instance roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    /// The data home: agent trees and `skills/`.
    pub data: PathBuf,
    /// The declared-configuration home.
    pub config: PathBuf,
    /// The state home namespace (logs live under it).
    pub state: PathBuf,
}

/// The log tree of one instance under a state root: the
/// `tagmata/<slug>/logs` leaf. One spelling, called by the record's
/// `logs_dir` and by the restart timeout message.
pub fn log_tree(state: &Path, slug: &str) -> PathBuf {
    state.join("tagmata").join(slug).join("logs")
}

/// Why a root could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// No level named a usable directory.
    Missing,
    /// The own key was set to a relative path.
    NotAbsolute {
        /// The own key that carried the value.
        key: &'static str,
        /// The value, verbatim (rendered lossily).
        value: OsString,
    },
    /// The slug cannot name a tree leaf.
    InvalidSlug {
        /// The slug as given.
        slug: String,
    },
}

/// A root that could not be resolved: which root, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootsError {
    kind: RootKind,
    cause: Cause,
}

impl RootsError {
    fn missing(kind: RootKind) -> Self {
        RootsError {
            kind,
            cause: Cause::Missing,
        }
    }

    fn not_absolute(kind: RootKind, key: &'static str, value: OsString) -> Self {
        RootsError {
            kind,
            cause: Cause::NotAbsolute { key, value },
        }
    }

    fn invalid_slug(slug: &str) -> Self {
        RootsError {
            kind: RootKind::Data,
            cause: Cause::InvalidSlug {
                slug: slug.to_owned(),
            },
        }
    }

    /// Which root failed.
    pub fn kind(&self) -> RootKind {
        self.kind
    }

    /// Why it failed.
    pub fn cause(&self) -> &Cause {
        &self.cause
    }

    /// The keys that fix a [`Cause::Missing`] root, in order: the own key, the
    /// XDG variable and `HOME`. `None` for a cause that is not about a missing
    /// location.
    pub fn remedies(&self) -> Option<[&'static str; 3]> {
        matches!(self.cause, Cause::Missing).then(|| self.kind.remedies())
    }
}

impl RootKind {
    /// How the canonical default renders in the error text.
    fn default_hint(self) -> &'static str {
        match self {
            RootKind::Data => "$HOME/.local/share/kallipai/tagmata/<slug>",
            RootKind::Config => "$HOME/.config/kallipai/tagmata/<slug>",
            RootKind::State => "$HOME/.local/state/kallipai",
        }
    }
}

impl std::fmt::Display for RootsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = self.kind;
        match &self.cause {
            Cause::Missing => write!(
                f,
                "cannot resolve the instance {} root: set {} or {} to an absolute \
                 directory, or HOME to a home directory (the default is {})",
                kind.noun(),
                kind.own_key(),
                kind.xdg_var(),
                kind.default_hint(),
            ),
            Cause::NotAbsolute { key, value } => write!(
                f,
                "{} {:?} is not absolute: give an absolute path or remove the key",
                key,
                value.to_string_lossy(),
            ),
            Cause::InvalidSlug { slug } => write!(
                f,
                "instance slug {slug:?} does not match [a-z0-9][a-z0-9-]* \
                 (at most 64 characters)",
            ),
        }
    }
}

impl std::error::Error for RootsError {}

/// Resolve the three roots for `slug` from a key lookup.
pub fn resolve<F>(slug: &str, get: F) -> Result<Roots, RootsError>
where
    F: Fn(&str) -> Option<OsString>,
{
    if !valid_slug(slug) {
        return Err(RootsError::invalid_slug(slug));
    }
    Ok(Roots {
        data: resolve_one(RootKind::Data, slug, &get)?,
        config: resolve_one(RootKind::Config, slug, &get)?,
        state: resolve_one(RootKind::State, slug, &get)?,
    })
}

/// Resolve the three roots from a composed environment (`KEY=VALUE` pairs).
///
/// This is the daemon's entry point: the pairs are the exact argv env the spawn
/// helper exports, so the tagma resolves the same values from the same set.
pub fn resolve_pairs(slug: &str, pairs: &[String]) -> Result<Roots, RootsError> {
    let mut values: Vec<(&str, OsString)> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        if let Some((key, value)) = pair.split_once('=') {
            values.push((key, OsString::from(value)));
        }
    }
    resolve(slug, |key| {
        values
            .iter()
            .rev()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| value.clone())
    })
}

/// Resolve the three roots from this process's own environment.
pub fn resolve_process_env(slug: &str) -> Result<Roots, RootsError> {
    resolve(slug, |key| std::env::var_os(key))
}

/// One root, first match wins: own key, XDG variable, `HOME` default.
fn resolve_one<F>(kind: RootKind, slug: &str, get: &F) -> Result<PathBuf, RootsError>
where
    F: Fn(&str) -> Option<OsString>,
{
    if let Some(value) = get(kind.own_key()).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(&value);
        if path.is_absolute() {
            return Ok(path);
        }
        return Err(RootsError::not_absolute(kind, kind.own_key(), value));
    }
    if let Some(base) = get(kind.xdg_var())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(kind.under(&base, slug));
    }
    if let Some(home) = get("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(kind.under(&home.join(kind.home_dir()), slug));
    }
    Err(RootsError::missing(kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(*value)))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn resolved(pairs: &[(&str, &str)]) -> Roots {
        resolve("main", lookup(pairs)).expect("roots resolve")
    }

    #[test]
    fn home_alone_yields_the_canonical_defaults() {
        let roots = resolved(&[("HOME", "/home/u")]);
        assert_eq!(
            roots.data,
            PathBuf::from("/home/u/.local/share/kallipai/tagmata/main")
        );
        assert_eq!(
            roots.config,
            PathBuf::from("/home/u/.config/kallipai/tagmata/main")
        );
        assert_eq!(roots.state, PathBuf::from("/home/u/.local/state/kallipai"));
    }

    #[test]
    fn xdg_alone_yields_the_xdg_roots() {
        let roots = resolved(&[
            ("XDG_DATA_HOME", "/data"),
            ("XDG_CONFIG_HOME", "/conf"),
            ("XDG_STATE_HOME", "/st"),
        ]);
        assert_eq!(roots.data, PathBuf::from("/data/kallipai/tagmata/main"));
        assert_eq!(roots.config, PathBuf::from("/conf/kallipai/tagmata/main"));
        assert_eq!(roots.state, PathBuf::from("/st/kallipai"));
    }

    #[test]
    fn an_own_key_wins_and_is_taken_verbatim() {
        let roots = resolved(&[
            ("HOME", "/home/u"),
            ("XDG_DATA_HOME", "/data"),
            ("KALLIPAI_TAGMA_DATA_DIR", "/srv/instances/main"),
        ]);
        assert_eq!(roots.data, PathBuf::from("/srv/instances/main"));
        // The untouched roots keep resolving from the level below.
        assert_eq!(
            roots.config,
            PathBuf::from("/home/u/.config/kallipai/tagmata/main")
        );
    }

    #[test]
    fn each_root_has_its_own_own_key() {
        let roots = resolved(&[
            ("HOME", "/home/u"),
            ("KALLIPAI_TAGMA_CONFIG_DIR", "/srv/conf"),
            ("KALLIPAI_TAGMA_STATE_DIR", "/srv/state"),
        ]);
        assert_eq!(
            roots.data,
            PathBuf::from("/home/u/.local/share/kallipai/tagmata/main")
        );
        assert_eq!(roots.config, PathBuf::from("/srv/conf"));
        assert_eq!(roots.state, PathBuf::from("/srv/state"));
    }

    #[test]
    fn an_empty_own_key_falls_through_to_xdg() {
        let roots = resolved(&[
            ("KALLIPAI_TAGMA_DATA_DIR", ""),
            ("XDG_DATA_HOME", "/data"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(roots.data, PathBuf::from("/data/kallipai/tagmata/main"));
    }

    #[test]
    fn an_empty_xdg_value_falls_through_to_home() {
        let roots = resolved(&[("XDG_DATA_HOME", ""), ("HOME", "/home/u")]);
        assert_eq!(
            roots.data,
            PathBuf::from("/home/u/.local/share/kallipai/tagmata/main")
        );
    }

    #[test]
    fn a_relative_xdg_value_names_nothing() {
        let roots = resolved(&[("XDG_DATA_HOME", "data"), ("HOME", "/home/u")]);
        assert_eq!(
            roots.data,
            PathBuf::from("/home/u/.local/share/kallipai/tagmata/main")
        );
    }

    #[test]
    fn a_relative_own_key_is_refused() {
        let err = resolve(
            "main",
            lookup(&[("KALLIPAI_TAGMA_DATA_DIR", "instances/main")]),
        )
        .expect_err("relative own key");
        assert_eq!(err.kind(), RootKind::Data);
        assert_eq!(
            err.cause(),
            &Cause::NotAbsolute {
                key: "KALLIPAI_TAGMA_DATA_DIR",
                value: OsString::from("instances/main"),
            }
        );
        assert_eq!(err.remedies(), None);
    }

    #[test]
    fn nothing_set_names_the_three_remedies() {
        let err = resolve("main", lookup(&[])).expect_err("no environment");
        assert_eq!(err.kind(), RootKind::Data);
        assert_eq!(*err.cause(), Cause::Missing);
        assert_eq!(
            err.remedies(),
            Some(["KALLIPAI_TAGMA_DATA_DIR", "XDG_DATA_HOME", "HOME"])
        );
        let text = err.to_string();
        for key in err.remedies().expect("remedies") {
            assert!(text.contains(key), "{key} missing from {text:?}");
        }
    }

    #[test]
    fn every_root_reports_its_own_missing_remedies() {
        let empty = lookup(&[]);
        for kind in [RootKind::Data, RootKind::Config, RootKind::State] {
            let err = resolve_one(kind, "main", &empty).expect_err("missing");
            assert_eq!(err.kind(), kind);
            assert_eq!(
                err.remedies(),
                Some([kind.own_key(), kind.xdg_var(), "HOME"])
            );
        }
    }

    #[test]
    fn a_malformed_slug_is_refused() {
        let err = resolve("../evil", lookup(&[("HOME", "/home/u")])).expect_err("bad slug");
        assert_eq!(
            err.cause(),
            &Cause::InvalidSlug {
                slug: "../evil".to_owned()
            }
        );
    }

    #[test]
    fn a_composed_pair_list_and_a_lookup_agree() {
        let pairs: Vec<String> = [
            "HOME=/home/u",
            "XDG_CONFIG_HOME=/conf",
            "KALLIPAI_TAGMA_STATE_DIR=/srv/state",
        ]
        .iter()
        .map(|pair| (*pair).to_owned())
        .collect();
        let from_pairs = resolve_pairs("main", &pairs).expect("pairs resolve");
        let from_lookup = resolved(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/conf"),
            ("KALLIPAI_TAGMA_STATE_DIR", "/srv/state"),
        ]);
        assert_eq!(from_pairs, from_lookup);
    }
}
