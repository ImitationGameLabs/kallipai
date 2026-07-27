//! Skill frontmatter parsing.

/// Skill metadata parsed from YAML frontmatter.
///
/// **Note:** `name` here is a display label from the frontmatter, not the
/// canonical skill identifier. The skill's unique identity is its path
/// relative to the skills root (e.g. `code/refactoring`), which determines
/// the on-disk location and is used for all lookups and routing.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: Option<String>,
}

/// Parse YAML frontmatter from a skill markdown file into [`SkillMeta`].
///
/// Handles the simple `key: value` format used in skill files without pulling
/// in a YAML library. Returns `None` if no frontmatter is present OR no `name`
/// field is present (a skill's `name` is required). Only the frontmatter
/// between the `---` delimiters is scanned; any prose body after the closing
/// `---` is ignored.
pub fn parse_frontmatter(content: &str) -> Option<SkillMeta> {
    frontmatter_value(content, "name").map(|name| SkillMeta {
        name,
        description: frontmatter_value(content, "description"),
    })
}

/// Parse only the `description` field from frontmatter — for category
/// `README.md` files that carry a description but no `name` (the category's
/// navigable identifier is its directory name, not a frontmatter label).
/// Returns `None` if no frontmatter or no `description` field.
pub fn parse_frontmatter_description(content: &str) -> Option<String> {
    frontmatter_value(content, "description")
}

/// Extract the first `key: value` line from YAML frontmatter. Returns `None`
/// if there is no frontmatter block or no matching key.
fn frontmatter_value(content: &str, key: &str) -> Option<String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = trimmed[3..].trim_start_matches(['\n', '\r']);
    let end = after_first.find("\n---")?;
    let prefix = format!("{key}:");

    for line in after_first[..end].lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&prefix) {
            return Some(rest.trim().to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_extracts_name_and_description() {
        let content = "---\nname: refactoring\ndescription: safe refactor patterns\n---\nbody";
        let meta = parse_frontmatter(content).expect("frontmatter present");
        assert_eq!(meta.name, "refactoring");
        assert_eq!(meta.description.as_deref(), Some("safe refactor patterns"));
    }

    #[test]
    fn parse_frontmatter_ignores_body_after_closing_delimiter() {
        // A README with prose + tables after the frontmatter parses cleanly.
        let content =
            "---\nname: Agent skills\ndescription: nav\n---\n\n# Agent Skills\n\n| a | b |\n";
        let meta = parse_frontmatter(content).expect("frontmatter present");
        assert_eq!(meta.name, "Agent skills");
        assert_eq!(meta.description.as_deref(), Some("nav"));
    }

    #[test]
    fn parse_frontmatter_returns_none_without_frontmatter() {
        assert!(parse_frontmatter("# just a heading\nbody").is_none());
        assert!(parse_frontmatter("").is_none());
    }

    #[test]
    fn parse_frontmatter_description_optional() {
        let meta = parse_frontmatter("---\nname: minimal\n---\n").expect("frontmatter present");
        assert_eq!(meta.name, "minimal");
        assert!(meta.description.is_none());
    }

    #[test]
    fn parse_frontmatter_description_extracts_without_name() {
        // A category README may carry only `description` (no `name`) — the
        // directory name is the identifier. This extractor must not require name.
        let content = "---\ndescription: category of skills\n---\n# Heading\n";
        assert_eq!(
            parse_frontmatter_description(content).as_deref(),
            Some("category of skills")
        );
        // No frontmatter at all.
        assert!(parse_frontmatter_description("just prose").is_none());
        // Frontmatter but no description.
        assert!(parse_frontmatter_description("---\nname: x\n---\n").is_none());
    }
}
