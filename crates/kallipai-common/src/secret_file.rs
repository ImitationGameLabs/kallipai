//! Atomic secret-file primitives for boot-provisioned credentials.
//!
//! One lifecycle shape uses these mechanics: a service-generated secret is
//! written once into persistent state and only read afterwards, so the
//! value stays stable across restarts. The guarantees a reader gets: never
//! a partial file (temp file + rename in the destination directory), and
//! an on-disk mode that is explicit rather than umask-dependent. Boot
//! provisioning callers (the platform-internal token, the minted admin
//! token) share this shape.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// How often a retrying reader re-checks for the file to appear.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long a booting consumer waits for the shared secret file to appear.
pub const BOOT_RETRY: Duration = Duration::from_secs(15);

/// Write `contents` to `path` atomically with an explicit mode.
///
/// The temp file is created in the destination's directory (same filesystem,
/// so the final rename is atomic) with the target mode already applied, then
/// synced and renamed over the destination. The parent directory must exist:
/// a missing directory is a deployment misconfiguration, not something to
/// paper over with an implicit mkdir.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("secret file path has no parent directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temp file next to {}", path.display()))?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting mode {:o} on temp file", mode))?;
    tmp.write_all(contents)
        .and_then(|()| tmp.as_file().sync_all())
        .with_context(|| format!("writing {}", path.display()))?;
    tmp.persist(path)
        .with_context(|| format!("persisting {}", path.display()))?;
    Ok(())
}

/// Read a trimmed secret from `path`, retrying until `timeout` elapses.
///
/// The bounded wait absorbs the boot race where a consumer starts after the
/// owning service's process exists but before the secret file is written
/// (systemd `Type=simple` marks a unit started at exec; compose
/// `service_started` at container start). Expiry is a hard error naming the
/// path: fail-closed, never a fallback to a weaker auth posture. Blocking;
/// boot path only.
pub fn read_trimmed_with_retry(path: &Path, timeout: Duration) -> anyhow::Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::read_to_string(path) {
            Ok(raw) => {
                let secret = raw.trim();
                if secret.is_empty() {
                    bail!(
                        "{} exists but is empty; refusing to guess (delete the file to re-provision)",
                        path.display()
                    );
                }
                return Ok(secret.to_owned());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if Instant::now() >= deadline {
                    bail!(
                        "secret file {} did not appear within {:?}",
                        path.display(),
                        timeout
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn write_atomic_sets_explicit_mode_and_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        write_atomic(&path, b"tok-1\n", 0o640).expect("write");
        let meta = fs::metadata(&path).expect("meta");
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read_to_string(&path).expect("read"), "tok-1\n");
    }

    #[test]
    fn write_atomic_overwrites_existing_and_replaces_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        write_atomic(&path, b"old", 0o600).expect("write 1");
        write_atomic(&path, b"new", 0o640).expect("write 2");
        assert_eq!(fs::read_to_string(&path).expect("read"), "new");
        let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn write_atomic_missing_parent_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope").join("secret");
        let err = write_atomic(&path, b"x", 0o600).expect_err("must fail");
        assert!(err.to_string().contains("temp file"), "{err}");
    }

    #[test]
    fn read_trims_surrounding_whitespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        fs::write(&path, "  tok\n").expect("write");
        let got = read_trimmed_with_retry(&path, Duration::from_millis(50)).expect("read");
        assert_eq!(got, "tok");
    }

    #[test]
    fn read_empty_file_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        fs::write(&path, "\n").expect("write");
        let err = read_trimmed_with_retry(&path, Duration::from_millis(50)).expect_err("must fail");
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn read_missing_path_times_out_naming_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent");
        let err =
            read_trimmed_with_retry(&path, Duration::from_millis(300)).expect_err("must fail");
        assert!(err.to_string().contains("absent"), "{err}");
    }

    #[test]
    fn read_waits_for_a_late_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("late");
        let writer = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            fs::write(writer, "late-token").expect("write");
        });
        let got = read_trimmed_with_retry(&path, Duration::from_secs(5)).expect("read");
        assert_eq!(got, "late-token");
    }
}
