//! Minimal skill loading for kallip.
//!
//! Skills are suggestions, never mandatory instructions. They enter the
//! agent's context through the pinned layer of the context store.
//!
//! ## Skill identity
//!
//! A skill is uniquely identified by its **path relative to the skills root**
//! (e.g. `code/refactoring`). This path determines the on-disk layout
//! (`<skills_root>/<path>.md`) and is used for all lookups and routing.
//! The `name` field in YAML frontmatter is a display label — it is returned
//! by the metadata endpoint but is **not** used as an identifier and is not
//! required to match the path.
//!
//! The [`load_skill`] function resolves skill files from the shared skill
//! directory or an agent-local directory.

use std::path::{Path, PathBuf};

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

# Two control primitives you cannot do without

You run continuously: a plain response with no tool call does **not** end your
turn — the harness re-prompts you. To end the current run and park until the
next input arrives, call the `break` tool (call it last in a round). To address
the user — deliver a message they will see — run
`kallip lesche send "<text>"` (or pipe to its stdin) via `bash_exec`; the
`kallip` command is auto-allowed and needs no approval. So a finished turn that
delivers a message and yields is: `kallip lesche send`, then `break`. You may
also do work and `break` without sending anything, or send a message and keep
working — the two are independent. Sending is not only for responses: use it
whenever you need to reach the user (a proactive heads-up, a partial result, a
question), not just to answer.
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

/// Seed the shared skill directory with bundled defaults on first boot.
///
/// Reads the read-only seed tree from `KALLIP_SKILLS_SEED` and copies it into
/// [`skill_dir()`] when (and only when) that directory is empty. `skill_dir()`
/// honors `KALLIP_SKILLS_ROOT` as a **path override** (not an opt-out): seeding
/// targets whatever it resolves to, so an empty `KALLIP_SKILLS_ROOT` dir is
/// seeded just like an empty `<data_dir>/skills/`. No-op when:
///   - `KALLIP_SKILLS_SEED` is unset — no bundled tree (bare `cargo run`);
///   - the target is already non-empty — prior seed or agent-authored content
///     wins (never clobber), regardless of how the target path was chosen.
///
/// An empty seed tree is refused loudly (see [`seed_into`]).
///
/// Seeding is atomic and retryable: the seed is copied into a staging sibling
/// and renamed entry-by-entry into the empty target, so a mid-copy failure
/// leaves the target empty for the next boot to retry. Failures are returned
/// to the caller, which logs and continues — skills are optional context, not
/// a boot prerequisite.
pub fn seed_skills_if_empty() -> Result<()> {
    let Some(seed) = seed_dir() else {
        return Ok(());
    };
    let target = skill_dir()?;
    if !dir_is_empty(&target)? {
        return Ok(());
    }
    seed_into(&seed, &target)?;
    tracing::info!("seeded shared skills from {}", seed.display());
    Ok(())
}

