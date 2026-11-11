//! Sticky working-directory resolution for commands and spawns.
//!
//! The foreground `exec` path recovers the post-command cwd from a marker the
//! `bash -c` script's `EXIT` trap prints (to stderr) — the marker carries
//! `pwd -P` as a payload. `resolve_str` takes that extracted string and
//! resolves it honestly: NFC-normalize (macOS APFS stores decomposed NFD), then
//! `canonicalize` — the guard that makes cwd honest: if the command `rmdir`'d
//! its own cwd, the canonicalize fails and we fall back rather than report a
//! stale value.
//!
//! `spawn_dir` answers the other end of the same question: which directory a
//! spawn starts in. Existence is not enough: a directory whose mode carries no
//! search (`x`) bit cannot be entered by a normal process, while
//! `canonicalize` still returns it and a privileged caller's `access(X_OK)`
//! passes whatever the mode, so the mode bits are checked on their own
//! ([`enterable`]), the half that takes effect here; a refused directory
//! sends the spawn to the configured fallback together with a [`CwdReject`]
//! the caller reports. [`remembered`] closes the loop at the far end: a
//! command that leaves the shell in a directory nobody can enter does not
//! become the recorded cwd, so the next call starts from a directory the
//! kernel accepts.
//!
//! Foreground and background spawns share the resolution, so both enter the
//! agent's working directory — and both fall back into the same one.

use std::path::{Path, PathBuf};

use nix::errno::Errno;
use unicode_normalization::UnicodeNormalization;

/// A working directory refused for entering, with the errno `chdir` reports.
#[derive(Debug, Clone)]
pub(super) struct CwdReject {
    /// The directory that was refused.
    pub(super) path: PathBuf,
    /// One of `EACCES`, `ENOENT`, `ENOTDIR`.
    pub(super) errno: &'static str,
}

/// The errno name for an I/O error `chdir` can return about a directory, or
/// `None` for every other failure.
pub(super) fn errno_name(error: &std::io::Error) -> Option<&'static str> {
    name_of(Errno::from_raw(error.raw_os_error()?))
}

/// The errno name carried by a rendered error string.
///
/// A structured spawn failure keeps the errno only inside its rendered detail
/// (`<message> (os error <code>)`), which is where the background path has to
/// read it from: that path sees a [`crate::ShellError`], not the `io::Error`
/// the registry started with.
pub(super) fn errno_name_in(detail: &str) -> Option<&'static str> {
    let code = detail.rsplit_once("(os error ")?.1.strip_suffix(')')?;
    name_of(Errno::from_raw(code.parse().ok()?))
}

/// The name of an errno a directory can be refused with.
fn name_of(errno: Errno) -> Option<&'static str> {
    match errno {
        Errno::EACCES => Some("EACCES"),
        Errno::ENOENT => Some("ENOENT"),
        Errno::ENOTDIR => Some("ENOTDIR"),
        _ => None,
    }
}

/// Whether a process can enter `path`: it exists, it is a directory, its mode
/// carries a search (`x`) bit, and this process may search it.
///
/// Both halves are needed. `access(X_OK)` is the kernel's own answer for this
/// process, which matters for the ordinary case (another user's `0700`); a
/// privileged caller passes it whatever the mode, so the mode bits are
/// checked too — a directory with no search bit anywhere is what a normal
/// process is refused on, and the failure is silent enough that recording it
/// as a working directory locks the next call out.
pub(super) fn enterable(path: &Path) -> Result<(), CwdReject> {
    use std::os::unix::fs::PermissionsExt;

    let reject = |errno: &'static str| CwdReject {
        path: path.to_path_buf(),
        errno,
    };
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_dir() => Err(reject("ENOTDIR")),
        Ok(meta) if meta.permissions().mode() & 0o111 == 0 => Err(reject("EACCES")),
        Ok(_) if !crate::error::is_searchable(path) => Err(reject("EACCES")),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(reject("ENOENT")),
        // Any other refusal to stat the path (a parent without search
        // permission, a symlink loop) leaves the directory just as unusable.
        Err(_) => Err(reject("EACCES")),
    }
}

