//! Canonical directory archives: canonical tar -> content-addressed blob,
//! and the extraction half of the round trip.
//!
//! Canonical packing is what makes the archive verifiable: the same tree
//! always produces byte-identical bytes (entries sorted by path, mtime/uid/
//! gid zeroed, fixed modes), so the content address is a property of the
//! directory content, not of when it was packed. Regular files and
//! directories only — a directory with anything else is a caller error,
//! not a silent skip.
//!
//! `max_bytes` enforces the semantic size cap while packing: the running
//! total is checked as entries are appended, so an oversized tree aborts
//! the pack early instead of reading all of it and refusing afterwards.

use std::fs;
use std::io::Cursor;
use std::path::Path;

use crate::Error;

/// Packs `dir` into canonical tar bytes (whole directory, relative
/// paths). Symlinks are rejected outright — the canonical archive
/// contains regular files and directories only; a link could silently
/// pull out-of-dossier content into the archive or loop the walk.
/// Names must be UTF-8 too: a non-UTF-8 name would be mangled into
/// the tar entry name, so it is a caller error as well.
///
/// `max_bytes` caps the packed byte total: the walk aborts with
/// [`Error::DossierTooLarge`] as soon as the running total crosses it,
/// before reading the remainder of the tree.
pub fn pack_dir(dir: &Path, max_bytes: Option<u64>) -> Result<Vec<u8>, Error> {
    let mut paths: Vec<_> = collect_relative(dir, dir)?;
    // Fixed ordering is half of canonicity.
    paths.sort();

    let mut builder = tar::Builder::new(Vec::new());
    let mut total: u64 = 0;
    for rel in paths {
        let full = dir.join(&rel);
        let meta = fs::symlink_metadata(&full)?;
        // symlink_metadata, not metadata: a link swapped in between the
        // walk above and this stat must land in the error arm, not get
        // silently dereferenced into the archive.
        if meta.is_dir() {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o755);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_entry_type(tar::EntryType::Directory);
            builder.append_data(&mut header, format!("{}/", rel.display()), std::io::empty())?;
        } else if meta.is_file() {
            let mut header = tar::Header::new_gnu();
            header.set_size(meta.len());
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_entry_type(tar::EntryType::Regular);
            // Predict from the stat before reading: one huge file should
            // abort the pack before its bytes are read, not after.
            total += meta.len();
            if let Some(max) = max_bytes
                && total > max
            {
                return Err(Error::DossierTooLarge {
                    size: total as usize,
                    max: max as usize,
                });
            }
            let bytes = fs::read(&full)?;
            total += bytes.len() as u64;
            if let Some(max) = max_bytes
                && total > max
            {
                return Err(Error::DossierTooLarge {
                    size: total as usize,
                    max: max as usize,
                });
            }
            builder.append_data(&mut header, &rel, bytes.as_slice())?;
        } else {
            return Err(Error::NonRegularEntry(rel.display().to_string()));
        }
    }
    Ok(builder.into_inner()?)
}

/// Stores the packed bytes; the returned id is the content address.
pub async fn ingest(
    blobs: &dyn crate::store::BlobStore,
    bytes: Vec<u8>,
) -> Result<crate::BlobId, Error> {
    let mut cursor = Cursor::new(bytes);
    let id = blobs.put(&mut cursor).await?.id;
    Ok(id)
}

/// Fetches the closed archive and unpacks it under `dest` (created if
/// absent). Entry paths are checked against `dest` (`unpack_in`).
pub async fn extract(
    blobs: &dyn crate::store::BlobStore,
    id: &crate::BlobId,
    dest: &Path,
) -> Result<(), Error> {
    let bytes = blobs.get(id).await?;
    fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(&bytes[..]);
    for entry in archive.entries()? {
        let mut entry = entry?;
        entry.unpack_in(dest)?;
    }
    Ok(())
}

