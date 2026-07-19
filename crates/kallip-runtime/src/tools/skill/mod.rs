//! Minimal skill loading for kallip.
//!
//! Skills are suggestions, never mandatory instructions. They enter the
//! agent's context through the pinned layer of the context store.
//!
//! ## Skill identity
//!
//! A skill is uniquely identified by its **path relative to the skills root**
//! (e.g. `code/refactoring`). This path determines the on-disk layout
//! (`<skills_root>/<path>.md`) and is used for all lookups, routing,
//! and promote operations. The `name` field in YAML frontmatter is a display
//! label — it is returned by the metadata endpoint but is **not** used as an
//! identifier and is not required to match the path.
//!
//! The [`load_skill`] function resolves skill files from the shared skill
//! directory or an agent-local directory.

pub mod promote;
pub use promote::promote_skill_from_content;

use std::path::Path;

use anyhow::{Context, Result, bail};

use kallip_common::protocol::SkillMeta;

pub const META_SKILL_NAME: &str = "bootstrap";

const DEFAULT_META_SKILL: &str = r#"---
name: bootstrap
description: Working with your context — weighing what you see, and finding notes from past sessions
---

# Weigh everything in your context

Everything in your context — notes you pinned, your running summary, tool
output, what the user said, and any skill you have loaded — is input to your
judgment. Nothing here is a command to execute without weighing it against
what you see now. Past notes record decisions that may be stale or no longer
fit the situation; weigh them, do not follow them blindly.

Your data directory has a `skills/` folder — experience distilled in past
sessions, with an `index.md` listing what is there. Read the index when you
enter a new task or switch topics — the same boundary where you would evict
the previous task's context — and also whenever you hit something unfamiliar
mid-task, before you re-derive or go read external docs: it may already hold
the answer, and one index read is cheap. When a note genuinely matches what
you are doing, read it, then in the next turn pin what you read with
`context_pin_last` (kind `tool-result`, label `skill:<name>`); don't load
speculatively — a skill you don't use occupies a pin slot. Loaded notes stay
in context across turns; unpin them when the task moves on. The
`skill-management` notes cover creating and promoting your own; the
`context-management` notes cover what to keep and what to evict.
"#;

/// Returns the shared skill directory.
///
/// `KALLIP_SKILLS_ROOT`, if set, is used verbatim. Otherwise the directory
/// is `<data_dir_root>/skills/` — i.e. `$KALLIP_DATA_DIR/skills/` when the
/// env var is set, or `~/.local/share/kallip/skills/` via the XDG fallback
/// (see [`crate::persistence::data_dir_root`]).
pub fn skill_dir() -> Result<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("KALLIP_SKILLS_ROOT")
        && !dir.is_empty()
    {
        return Ok(std::path::PathBuf::from(dir));
    }
    Ok(crate::persistence::data_dir_root()?.join("skills"))
}

/// Parses YAML frontmatter from a skill markdown file.
///
/// Returns `None` if no frontmatter is present. Handles the simple
/// `key: value` format used in skill files without requiring a YAML library.
pub fn parse_frontmatter_meta(content: &str) -> Option<SkillMeta> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = trimmed[3..].trim_start_matches(['\n', '\r']);
    let end = after_first.find("\n---")?;

    let frontmatter = &after_first[..end];
    let mut name = None;
    let mut description = None;

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("description:") {
            description = Some(rest.trim().to_owned());
        }
    }

    name.map(|n| SkillMeta {
        name: n,
        description,
    })
}

/// Resolves a skill file to its raw content.
///
/// Checks the agent-local directory first (if `agent_dir` is provided),
/// then falls back to the shared skill directory. Returns the raw file
/// content including frontmatter.
fn resolve_skill_content(name: &str, agent_dir: Option<&Path>) -> Result<String> {
    // Try agent-local first.
    if let Some(sd) = agent_dir {
        let local_path = sd.join("skills").join(format!("{name}.md"));
        if local_path.exists() {
            return std::fs::read_to_string(&local_path)
                .with_context(|| format!("failed to read local skill '{name}'"));
        }
    }

    // Fall back to shared.
    let path = skill_dir()?.join(format!("{name}.md"));
    std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read skill '{name}' from {}", path.display()))
}

/// Reads a skill file and returns its metadata (name + description).
///
/// If the file has no frontmatter, `name` defaults to the last path
/// component of the skill name.
pub fn skill_metadata(name: &str, agent_dir: Option<&Path>) -> Result<SkillMeta> {
    validate_skill_name(name)?;
    let content = resolve_skill_content(name, agent_dir)?;

    Ok(parse_frontmatter_meta(&content).unwrap_or_else(|| {
        let default_name = name.rsplit('/').next().unwrap_or(name).to_owned();
        SkillMeta {
            name: default_name,
            description: None,
        }
    }))
}

/// Validates a skill name for path traversal attacks.
///
/// Allows `/` for nested categories (e.g. `code/refactoring`) but rejects
/// `..` components, backslashes, and empty components.
pub fn validate_skill_name(name: &str) -> Result<()> {
    if name.contains('\\') {
        bail!("invalid skill name: {name}");
    }
    for component in name.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            bail!("invalid skill name: {name}");
        }
    }
    Ok(())
}

/// Reads a skill file, strips frontmatter, and returns the body.
///
/// Checks the agent-local directory first (if `agent_dir` is provided),
/// then falls back to the shared skill directory. Local takes precedence
/// on name collision.
pub fn load_skill(name: &str, agent_dir: Option<&Path>) -> Result<String> {
    validate_skill_name(name)?;
    let content = resolve_skill_content(name, agent_dir)?;
    Ok(strip_frontmatter(&content).trim().to_owned())
}

