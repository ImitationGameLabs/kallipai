//! Sticky working-directory resolution after a command.
//!
//! The foreground `exec` path recovers the post-command cwd from a marker the
//! `bash -c` script's `EXIT` trap prints (to stderr) — the marker carries
//! `pwd -P` as a payload. `resolve_str` takes that extracted string and
//! resolves it honestly: NFC-normalize (macOS APFS stores decomposed NFD), then
//! `canonicalize` — the guard that makes cwd honest: if the command `rmdir`'d
//! its own cwd, the canonicalize fails and we fall back rather than report a
//! stale value.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// Resolve a post-command cwd string (the payload from the EXIT-trap marker)
/// honestly.
///
/// `raw` is trimmed (defensively — `pwd -P` is newline-terminated, and CRLF
/// edges on some emulated environments should not leak), NFC-normalized so the
/// same path compares equal across runs, and `canonicalize`d. Returns `fallback`
/// when the value is empty or the directory no longer exists — so the result is
/// never stale, only an explicit "unknown -> fallback".
pub(super) fn resolve_str(raw: &str, fallback: &Path) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fallback.to_path_buf();
    }

    // NFC-normalize so the same path compares equal across runs.
    let nfc: String = trimmed.nfc().collect();
    let candidate = PathBuf::from(nfc);

    // canonicalize confirms the dir still exists and resolves symlinks.
    std::fs::canonicalize(&candidate).unwrap_or_else(|_| fallback.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_str_reads_payload() {
        let resolved = resolve_str("/tmp\n", Path::new("/fallback"));
        // /tmp canonicalizes to itself on most systems.
        assert!(resolved.ends_with("tmp") || resolved == Path::new("/tmp"));
    }

    #[test]
    fn resolve_str_trims_crlf() {
        // A trailing CR (CRLF edge) must not leak into the candidate path.
        let resolved = resolve_str("/tmp\r\n", Path::new("/fallback"));
        assert!(resolved.ends_with("tmp") || resolved == Path::new("/tmp"));
    }

    #[test]
    fn resolve_str_empty_falls_back() {
        let resolved = resolve_str("   \n", Path::new("/fallback"));
        assert_eq!(resolved, Path::new("/fallback"));
    }

    #[test]
    fn resolve_str_blank_falls_back() {
        let resolved = resolve_str("", Path::new("/fallback"));
        assert_eq!(resolved, Path::new("/fallback"));
    }

    #[test]
    fn resolve_str_deleted_dir_falls_back() {
        // A path that does not exist on disk -> canonicalize fails -> fallback.
        let resolved = resolve_str("/this/should/not/exist/ja-cwd-test", Path::new("/fallback"));
        assert_eq!(resolved, Path::new("/fallback"));
    }
}
