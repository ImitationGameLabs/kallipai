//! Skill promotion: writes validated content to the shared directory.
//!
//! This is the I/O layer used by the daemon's approve path. The caller
//! (the promote-request route handler) is responsible for frontmatter
//! validation and the consistency check against the previous snapshot.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{META_SKILL_NAME, validate_skill_name};

/// Promotes a skill using pre-validated content (not read from disk).
///
/// Validates the skill name and writes the content to the shared directory,
/// overwriting any existing file. Used by the promote-request approval path
/// where content was snapshotted at submission time and the caller has
/// already performed the consistency check against the previous snapshot.
/// The caller is responsible for frontmatter validation.
pub fn promote_skill_from_content(name: &str, content: &str, shared_root: &Path) -> Result<String> {
    validate_skill_name(name)?;

    if name == META_SKILL_NAME {
        bail!(
            "cannot promote the '{META_SKILL_NAME}' skill; \
             it is managed by the skill system"
        );
    }

    let dest_dir = shared_root.join(name);
    let dest = dest_dir.join("SKILL.md");

    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("failed to create directory {}", dest_dir.display()))?;
    crate::persistence::atomic_write(&dest, content)
        .with_context(|| format!("failed to write skill to {}", dest.display()))?;

    Ok(dest.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promote_skill_from_content_writes_to_shared() {
        let shared = tempfile::tempdir().unwrap();
        let content = "---\nname: my-skill\ndescription: test\n---\nBody\n";

        let dest = promote_skill_from_content("my-skill", content, shared.path()).unwrap();
        assert!(dest.contains("my-skill"));
        assert!(std::path::Path::new(&dest).exists());

        let written = std::fs::read_to_string(&dest).unwrap();
        assert!(written.contains("name: my-skill"));
    }

    #[test]
    fn promote_skill_from_content_rejects_bootstrap() {
        let shared = tempfile::tempdir().unwrap();
        let err = promote_skill_from_content(
            "bootstrap",
            "---\nname: bootstrap\n---\nBody\n",
            shared.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("bootstrap"));
    }

    #[test]
    fn promote_skill_from_content_overwrites_existing() {
        // The function unconditionally overwrites; the caller is responsible
        // for the consistency check against the previous snapshot.
        let shared = tempfile::tempdir().unwrap();
        let dest_dir = shared.path().join("existing");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(dest_dir.join("SKILL.md"), "old content").unwrap();

        promote_skill_from_content("existing", "---\nname: existing\n---\nNew\n", shared.path())
            .unwrap();
        let written =
            std::fs::read_to_string(shared.path().join("existing").join("SKILL.md")).unwrap();
        assert!(written.contains("New"));
    }

    #[test]
    fn promote_skill_from_content_rejects_invalid_name() {
        let shared = tempfile::tempdir().unwrap();
        let err = promote_skill_from_content("../evil", "x", shared.path()).unwrap_err();
        assert!(err.to_string().contains("invalid skill name"));
    }

    #[test]
    fn promote_skill_from_content_supports_nested_name() {
        let shared = tempfile::tempdir().unwrap();
        let dest = promote_skill_from_content(
            "code/refactor",
            "---\nname: refactor\n---\nBody\n",
            shared.path(),
        )
        .unwrap();
        assert!(dest.contains("code/refactor"));
        assert!(std::path::Path::new(&dest).exists());
    }
}