/// Returns the built-in meta-skill content (a thin "floor" on working with
/// your context — a universal judgment stance, plus a pointer to the skill
/// index for notes from past sessions).
///
/// The meta-skill is compiled into the binary and never written to disk.
/// It is appended to the system prompt at agent spawn time. It deliberately
/// teaches no operations: skill lifecycle lives in the `skill-management`
/// skill and context hygiene in the `context-management` skill, both
/// discoverable via the index it points at.
pub fn meta_skill_content() -> &'static str {
    strip_frontmatter(DEFAULT_META_SKILL).trim()
}

/// Strips YAML frontmatter (content between `---` delimiters).
///
/// Returns the body after the second `---`. If no frontmatter is found,
/// returns the original content unchanged.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content;
    }
    let after_first = &trimmed[3..];
    let after_first = after_first.trim_start_matches(['\n', '\r']);
    if let Some(end) = after_first.find("\n---") {
        let body = after_first[end + 4..].trim_start_matches(['\n', '\r']);
        if body.is_empty() {
            return content;
        }
        let offset = body.as_ptr() as usize - content.as_ptr() as usize;
        return &content[offset..];
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_surfaces_discovery_and_stance() {
        // The compiled meta-skill (appended to every agent's prompt at spawn,
        // routes/agent.rs) is the ONLY guaranteed surface an agent sees before
        // it discovers anything. It is kept deliberately thin: a universal
        // judgment stance plus a discovery pointer. It teaches no operations
        // -- those live in the skill files this test also pins down below.
        //
        // Assert against the RAW constant so frontmatter regressions are
        // caught (meta_skill_content() strips frontmatter).

        // --- Positive: the discovery contract ---
        assert!(
            DEFAULT_META_SKILL.contains("context_pin_last"),
            "floor must name the load verb: {DEFAULT_META_SKILL}"
        );
        assert!(
            DEFAULT_META_SKILL.contains("index.md") && DEFAULT_META_SKILL.contains("skills/"),
            "floor must point at the skill index: {DEFAULT_META_SKILL}"
        );
        assert!(
            DEFAULT_META_SKILL.contains("weigh") || DEFAULT_META_SKILL.contains("judgment"),
            "floor must establish the judgment stance: {DEFAULT_META_SKILL}"
        );

        // --- Negative: deliberately dropped, paired with the positives above
        // so a future edit cannot satisfy them by deleting discovery. ---
        assert!(
            !DEFAULT_META_SKILL.contains("skill system"),
            "floor must not re-specialize skills as a 'system': {DEFAULT_META_SKILL}"
        );
        assert!(
            !meta_skill_content().contains("context_unpin"),
            "floor must not enumerate secondary context tools (self-describing via tool layer)"
        );
        assert!(
            !DEFAULT_META_SKILL.contains("Skill system usage and behavioral guidelines"),
            "frontmatter description must not repeat the old framing"
        );

        // The operations the floor no longer teaches (promote, unpin, evict)
        // live in the skill files it points at. We do NOT compile-bind those
        // files here: skills/ is a content directory the runtime loads from a
        // data dir at runtime (skill_dir()), not a build-time dependency.
        // Coverage of those operations is a skills/ review concern, kept
        // separate so prose edits in skills/ cannot break compilation of this
        // crate.
    }

    #[test]
    fn strip_frontmatter_with_frontmatter() {
        let input = "---\nname: test\n---\nHello world\n";
        assert_eq!(strip_frontmatter(input), "Hello world\n");
    }

    #[test]
    fn strip_frontmatter_without_frontmatter() {
        let input = "Hello world\n";
        assert_eq!(strip_frontmatter(input), "Hello world\n");
    }

    #[test]
    fn strip_frontmatter_empty_body() {
        let input = "---\nname: test\n---\n";
        assert_eq!(strip_frontmatter(input), input);
    }

    #[test]
    fn validate_skill_name_allows_nested() {
        assert!(validate_skill_name("code/refactoring").is_ok());
        assert!(validate_skill_name("a/b/c").is_ok());
        assert!(validate_skill_name("simple").is_ok());
    }

    #[test]
    fn validate_skill_name_rejects_traversal() {
        assert!(validate_skill_name("../etc/passwd").is_err());
        assert!(validate_skill_name("foo/..").is_err());
        assert!(validate_skill_name("foo/../bar").is_err());
        assert!(validate_skill_name("foo//bar").is_err());
        assert!(validate_skill_name("foo/./bar").is_err());
    }

    #[test]
    fn load_skill_rejects_backslash() {
        assert!(load_skill("foo\\bar", None).is_err());
    }

    #[test]
    fn parse_frontmatter_meta_extracts_name_and_description() {
        let input = "---\nname: refactoring\ndescription: Safe patterns\n---\nBody here\n";
        let meta = parse_frontmatter_meta(input).unwrap();
        assert_eq!(meta.name, "refactoring");
        assert_eq!(meta.description.as_deref(), Some("Safe patterns"));
    }

    #[test]
    fn parse_frontmatter_meta_name_only() {
        let input = "---\nname: minimal\n---\nBody\n";
        let meta = parse_frontmatter_meta(input).unwrap();
        assert_eq!(meta.name, "minimal");
        assert!(meta.description.is_none());
    }

    #[test]
    fn parse_frontmatter_meta_returns_none_without_frontmatter() {
        let input = "Just plain markdown.\n";
        assert!(parse_frontmatter_meta(input).is_none());
    }
}
