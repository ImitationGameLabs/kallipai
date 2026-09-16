//! `skill` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum SkillCommand {
    /// Generate the skill index for a directory from each file's frontmatter
    ///
    /// Reads the directory at `path` directly and prints a markdown bullet
    /// index of its entries: each `.md` skill (from its frontmatter) and each
    /// subdirectory (from its `README.md` frontmatter), with each category's
    /// children inlined one level deep. The agent passes the `skills path`
    /// from its identity facts, then pins this output.
    Index(SkillIndexArgs),
    /// Show metadata for a specific skill
    Meta(SkillMetaArgs),
}

#[derive(Args)]
pub(crate) struct SkillIndexArgs {
    /// Absolute path of the skill directory to index.
    pub path: PathBuf,
    /// Number of levels to render (default 2). `1` gives a flat one-level
    /// view; raise it for a small subtree to fetch more in one batch. Clamped
    /// to `[1, MAX_INDEX_DEPTH]` by the renderer.
    #[arg(long, default_value_t = 2)]
    pub depth: u32,
}

#[derive(Args)]
pub(crate) struct SkillMetaArgs {
    /// Path to the skill — the stem (`<skills>/agent/kallip`) or the full
    /// `<skills>/agent/kallip.md`. Read directly from the filesystem.
    pub path: PathBuf,
}