/// What the arrow in [`note`] points at. A refused spawn runs somewhere
/// else; a refused write-back leaves the record pointing at a directory of
/// its own choosing, which the command has already left.
#[derive(Debug, Clone, Copy)]
pub(super) enum NoteTarget {
    /// The command started in the named directory.
    Started,
    /// Nothing new is recorded from the refused candidate, so the record
    /// points here instead.
    Recorded,
}

/// The one line a refused directory produces, in the shape of its event:
/// `cwd fallback: <refused> (<errno>) -> started in <dir>` for a spawn that
/// ran elsewhere, `cwd fallback: <candidate> (<errno>) -> record now at <dir>`
/// for a record that points elsewhere.
pub(super) fn note(reject: &CwdReject, to: &Path, target: NoteTarget) -> String {
    let arrow = match target {
        NoteTarget::Started => "started in",
        NoteTarget::Recorded => "record now at",
    };
    format!(
        "cwd fallback: {} ({}) -> {} {}",
        reject.path.display(),
        reject.errno,
        arrow,
        to.display()
    )
}

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

/// The directory a spawn starts in: the cached working directory while it can
/// be entered, the configured fallback otherwise — plus the reason when the
/// cached one was refused, so the caller can report it.
///
/// Foreground and background spawns share the resolution, so a background task
/// starts where a foreground command would.
pub(super) fn spawn_dir(cached: &Path, fallback: &Path) -> (PathBuf, Option<CwdReject>) {
    match enterable(cached) {
        Ok(()) => {
            // Existence and the mode are settled; canonicalize still decides
            // the shape (symlinks resolved), and its own failure is the same
            // fallback every other unresolvable value takes.
            let dir = std::fs::canonicalize(cached).unwrap_or_else(|_| fallback.to_path_buf());
            (dir, None)
        }
        Err(reject) => (fallback.to_path_buf(), Some(reject)),
    }
}

/// The cwd to record after a command: the directory the command ended in when
/// it can be entered, else the directory recorded before it, else the
/// configured fallback. The refused candidate rides back so the caller can say
/// why the recorded cwd did not move.
pub(super) fn remembered(
    candidate: PathBuf,
    previous: &Path,
    fallback: &Path,
) -> (PathBuf, Option<CwdReject>) {
    match enterable(&candidate) {
        Ok(()) => (candidate, None),
        Err(reject) => {
            let kept = if enterable(previous).is_ok() {
                previous.to_path_buf()
            } else {
                fallback.to_path_buf()
            };
            (kept, Some(reject))
        }
    }
}

