//! The single answer to "is this instance enrolled?", shared by the daemon
//! and the tagma.
//!
//! Enrollment material lives at `<data root>/credentials/<entry>/`: the
//! archeion-issued `tagma.id`, the secret `tagma.token` and the non-secret
//! `archeion.url` — all three landing `0o600`. The call sites that decide
//! boot behaviour from those files are the tagma's relay activation and
//! the daemon's replay filter, adopt probe and directory scan, and they
//! must not answer the same question differently. A blank file is not an
//! enrollment: it is what an interrupted write leaves behind, and each
//! of those call sites has to read it as not-enrolled.
//!
//! The predicate, in one place: enrolled means both files are readable and
//! non-blank after trimming. [`read_entry`] hands the tagma loader the pair
//! it authenticates with; [`read_enrolled_id`] answers the same question
//! with a narrower answer: the id alone. That accessor is the daemon's —
//! it opens `tagma.token` only to decide blank-or-not and never returns
//! it (`InstanceInfo.tagma_id` carries the id alone; see `wire.rs`).
//!
//! Holding the on-disk layout here is deliberate, and `socket.rs` is the
//! precedent: this crate exists so the daemon and its clients agree on the
//! daemon's paths, and the enrollment predicate is the same kind of
//! agreement.

use std::fs;
use std::path::{Path, PathBuf};

/// File name of the archeion-issued identity inside an entry directory.
pub const TAGMA_ID_FILE: &str = "tagma.id";

/// File name of the archeion-issued secret inside an entry directory.
pub const TAGMA_TOKEN_FILE: &str = "tagma.token";

/// Directory under the instance data root holding one subdirectory per relay
/// entry.
const CREDENTIALS_DIR: &str = "credentials";

/// A completed enrollment: the archeion-issued pair, both sides trimmed.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredEnrollment {
    pub id: String,
    pub token: String,
}

/// Redacted on purpose: the token is the secret half of the pair, and a
/// derived `Debug` would print it wherever an enrollment is formatted.
impl std::fmt::Debug for StoredEnrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredEnrollment")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// The stored enrollment in `entry_dir`, or `None` when it holds none.
///
/// `None` covers every shape that cannot authenticate: a missing file, an
/// unreadable one, a directory where a file belongs, and a file that is
/// blank or whitespace-only — that last one is what an interrupted write
/// leaves behind, and it is not an enrollment.
pub fn read_entry(entry_dir: &Path) -> Option<StoredEnrollment> {
    let id = read_non_blank(&entry_dir.join(TAGMA_ID_FILE))?;
    let token = read_non_blank(&entry_dir.join(TAGMA_TOKEN_FILE))?;
    Some(StoredEnrollment { id, token })
}

/// The enrolled identity in `entry_dir` under the [`read_entry`] predicate,
/// or `None` when the entry is not enrolled.
///
/// The same question with a narrower answer. The daemon's scan needs the
/// verdict and the id; the adopt probe and the replay filter need only
/// the verdict. None of them see the secret: `tagma.token` is opened to
/// decide blank-or-not and is not returned.
pub fn read_enrolled_id(entry_dir: &Path) -> Option<String> {
    read_entry(entry_dir).map(|stored| stored.id)
}

/// The root of the credential layout under `data_dir`: one directory per
/// relay entry, beside the shared `device.key` and `operator-token.env`.
pub fn credentials_root(data_dir: &Path) -> PathBuf {
    data_dir.join(CREDENTIALS_DIR)
}

/// The credential directory of the relay entry `name`.
pub fn entry_dir(data_dir: &Path, name: &str) -> PathBuf {
    credentials_root(data_dir).join(name)
}

