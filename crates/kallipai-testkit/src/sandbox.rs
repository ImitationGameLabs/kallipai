//! Sandbox containment for fixtures that boot a daemon or a tagma.
//!
//! A product process composes its roots from its own key, the XDG variable and
//! `HOME`, in that order. A fixture therefore hands a process an environment it
//! built itself and proves it sandboxed *before* the process starts: the first
//! thing an instance writes into a wrong tree is its own `runtime.json`, so an
//! after-the-fact check is a report of damage, not a guard.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use kallipai_daemon_common::roots::RootKind;

/// The three own keys, read off the product's own root table rather than copied:
/// a rename there has to reach this guard. Their presence is the failure, not
/// their value — the daemon passes every own key of its own environment to each
/// launch as the deployment layer, so one inherited from the caller roots
/// instances at a tree the fixture never named.
pub fn own_keys() -> [&'static str; 3] {
    RootKind::all().map(RootKind::own_key)
}

/// The standard keys, with the home they fall back to. An unset `XDG_*` is not a
/// hole as long as `HOME` resolves inside the sandbox, because that is what the
/// product falls back to.
pub fn standard_keys() -> [&'static str; 3] {
    RootKind::all().map(RootKind::xdg_var)
}

/// Panic unless `pairs` roots every instance the process may launch inside
/// `allowed`.
///
/// `pairs` is the environment a fixture is about to hand the process, not the
/// process's answer: this runs before the spawn, which is the last point where
/// a foreign root is still harmless. `allowed` carries more than one root
/// because a fixture with separate data, config and workspace trees has no
/// single sandbox directory to name.
pub fn assert_env_roots(pairs: &[(String, String)], allowed: &[&Path]) {
    let present: HashMap<&str, &str> = pairs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    for key in own_keys() {
        assert!(
            !present.contains_key(key),
            "{key} reached a process: an own root key from the caller's \
             environment rides the deployment layer into every launch"
        );
    }
    let home = present.get("HOME").unwrap_or_else(|| {
        panic!("HOME is unset for the process: an unset home resolves the real one")
    });
    inside("HOME", home, allowed);
    for key in standard_keys() {
        if let Some(value) = present.get(key) {
            inside(key, value, allowed);
        }
    }
}

/// The same judgement over a running process's environment, read back from
/// `/proc`: the check a fixture cannot fake by forgetting to pin a key.
pub fn assert_process_roots(pid: u32, allowed: &[&Path]) {
    let environ = std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_else(|error| panic!("read the environment of pid {pid}: {error}"));
    let mut pairs = Vec::new();
    for entry in environ.split(|byte| *byte == 0) {
        let Ok(entry) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some((key, value)) = entry.split_once('=') {
            pairs.push((key.to_owned(), value.to_owned()));
        }
    }
    assert_env_roots(&pairs, allowed);
}

/// Panic unless a directory a fixture hands to the stop path resolves inside
/// one of `allowed`. The records area decides which instances get signalled, so
/// a fixture that names the wrong one has to fail here rather than stop a
/// process it does not own.
pub fn assert_contained(label: &str, path: &Path, allowed: &[&Path]) {
    assert!(
        allowed
            .iter()
            .any(|root| resolve(path).starts_with(resolve(root))),
        "{label}={} escapes the fixture roots {allowed:?}",
        path.display()
    );
}

/// Containment on whole path components, never on a string prefix: a sibling
/// `sandbox-2` is not inside `sandbox`. Both sides are canonicalized, so a link
/// inside the sandbox cannot pass for the root it points at.
fn inside(key: &str, value: &str, allowed: &[&Path]) {
    let path = Path::new(value);
    assert!(
        value.len() > 1 && path.is_absolute(),
        "{key}={value} is not an absolute path: a relative or empty root \
         resolves against the process's own working directory"
    );
    assert_contained(key, path, allowed);
}

/// Canonicalize a path, falling back to its literal spelling when it does not
/// exist yet. The fallback keeps the check fail-closed: an unresolved path is
/// compared as written, which can only make it look outside, never inside.
fn resolve(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