/// Location of the bundled read-only skill tree, if configured.
///
/// `KALLIP_SKILLS_SEED` points at a store path (e.g. the `shared-skills` flake
/// output's `${out}/share/kallip/skills`). Unset or empty → `None`.
fn seed_dir() -> Option<PathBuf> {
    let s = std::env::var("KALLIP_SKILLS_SEED").ok()?;
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Whether the directory has no entries. The caller guarantees it exists.
fn dir_is_empty(p: &Path) -> Result<bool> {
    Ok(std::fs::read_dir(p)?.next().is_none())
}

/// Copy `seed` into `target` via a staging dir + atomic per-entry rename.
///
/// Staging lives as a sibling of `target` (under the same data dir) so the
/// final renames stay on the same filesystem and are atomic. On any error the
/// staging dir is removed, leaving `target` untouched; the next boot observes
/// an empty target and retries. The staging dir has a fixed name (`.skills-seed`)
/// rather than a pid-suffixed one: one tagma per data dir is the operational
/// invariant, and `try_copy_seed` clears any stale staging left by a prior
/// crashed boot before reusing the name.
fn seed_into(seed: &Path, target: &Path) -> Result<()> {
    // Refuse an empty seed tree: copy_dir_all would "succeed" having copied
    // nothing, target would stay empty, and every subsequent boot would re-log
    // a spurious success — the operator would get no signal that the seed is
    // broken (e.g. a future shared-skills.nix regression shipping an empty
    // store path).
    if dir_is_empty(seed)? {
        bail!(
            "skill seed dir {} is empty; refusing to seed a no-op tree",
            seed.display()
        );
    }

    let parent = target.parent().context("skill dir has no parent")?;
    let staging = parent.join(".skills-seed");

    if let Err(e) = try_copy_seed(seed, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // Move each top-level staging entry into the (empty) target. rename is
    // atomic per entry; the target's parent is writable (it is the data dir).
    for entry in std::fs::read_dir(&staging)? {
        let entry = entry?;
        let dst = target.join(entry.file_name());
        std::fs::rename(entry.path(), &dst)
            .with_context(|| format!("rename staged skill into {}", target.display()))?;
    }
    let _ = std::fs::remove_dir_all(&staging); // empty now
    Ok(())
}

/// Copy the seed tree into `staging`, clearing any stale staging dir first.
fn try_copy_seed(seed: &Path, staging: &Path) -> Result<()> {
    if staging.exists() {
        std::fs::remove_dir_all(staging)?;
    }
    crate::persistence::copy_dir_all(seed, staging)
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

/// Validates a skill name for path traversal attacks and name collisions.
///
/// Allows `/` for nested categories (e.g. `code/refactoring`) but rejects
/// `..` components, backslashes, and empty components. Also rejects the
/// reserved [`META_SKILL_NAME`] (`bootstrap`): the meta-skill is compiled in
/// and injected into the system prompt at spawn, so a disk file under that
/// name must never shadow it via the read paths (`load_skill`,
/// [`skill_metadata`]). The root agent authors shared skills directly via
/// `bash_exec`, so this guard lives on the read side rather than the writer.
pub fn validate_skill_name(name: &str) -> Result<()> {
    if name == META_SKILL_NAME {
        bail!("skill name '{name}' is reserved (compiled-in meta-skill)");
    }
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
    use serial_test::serial;
    use tempfile::TempDir;

    /// Run `f` with the three skill-related env vars pinned: `KALLIP_DATA_DIR`
    /// to `data_dir`, `KALLIP_SKILLS_SEED` to `seed` (None unsets), and
    /// `KALLIP_SKILLS_ROOT` to `root` (None unsets). Pinned together so the
    /// process-global state stays consistent.
    fn with_skill_env<R>(
        data_dir: &Path,
        seed: Option<&str>,
        root: Option<&str>,
        f: impl FnOnce() -> R,
    ) -> R {
        temp_env::with_vars(
            [
                ("KALLIP_DATA_DIR", Some(data_dir.to_str().unwrap())),
                ("KALLIP_SKILLS_SEED", seed),
                ("KALLIP_SKILLS_ROOT", root),
            ],
            f,
        )
    }

    /// Build a minimal seed fixture (mirrors the shipped layout: an index plus
    /// a nested category). Returns the tempdir holding it.
    fn build_seed_fixture() -> TempDir {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("code")).unwrap();
        let index = tmp.path().join("index.md");
        std::fs::write(&index, "# index\n").unwrap();
        // Read-only, like a nix-store source file — exercises the mode-preserving
        // contract of fs::copy (the seeded defaults must not become writable).
        std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::write(tmp.path().join("code/aifed.md"), "aifed skill\n").unwrap();
        tmp
    }

    #[test]
    #[serial]
    fn seed_dir_reads_env() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        // set
        with_skill_env(tmp.path(), Some(&path), None, || {
            assert_eq!(seed_dir().unwrap(), PathBuf::from(&path));
        });
        // unset
        with_skill_env(tmp.path(), None, None, || {
            assert!(seed_dir().is_none());
        });
        // empty string -> None
        with_skill_env(tmp.path(), Some(""), None, || {
            assert!(seed_dir().is_none());
        });
    }

    #[test]
    #[serial]
    fn seed_copies_tree_when_target_empty() {
        let data = TempDir::new().unwrap();
        let seed = build_seed_fixture();
        // The boot path creates the skills dir before seeding; mirror that.
        std::fs::create_dir_all(data.path().join("skills")).unwrap();
        let seed_path = seed.path().to_str().unwrap().to_owned();

        with_skill_env(data.path(), Some(&seed_path), None, || {
            seed_skills_if_empty().unwrap();
        });

        let skills = data.path().join("skills");
        assert!(skills.join("index.md").exists(), "top-level index seeded");
        assert!(skills.join("code/aifed.md").exists(), "nested skill seeded");
        // Read-only mode preserved (0444 from the fixture).
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(skills.join("index.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o444, "seeded shipped skills stay read-only");
    }

    #[test]
    #[serial]
    fn seed_is_noop_when_target_nonempty() {
        let data = TempDir::new().unwrap();
        let seed = build_seed_fixture();
        let skills = data.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        // Pre-existing agent-authored file: seeding must not clobber it.
        std::fs::write(skills.join("mine.md"), "authored\n").unwrap();
        let seed_path = seed.path().to_str().unwrap().to_owned();

        with_skill_env(data.path(), Some(&seed_path), None, || {
            seed_skills_if_empty().unwrap();
        });

        // Authored file untouched; no seeded files appeared.
        assert_eq!(
            std::fs::read_to_string(skills.join("mine.md")).unwrap(),
            "authored\n"
        );
        assert!(!skills.join("index.md").exists());
    }

    #[test]
    #[serial]
    fn seed_targets_skills_root_dir_when_empty() {
        // KALLIP_SKILLS_ROOT only relocates the skill dir; it is NOT an opt-out.
        // An empty ROOT dir is seeded just like an empty <data_dir>/skills —
        // the operator asked for a different location, not for "leave it empty".
        let data = TempDir::new().unwrap();
        let seed = build_seed_fixture();
        let root = data.path().join("managed-skills");
        std::fs::create_dir_all(&root).unwrap();
        let seed_path = seed.path().to_str().unwrap().to_owned();
        let root_path = root.to_str().unwrap().to_owned();

        with_skill_env(data.path(), Some(&seed_path), Some(&root_path), || {
            seed_skills_if_empty().unwrap();
        });

        assert!(root.join("index.md").exists(), "ROOT dir was seeded");
        // The default <data_dir>/skills was NOT the target, so it stayed empty/absent.
        assert!(
            !data.path().join("skills/index.md").exists(),
            "seed wrote to the default path instead of ROOT"
        );
        // skill_dir() honors ROOT — that is how the seed found the target.
        with_skill_env(data.path(), Some(&seed_path), Some(&root_path), || {
            assert_eq!(skill_dir().unwrap(), root);
        });
    }

    #[test]
    #[serial]
    fn seed_skips_nonempty_skills_root_dir() {
        // The never-clobber rule holds regardless of how the target path was
        // chosen: a non-empty KALLIP_SKILLS_ROOT dir is left alone.
        let data = TempDir::new().unwrap();
        let seed = build_seed_fixture();
        let root = data.path().join("managed-skills");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("mine.md"), "authored\n").unwrap();
        let seed_path = seed.path().to_str().unwrap().to_owned();
        let root_path = root.to_str().unwrap().to_owned();

        with_skill_env(data.path(), Some(&seed_path), Some(&root_path), || {
            seed_skills_if_empty().unwrap();
        });

        assert!(root.join("mine.md").exists(), "authored file preserved");
        assert!(
            !root.join("index.md").exists(),
            "non-empty ROOT was not seeded"
        );
    }

    #[test]
    #[serial]
    fn seed_skips_when_seed_unset() {
        let data = TempDir::new().unwrap();
        std::fs::create_dir_all(data.path().join("skills")).unwrap();

        with_skill_env(data.path(), None, None, || {
            seed_skills_if_empty().unwrap();
        });

        assert!(!data.path().join("skills/index.md").exists());
    }

    #[test]
    #[serial]
    fn seed_is_noop_with_root_but_no_seed() {
        // ROOT alone must NOT cause skipping — only an absent seed or a
        // non-empty target does. Pins the precise rule: with ROOT set and
        // KALLIP_SKILLS_SEED unset, seeding is a no-op for the no-seed reason
        // (not because ROOT was set). A regression that re-introduces a
        // ROOT short-circuit would still pass other tests but fail to
        // distinguish the two skip reasons.
        let data = TempDir::new().unwrap();
        let root = data.path().join("managed-skills");
        std::fs::create_dir_all(&root).unwrap();
        let root_path = root.to_str().unwrap().to_owned();

        with_skill_env(data.path(), None, Some(&root_path), || {
            seed_skills_if_empty().unwrap();
        });

        assert!(
            !root.join("index.md").exists(),
            "no seed configured -> no seed written"
        );
    }

    #[test]
    #[serial]
    fn seed_failure_leaves_target_empty_and_no_staging() {
        let data = TempDir::new().unwrap();
        std::fs::create_dir_all(data.path().join("skills")).unwrap();
        // Point at a path that does not exist -> copy fails.
        let bad = data
            .path()
            .join("does-not-exist")
            .to_str()
            .unwrap()
            .to_owned();

        with_skill_env(data.path(), Some(&bad), None, || {
            // Failure is returned (the boot caller logs+continues).
            assert!(seed_skills_if_empty().is_err());
        });

        let skills = data.path().join("skills");
        // Target stayed empty.
        assert!(std::fs::read_dir(&skills).unwrap().next().is_none());
        // No staging artifact left behind (retryable on the next boot).
        assert!(
            !std::fs::read_dir(data.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().starts_with(".skills-seed")),
            "staging dir leaked after failed seed"
        );
    }

    #[test]
    #[serial]
    fn seed_fails_when_seed_dir_is_empty() {
        // An empty seed tree is a broken seed (e.g. a future shared-skills.nix
        // regression). Refuse it loudly rather than logging a spurious success
        // and leaving the target empty every boot.
        let data = TempDir::new().unwrap();
        let empty_seed = TempDir::new().unwrap();
        std::fs::create_dir_all(data.path().join("skills")).unwrap();
        let seed_path = empty_seed.path().to_str().unwrap().to_owned();

        with_skill_env(data.path(), Some(&seed_path), None, || {
            assert!(seed_skills_if_empty().is_err());
        });

        assert!(
            std::fs::read_dir(data.path().join("skills"))
                .unwrap()
                .next()
                .is_none(),
            "target populated despite empty seed"
        );
    }

    #[test]
    #[serial]
    fn seed_clears_stale_staging_from_prior_crash() {
        // A prior boot that crashed mid-copy leaves a stale `.skills-seed`. The
        // next boot must reclaim it (fixed staging name) and complete cleanly.
        let data = TempDir::new().unwrap();
        let seed = build_seed_fixture();
        std::fs::create_dir_all(data.path().join("skills")).unwrap();
        // Simulate a crashed prior boot: stale staging with garbage inside.
        let stale = data.path().join(".skills-seed");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("leftover.md"), "garbage\n").unwrap();
        let seed_path = seed.path().to_str().unwrap().to_owned();

        with_skill_env(data.path(), Some(&seed_path), None, || {
            seed_skills_if_empty().unwrap();
        });

        let skills = data.path().join("skills");
        assert!(
            skills.join("index.md").exists(),
            "seed completed despite stale staging"
        );
        assert!(!stale.exists(), "stale staging dir was not reclaimed");
        assert!(
            !skills.join("leftover.md").exists(),
            "stale garbage leaked into target"
        );
    }

    #[test]
    fn bootstrap_surfaces_discovery_and_stance() {
        // The compiled meta-skill (appended to every agent's prompt at spawn,
        // routes/agent.rs) is the ONLY guaranteed surface an agent sees before
        // it discovers anything. It is kept deliberately thin: a universal
        // judgment stance, a discovery pointer, and the two control primitives
        // the agent cannot behave correctly without (it would loop forever
        // without `break`, and could never address the user without
        // `kallip lesche send`). All other operations live in the skill files this
        // test also pins down below.
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

        // --- Positive: the two non-negotiable control primitives ---
        assert!(
            DEFAULT_META_SKILL.contains("break"),
            "floor must name the `break` yield primitive: {DEFAULT_META_SKILL}"
        );
        assert!(
            DEFAULT_META_SKILL.contains("kallip lesche send"),
            "floor must name the `kallip lesche send` user-address primitive: {DEFAULT_META_SKILL}"
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

        // The operations the floor no longer teaches (sharing, unpin, evict)
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
    fn validate_skill_name_rejects_reserved_bootstrap() {
        // The meta-skill is compiled in; a disk file under this name must never
        // shadow it via the read paths.
        assert!(validate_skill_name(META_SKILL_NAME).is_err());
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
