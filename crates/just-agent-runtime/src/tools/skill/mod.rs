//! Minimal skill loading for just-agent.
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
//! directory or an agent-local directory. The [`FilePinTool`] LLM tool
//! exposes a general-purpose "read file and pin" operation to the agent.

pub mod promote;
pub use promote::promote_skill_from_content;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use just_llm_client::types::chat::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;
use just_agent_common::protocol::SkillMeta;

pub const META_SKILL_NAME: &str = "bootstrap";

const DEFAULT_META_SKILL: &str = r#"---
name: bootstrap
description: Skill system usage and behavioral guidelines
---

# Skill System

You have access to a skill system for loading reference material and best practices.

## Discovering Skills

- `just-agent skill paths` — shows shared and local skill directory paths
- `just-agent skill meta <name>` — shows a skill's name and description
- List available skills with `ls` on the returned paths

## Loading Skills

Use `read_file_and_pin` to load a file into persistent context. Skill files
are at `<skill_dir>/<name>.md`. Use the label `skill:<name>` for skills.
Use `context_unpin` to remove pinned items.

## Creating Skills

Use `just-agent skill paths` to find your local directory, then write
`<name>.md` there with YAML frontmatter. Only write to your local
directory — the shared directory is managed by operators.

    ---
    name: my-skill
    description: Short description
    ---

    # Skill content here

Keep skills concise — capture key decisions, pitfalls, and effective patterns.

# Behavioral Guidelines

## Gather Information

When facing something unfamiliar, read broadly before acting. Check
documentation, search for existing patterns, examine similar code in the
project. Don't rush to the first solution.

## Verify Carefully

Test assumptions before committing. Run small experiments to confirm your
understanding matches reality. Prefer incremental validation over bold leaps.

## Ask for Help

When genuinely uncertain, ask the user rather than guessing. A short
question is better than a long wrong path.
"#;

/// Returns the shared skill directory.
///
/// Checks `JUST_AGENT_SKILLS_ROOT` first (used as-is, no suffix), then
/// `JUST_AGENT_DATA_DIR` (appends `just-agent/skills/`), then falls back
/// to the platform data directory (`~/.local/share/just-agent/skills/`).
pub fn skill_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("JUST_AGENT_SKILLS_ROOT")
        && !dir.is_empty()
    {
        return std::path::PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("JUST_AGENT_DATA_DIR") {
        return std::path::PathBuf::from(dir)
            .join("just-agent")
            .join("skills");
    }
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("just-agent")
        .join("skills")
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
    let path = skill_dir().join(format!("{name}.md"));
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

/// Returns the built-in meta-skill content (skill system usage and behavioral guidelines).
///
/// The meta-skill is compiled into the binary and never written to disk.
/// It is appended to the system prompt at agent spawn time.
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

// --- read_file_and_pin ---

#[derive(Debug, Deserialize, Serialize)]
struct FilePinArgs {
    /// File path. Relative paths resolve against the workspace root.
    path: String,
    /// Label for the pinned item.
    label: String,
}

/// Reads a file from disk and pins its content into the agent's context.
///
/// Strips YAML frontmatter if present. Relative paths resolve against the
/// workspace root (current working directory). This is a general-purpose
/// shortcut for the common pattern of reading a file and pinning it for
/// cross-turn reference.
pub struct FilePinTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl FilePinTool {
    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LlmTool for FilePinTool {
    fn name(&self) -> &str {
        "read_file_and_pin"
    }

    fn description(&self) -> &str {
        "Read a file from disk and pin its content into context. \
         Strips YAML frontmatter if present. Use this to load reference \
         material, skills, or any content that should persist across \
         conversation turns."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path. Relative paths resolve against the workspace root."
                },
                "label": {
                    "type": "string",
                    "description": "Label for the pinned item. Use 'skill:<name>' convention for skills."
                }
            },
            "required": ["path", "label"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: FilePinArgs =
            serde_json::from_str(args_json).context("read_file_and_pin: invalid arguments")?;

        let path = std::path::Path::new(&args.path);
        let resolved = if path.is_relative() {
            std::env::current_dir()
                .context("failed to determine working directory")?
                .join(path)
        } else {
            path.to_path_buf()
        };

        let content = std::fs::read_to_string(&resolved)
            .with_context(|| format!("failed to read file '{}'", resolved.display()))?;
        let body = strip_frontmatter(&content).trim().to_owned();

        let message = ChatMessage::user(body);
        let mut ctx = self.ctx.lock().await;
        ctx.pin(&args.label, message)?;
        let labels = ctx.pinned_labels();
        Ok(serde_json::to_string(&json!({
            "pinned": args.label,
            "source": resolved.display().to_string(),
            "pinned_labels": labels,
        }))?)
    }
}

/// Creates the file-pin tool set.
pub fn file_pin_tool_set(ctx: Arc<Mutex<dyn AgenticContext>>) -> Vec<Box<dyn LlmTool>> {
    vec![Box::new(FilePinTool::new(ctx))]
}

#[cfg(test)]
mod tests {
    use super::*;

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
