//! Minimal skill loading for just-agent.
//!
//! Skills are suggestions, never mandatory instructions. They enter the
//! agent's context through the pinned layer of the context store.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use just_llm_client::types::chat::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;

const META_SKILL_NAME: &str = "novel-task";

const DEFAULT_META_SKILL: &str = r#"---
name: novel-task
description: How to approach unfamiliar tasks with care
---

# Approaching Unfamiliar Tasks

This skill guides you when encountering something new or uncertain.

## Gather Information

When facing something unfamiliar, read broadly before acting. Check
documentation, search for existing patterns, examine similar code in the
project. Don't rush to the first solution — understanding the landscape
first leads to better outcomes.

## Verify Carefully

Test assumptions before committing to them. Run small experiments to
confirm your understanding matches reality. Prefer incremental validation
over bold leaps. If a command might have destructive effects, check what
it would do before running it.

## Ask for Help

When genuinely uncertain, ask the user rather than guessing. A short
question is better than a long wrong path. Acknowledge what you don't
know — honesty about uncertainty builds trust.

## Distill Experience

After successfully navigating unfamiliar territory, reflect on what
worked. If this is a task type likely to recur, consider whether the
experience is worth capturing as a new skill. A good skill captures the
key decisions, pitfalls, and effective patterns — not a blow-by-blow
transcript.

To create a skill, write a `SKILL.md` file under the skills directory
with a YAML frontmatter header (name, description) followed by markdown
instructions. Keep it concise and focused on what you wish you had known
before starting.
"#;

/// Returns the skill directory.
///
/// Uses `JUST_AGENT_DATA_DIR` env var if set, otherwise falls back to
/// the platform data directory (`~/.local/share/just-agent/skills/`).
pub fn skill_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("JUST_AGENT_DATA_DIR") {
        return std::path::PathBuf::from(dir).join("skills");
    }
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("just-agent")
        .join("skills")
}

/// Reads a skill file, strips frontmatter, and returns the body.
pub fn load_skill(name: &str) -> Result<String> {
    if name.contains('/') || name.contains('\\') || name == ".." {
        bail!("invalid skill name: {name}");
    }
    let path = skill_dir().join(name).join("SKILL.md");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read skill '{name}' from {}", path.display()))?;
    Ok(strip_frontmatter(&content).trim().to_owned())
}

/// Ensures the meta-skill exists on disk and returns its content.
///
/// Creates the file from the built-in default if it doesn't exist yet.
/// The user can edit it afterward — it won't be overwritten.
pub fn ensure_meta_skill() -> Result<String> {
    let dir = skill_dir().join(META_SKILL_NAME);
    let path = dir.join("SKILL.md");

    if !path.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create skill directory {}", dir.display()))?;
        std::fs::write(&path, DEFAULT_META_SKILL)
            .with_context(|| format!("failed to write meta-skill to {}", path.display()))?;
    }

    load_skill(META_SKILL_NAME)
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

// --- skill_load ---

#[derive(Debug, Deserialize, Serialize)]
struct SkillLoadArgs {
    name: String,
}

/// Reads a skill from disk and pins it into the agent's context.
pub struct SkillLoadTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl SkillLoadTool {
    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LlmTool for SkillLoadTool {
    fn name(&self) -> &str {
        "skill_load"
    }

    fn description(&self) -> &str {
        "Load a skill from the skills directory and pin it into \
         the agent's context. Skills are suggestions and best practices, not \
         mandatory instructions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill name (directory under the skills directory)."
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: SkillLoadArgs =
            serde_json::from_str(args_json).context("skill_load: invalid arguments")?;
        let content = load_skill(&args.name)?;
        let label = format!("skill:{}", args.name);
        let message = ChatMessage::user(format!("[skill: {}]\n{content}", args.name));

        let mut ctx = self.ctx.lock().await;
        ctx.pin(&label, message)?;
        let labels = ctx.pinned_labels();
        Ok(serde_json::to_string(&json!({
            "loaded": args.name,
            "pinned_labels": labels,
        }))?)
    }
}

/// Creates the skill-loading tool.
pub fn skill_tool_set(ctx: Arc<Mutex<dyn AgenticContext>>) -> Vec<Box<dyn LlmTool>> {
    vec![Box::new(SkillLoadTool::new(ctx))]
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
    fn reject_path_traversal() {
        assert!(load_skill("../etc/passwd").is_err());
        assert!(load_skill("foo/bar").is_err());
    }
}