/// The credential entry directories under `data_dir`, sorted by file name.
///
/// Sorted because every caller wants a stable pick when several entries
/// exist: the scan takes the first enrolled entry, and the adopt probe
/// asks whether any is. An entry counts when its path resolves to a
/// directory, so a symlinked entry directory is listed too — the readers
/// above open the files inside and follow symlinks exactly the same way,
/// and the two sides disagreeing about whether an entry exists is what
/// this module exists to prevent. Entry names are never required to be
/// valid UTF-8: nothing here needs the name as text. A missing or
/// unreadable `credentials/` directory yields an empty list rather than an
/// error: no entries already means not-enrolled, which is what the
/// callers do with it.
pub fn entry_dirs(data_dir: &Path) -> Vec<PathBuf> {
    let root = credentials_root(data_dir);
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Read `path` and trim it; missing, unreadable and blank all read as absent.
fn read_non_blank(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one entry directory; `None` means "leave that file out".
    fn entry(dir: &Path, id: Option<&str>, token: Option<&str>) {
        fs::create_dir_all(dir).expect("entry dir");
        if let Some(id) = id {
            fs::write(dir.join(TAGMA_ID_FILE), id).expect("write id");
        }
        if let Some(token) = token {
            fs::write(dir.join(TAGMA_TOKEN_FILE), token).expect("write token");
        }
    }

    fn enrolled_pair(id: &str, token: &str) -> Option<StoredEnrollment> {
        Some(StoredEnrollment {
            id: id.to_owned(),
            token: token.to_owned(),
        })
    }

    #[test]
    fn a_missing_half_is_not_an_enrollment() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("default");
        assert_eq!(read_entry(&dir), None, "no directory at all");
        let id_only = root.path().join("id_only");
        entry(&id_only, Some("tid-1"), None);
        assert_eq!(read_entry(&id_only), None, "an id without a token");
        let token_only = root.path().join("token_only");
        entry(&token_only, None, Some("sk-1"));
        assert_eq!(read_entry(&token_only), None, "a token without an id");
    }

    #[test]
    fn blank_files_are_not_an_enrollment() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("default");
        entry(&dir, Some(""), Some("sk-1"));
        assert_eq!(read_entry(&dir), None, "an empty id");
        entry(&dir, Some("tid-1"), Some("   \n"));
        assert_eq!(read_entry(&dir), None, "a whitespace-only token");
        entry(&dir, Some("tid-1"), Some("sk-1"));
        assert_eq!(read_entry(&dir), enrolled_pair("tid-1", "sk-1"));
    }

    #[test]
    fn a_directory_where_a_file_belongs_is_not_an_enrollment() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("default");
        fs::create_dir_all(dir.join(TAGMA_ID_FILE)).expect("dir as file");
        fs::write(dir.join(TAGMA_TOKEN_FILE), "sk-1").expect("write token");
        assert_eq!(read_entry(&dir), None);
    }

    #[test]
    fn stored_values_come_back_trimmed() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("default");
        entry(&dir, Some("  tid-1\n"), Some("\tsk-1 \n"));
        assert_eq!(read_entry(&dir), enrolled_pair("tid-1", "sk-1"));
    }

    #[test]
    fn the_id_verdict_matches_the_pair_verdict() {
        let root = tempfile::tempdir().expect("tempdir");
        let cases = [
            ("both", Some("tid-1"), Some("sk-1")),
            ("blank_token", Some("tid-1"), Some(" ")),
            ("blank_id", Some(" "), Some("sk-1")),
            ("id_only", Some("tid-1"), None),
        ];
        for (name, id, token) in cases {
            let dir = root.path().join(name);
            entry(&dir, id, token);
            assert_eq!(
                read_enrolled_id(&dir),
                read_entry(&dir).map(|stored| stored.id),
                "{name}: both accessors answer the same question"
            );
        }
    }

    #[test]
    fn entry_dirs_lists_the_sorted_directories_only() {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(
            entry_dirs(root.path()).is_empty(),
            "a missing credentials/ directory is an empty list, not an error"
        );
        // Created out of alphabetical order, so a listing that echoed the
        // directory order would not match the sorted expectation.
        for name in ["b", "a", "d", "c"] {
            fs::create_dir_all(root.path().join("credentials").join(name)).expect("entry");
        }
        fs::write(root.path().join("credentials/loose"), "not a dir").expect("loose file");
        let names: Vec<String> = entry_dirs(root.path())
            .into_iter()
            .map(|dir| {
                dir.file_name()
                    .expect("entry dir has a name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
    }

    #[test]
    fn a_symlinked_entry_directory_is_listed_and_read_the_same_way() {
        let root = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        entry(&outside.path().join("default"), Some("tid-1"), Some("sk-1"));
        fs::create_dir_all(root.path().join("credentials")).expect("credentials dir");
        let linked = root.path().join("credentials/linked");
        std::os::unix::fs::symlink(outside.path().join("default"), &linked).expect("symlink");
        let dirs = entry_dirs(root.path());
        assert_eq!(dirs, [linked], "a symlinked entry is an entry");
        let listed = dirs.first().expect("one entry is listed");
        assert_eq!(
            read_enrolled_id(listed),
            Some("tid-1".to_owned()),
            "the listing and the predicate agree about the same entry"
        );
    }

    #[test]
    fn an_entry_name_that_is_not_utf8_is_still_listed() {
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir().expect("tempdir");
        let name = std::ffi::OsStr::from_bytes(&[b'c', b'a', b'f', 0xe9]);
        let dir = root.path().join("credentials").join(name);
        entry(&dir, Some("tid-1"), Some("sk-1"));
        assert_eq!(entry_dirs(root.path()), [dir]);
    }

    #[test]
    fn a_formatted_enrollment_keeps_the_id_and_hides_the_token() {
        let stored = StoredEnrollment {
            id: "tid-1".to_string(),
            token: "sk-secret".to_string(),
        };
        let shown = format!("{stored:?}");
        assert!(shown.contains("tid-1"), "{shown}");
        assert!(shown.contains("[REDACTED]"), "{shown}");
        assert!(!shown.contains("sk-secret"), "{shown}");
    }
}