fn collect_relative(root: &Path, dir: &Path) -> Result<Vec<std::path::PathBuf>, Error> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .expect("walk rooted at `root`")
            .to_path_buf();
        // A non-UTF-8 name would be lossily mangled the moment it
        // became a tar entry name; reject it here like any other
        // non-canonical content instead of archiving a corrupted name.
        if rel.to_str().is_none() {
            return Err(Error::NonUtf8Entry(rel.display().to_string()));
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(Error::SymlinkEntry(rel.display().to_string()));
        }
        if meta.is_dir() {
            out.push(rel.clone());
            out.extend(collect_relative(root, &path)?);
        } else {
            out.push(rel);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::LocalBackend;
    use std::fs;

    fn scratch(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        (tmp, dir)
    }

    #[test]
    fn pack_is_canonical_same_tree_same_bytes() {
        let (_t1, a) = scratch("dossier");
        let (_t2, b) = scratch("dossier");
        for root in [&a, &b] {
            fs::write(root.join("plan.md"), "# plan\n").unwrap();
            fs::create_dir(root.join("notes")).unwrap();
            fs::write(root.join("notes/z.md"), "z").unwrap();
            fs::write(root.join("notes/a.md"), "a").unwrap();
        }
        let bytes_a = pack_dir(&a, None).unwrap();
        let bytes_b = pack_dir(&b, None).unwrap();
        assert_eq!(bytes_a, bytes_b, "same tree must pack identically");

        // Sorted order: "notes" precedes "plan.md", "notes/a.md" before
        // "notes/z.md" — independent of readdir order.
        let text = String::from_utf8_lossy(&bytes_a);
        let a_pos = text.find("notes/a.md").unwrap();
        let z_pos = text.find("notes/z.md").unwrap();
        assert!(a_pos < z_pos);
    }

    #[tokio::test]
    async fn ingest_extract_round_trip() {
        let (_t, dir) = scratch("dossier");
        fs::write(dir.join("plan.md"), "# plan\n").unwrap();
        let packed = pack_dir(&dir, None).unwrap();

        let root = tempfile::tempdir().unwrap();
        let blobs = LocalBackend::new(root.path().join("blobs"));
        let id = ingest(&blobs, packed.clone()).await.unwrap();

        // Content addressing: repacking the same content is idempotent.
        let id2 = ingest(&blobs, pack_dir(&dir, None).unwrap()).await.unwrap();
        assert_eq!(id.as_str(), id2.as_str());

        let out = tempfile::tempdir().unwrap();
        extract(&blobs, &id, &out.path().join("unpacked"))
            .await
            .unwrap();
        let restored = out.path().join("unpacked/plan.md");
        assert_eq!(fs::read_to_string(restored).unwrap(), "# plan\n");
    }

    #[test]
    #[cfg(unix)]
    fn pack_rejects_symlinks_instead_of_dereferencing_or_looping() {
        let (_t, dir) = scratch("dossier");
        fs::write(dir.join("real.txt"), "stay").unwrap();
        // A file symlink must be an error, not silently dereferenced
        // into the archive (out-of-dossier content would leak in).
        std::os::unix::fs::symlink("/etc/hostname", dir.join("link.txt")).unwrap();
        let err = pack_dir(&dir, None).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");

        // A directory symlink pointing at an ancestor must not loop the
        // walk into a stack overflow either.
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();
        let err = pack_dir(&dir, None).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn pack_rejects_non_utf8_names_instead_of_lossy_archiving() {
        let (_t, dir) = scratch("dossier");
        fs::write(dir.join("plan.md"), "# plan\n").unwrap();
        // A non-UTF-8 name cannot survive the tar entry name round trip
        // without loss; caller error, matching the symlink rule.
        let bad: std::ffi::OsString = std::os::unix::ffi::OsStringExt::from_vec(vec![0xff]);
        fs::write(dir.join(bad), "x").unwrap();
        let err = pack_dir(&dir, None).unwrap_err();
        assert!(err.to_string().contains("non-UTF-8"), "{err}");
    }

    #[test]
    fn pack_rejects_a_missing_directory_as_an_io_error() {
        let err = pack_dir(Path::new("/nonexistent/dossier"), None).unwrap_err();
        assert!(err.to_string().contains("No such file"), "{err}");
    }

    /// The size cap fires mid-pack (early stop): a tree over the cap is
    /// refused with DossierTooLarge before the rest of the tree is read.
    #[test]
    fn pack_aborts_early_when_the_size_cap_is_crossed() {
        let (_t, dir) = scratch("dossier");
        fs::write(dir.join("small.txt"), "ok").unwrap();
        fs::write(dir.join("big.txt"), "x".repeat(4096)).unwrap();
        let err = pack_dir(&dir, Some(1024)).unwrap_err();
        assert!(
            matches!(err, Error::DossierTooLarge { ref max, .. } if *max == 1024),
            "{err}"
        );
    }
}
