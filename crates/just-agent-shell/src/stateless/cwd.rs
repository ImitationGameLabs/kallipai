//! Sticky working-directory resolution after a command (Fork B: pwd roundtrip).
//!
//! The per-call wrapper writes `pwd -P` to a tmpfile via an `EXIT` trap (so it
//! runs on normal exit, `exit`, and SIGTERM). `resolve` reads it back,
//! NFC-normalizes (macOS APFS stores decomposed NFD), and `canonicalize`s — the
//! guard that makes cwd honest: if the command `rmdir`'d its own cwd, the
//! canonicalize fails and we fall back rather than report a stale value.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::error::ShellError;

/// Read the post-command cwd from `pwd_file` and resolve it honestly.
///
/// Returns `fallback` when the value is empty or the directory no longer exists
/// — so the result is never stale, only an explicit "unknown → fallback".
pub(super) fn resolve(pwd_file: &Path, fallback: &Path) -> Result<PathBuf, ShellError> {
    let raw = std::fs::read(pwd_file).unwrap_or_default();
    let trimmed = String::from_utf8_lossy(&raw).trim().to_owned();
    if trimmed.is_empty() {
        return Ok(fallback.to_path_buf());
    }

    // NFC-normalize so the same path compares equal across runs.
    let nfc: String = trimmed.nfc().collect();
    let candidate = PathBuf::from(nfc);

    // canonicalize confirms the dir still exists and resolves symlinks.
    Ok(std::fs::canonicalize(&candidate).unwrap_or_else(|_| fallback.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_reads_pwd_file() {
        let dir = tempfile_dir();
        let pwd = dir.join("pwd");
        std::fs::write(&pwd, "/tmp\n").unwrap();
        let resolved = resolve(&pwd, Path::new("/fallback")).unwrap();
        // /tmp canonicalizes to itself on most systems.
        assert!(resolved.ends_with("tmp") || resolved == Path::new("/tmp"));
    }

    #[test]
    fn resolve_empty_falls_back() {
        let dir = tempfile_dir();
        let pwd = dir.join("pwd");
        std::fs::write(&pwd, "   \n").unwrap();
        let resolved = resolve(&pwd, Path::new("/fallback")).unwrap();
        assert_eq!(resolved, Path::new("/fallback"));
    }

    #[test]
    fn resolve_missing_file_falls_back() {
        let resolved = resolve(Path::new("/nonexistent/ja-pwd"), Path::new("/fallback")).unwrap();
        assert_eq!(resolved, Path::new("/fallback"));
    }

    #[test]
    fn resolve_deleted_dir_falls_back() {
        let dir = tempfile_dir();
        let pwd = dir.join("pwd");
        // A path that does not exist on disk → canonicalize fails → fallback.
        std::fs::write(&pwd, "/this/should/not/exist/ja-cwd-test\n").unwrap();
        let resolved = resolve(&pwd, Path::new("/fallback")).unwrap();
        assert_eq!(resolved, Path::new("/fallback"));
    }

    fn tempfile_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let unique = std::env::temp_dir().join(format!("ja-cwd-{}-{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&unique);
        unique
    }
}