/// The directory to retry a refused spawn in, or `None` when there is nowhere
/// else to go.
///
/// With a sandbox installed the child runs in its own user namespace
/// (`install_user_mount_ns`), without CAP_DAC_OVERRIDE, so a directory this
/// side may search can still be refused there; without one, only the gap
/// between the check and the `chdir` remains. Retrying in the fallback keeps
/// the command runnable; the caller decides from the errno whether a retry
/// is warranted.
pub(super) fn retry_target(dir: &Path, fallback: &Path) -> Option<PathBuf> {
    if dir == fallback || enterable(fallback).is_err() {
        return None;
    }
    Some(fallback.to_path_buf())
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

    #[test]
    fn spawn_dir_prefers_the_cached_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (resolved, reject) = spawn_dir(dir.path(), Path::new("/fallback"));
        assert_eq!(resolved, std::fs::canonicalize(dir.path()).unwrap());
        assert_ne!(resolved, Path::new("/fallback"));
        assert!(reject.is_none(), "an enterable directory is no fallback");
    }

    #[test]
    fn spawn_dir_falls_back_when_the_cached_directory_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let (resolved, reject) = spawn_dir(&gone, Path::new("/fallback"));
        assert_eq!(resolved, Path::new("/fallback"));
        let reject = reject.expect("a missing directory is a refusal");
        assert_eq!(reject.errno, "ENOENT");
    }

    #[test]
    fn enterable_reports_a_file_as_enotdir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain");
        std::fs::write(&file, b"").unwrap();
        let reject = enterable(&file).expect_err("a file cannot be entered");
        assert_eq!(reject.errno, "ENOTDIR");
    }

    #[test]
    fn enterable_reports_a_missing_path_as_enoent() {
        let dir = tempfile::tempdir().unwrap();
        let reject = enterable(&dir.path().join("gone")).expect_err("missing");
        assert_eq!(reject.errno, "ENOENT");
    }

    #[test]
    fn enterable_refuses_a_directory_without_search_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("no-search");
        std::fs::create_dir(&plain).unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let checked = enterable(&plain);
        // Restore before asserting: a failed assert must not leave a
        // hard-to-remove directory behind for the next run.
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        let reject = checked.expect_err("no x bit means no entry");
        assert_eq!(reject.errno, "EACCES");
        assert_eq!(reject.path, plain);
    }

    #[test]
    fn spawn_dir_refuses_a_directory_without_search_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("no-search");
        std::fs::create_dir(&plain).unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (resolved, reject) = spawn_dir(&plain, dir.path());
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolved, dir.path());
        let reject = reject.expect("a directory without x bits is refused");
        assert_eq!(reject.errno, "EACCES");
        assert_eq!(reject.path, plain);
    }

    #[test]
    fn remembered_takes_an_enterable_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let (kept, reject) =
            remembered(dir.path().to_path_buf(), Path::new("/previous"), dir.path());
        assert_eq!(kept, dir.path());
        assert!(reject.is_none());
    }

    #[test]
    fn remembered_keeps_the_previous_directory_when_the_candidate_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let previous = dir.path().join("previous");
        let plain = dir.path().join("no-search");
        std::fs::create_dir(&previous).unwrap();
        std::fs::create_dir(&plain).unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (kept, reject) = remembered(plain.clone(), &previous, dir.path());
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(kept, previous);
        assert_eq!(reject.expect("refused candidate").errno, "EACCES");
    }

    #[test]
    fn remembered_falls_back_when_the_previous_directory_is_gone_too() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let (kept, reject) = remembered(gone.clone(), &gone, dir.path());
        assert_eq!(kept, dir.path());
        assert_eq!(reject.expect("refused candidate").errno, "ENOENT");
    }

    #[test]
    fn errno_name_names_only_entry_errnos() {
        use std::io::Error;

        let denied = Error::from_raw_os_error(Errno::EACCES as i32);
        let missing = Error::from_raw_os_error(Errno::ENOENT as i32);
        let plain_file = Error::from_raw_os_error(Errno::ENOTDIR as i32);
        let other = Error::from_raw_os_error(Errno::EINVAL as i32);
        assert_eq!(errno_name(&denied), Some("EACCES"));
        assert_eq!(errno_name(&missing), Some("ENOENT"));
        assert_eq!(errno_name(&plain_file), Some("ENOTDIR"));
        assert_eq!(errno_name(&other), None, "only entry errnos are named");
        assert_eq!(errno_name(&Error::other("x")), None);
    }

    #[test]
    fn errno_name_in_reads_a_rendered_detail() {
        assert_eq!(
            errno_name_in("Permission denied (os error 13)"),
            Some("EACCES")
        );
        assert_eq!(
            errno_name_in("No such file or directory (os error 2)"),
            Some("ENOENT")
        );
        assert_eq!(
            errno_name_in("Not a directory (os error 20)"),
            Some("ENOTDIR")
        );
        assert_eq!(
            errno_name_in("Connection reset by peer (os error 104)"),
            None
        );
        assert_eq!(errno_name_in("no errno here"), None);
    }

    #[test]
    fn retry_target_skips_the_directory_already_used() {
        let dir = tempfile::tempdir().unwrap();
        assert!(retry_target(dir.path(), dir.path()).is_none());
    }

    #[test]
    fn retry_target_skips_an_unenterable_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        assert!(retry_target(dir.path(), &gone).is_none());
    }

    #[test]
    fn retry_target_offers_an_enterable_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        assert_eq!(
            retry_target(dir.path(), other.path()),
            Some(other.path().to_path_buf())
        );
    }
}
