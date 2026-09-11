//! Test-support helpers shared across the workspace's crates.

use std::path::Path;

/// A guard directory under the managed `/tmp/kallipai-dev` root: the
/// `TempDir` deletes the tree on drop, so nothing outlives the run
/// (Deref/AsRef keep call sites reading as plain paths).
///
/// The root guard must live as long as the tree: dropping it
/// (as a local in new()) would delete the created paths.
pub struct DevDir(tempfile::TempDir);

impl DevDir {
    /// The guard's root path.
    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Create a guard directory under the managed root, its name
    /// prefixed with `label`.
    pub fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join("kallipai-dev");
        std::fs::create_dir_all(&root).expect("create /tmp/kallipai-dev");
        Self(
            tempfile::Builder::new()
                .prefix(&format!("{label}-"))
                .tempdir_in(root)
                .expect("create test tempdir"),
        )
    }
}

impl std::ops::Deref for DevDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

impl AsRef<Path> for DevDir {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_creates_under_the_managed_root_and_cleans_up() {
        let dir = DevDir::new("testkit-selftest");
        let name = dir.0.path().file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("testkit-selftest-"));
        assert!(dir.path().is_dir());
        let path = dir.path().to_path_buf();
        assert!(path.exists());
        drop(dir);
        assert!(!path.exists(), "the guard must delete the tree on drop");
    }
}
