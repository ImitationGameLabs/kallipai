//! Scaffolding shared by the daemon's end-to-end tests.

use std::collections::HashMap;
use std::path::Path;

/// Refuse a boot whose instance roots can resolve outside `sandbox`.
///
/// The check reads the child's own environment back instead of trusting each
/// test to pin every home, so a new case that forgets `HOME` or an `XDG_*`
/// home fails here instead of writing into a real home. The three own root
/// keys must be absent: one inherited from the caller would ride the
/// deployment layer into every launch the daemon makes.
pub fn assert_sandboxed(pid: u32, sandbox: &Path) {
    let environ = std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_else(|error| panic!("read the daemon environment back: {error}"));
    let mut pairs: HashMap<String, String> = HashMap::new();
    for entry in environ.split(|byte| *byte == 0) {
        let Ok(entry) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some((key, value)) = entry.split_once('=') {
            pairs.insert(key.to_owned(), value.to_owned());
        }
    }
    for key in [
        "KALLIPAI_TAGMA_DATA_DIR",
        "KALLIPAI_TAGMA_CONFIG_DIR",
        "KALLIPAI_TAGMA_STATE_DIR",
    ] {
        assert!(
            !pairs.contains_key(key),
            "{key} reached the daemon: a root key from the caller's environment \
             must not ride the deployment layer"
        );
    }
    for key in [
        "HOME",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        let value = pairs.get(key).unwrap_or_else(|| {
            panic!("{key} is unset for the daemon: an unset home resolves the real one")
        });
        assert!(
            Path::new(value).starts_with(sandbox),
            "{key}={value} escapes the sandbox {}",
            sandbox.display()
        );
    }
}
