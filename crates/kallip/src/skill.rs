//! `kallip skill index` / `skill meta`: render a depth-bounded, navigable
//! bullet index of a skills directory, and read a single skill's frontmatter.

use std::path::Path;

use anyhow::{Context, Result};

use kallip_common::protocol::{SkillMeta, parse_frontmatter, parse_frontmatter_description};

/// Upper bound on `--depth`. Bounds pathological cost and cyclic-symlink
/// recursion — the collector's depth budget itself guarantees termination, and
/// this cap keeps a mistyped large value affordable.
const MAX_INDEX_DEPTH: u32 = 10;

/// One node in the collected skill tree.
enum Entry {
    /// A leaf `.md` skill. `name` is the file stem — the navigable identifier
    /// used by `kallip skill meta` and file reads (frontmatter `name` is only a
    /// display label, never shown).
    Skill {
        name: String,
        description: Option<String>,
    },
    /// A subdirectory (category). `name` is the directory name. `children` is
    /// populated iff the depth budget allowed expanding this category
    /// (`expanded`); otherwise the renderer recomputes the child count via a
    /// count-only readdir and offers the category as a drill-in target.
    Category {
        name: String,
        description: Option<String>,
        children: Vec<Entry>,
        expanded: bool,
    },
}

/// Classified kind of a single directory entry, shared by the recursive
/// collector and the count-only path so their skip rules cannot drift.
enum ChildKind {
    Skill,
    Category,
}

/// Classify one `read_dir` entry. Returns `None` for everything the index
/// skips: dotfiles/dot-dirs, non-`.md` files, the dir's own `README.md`, and
/// stale `index.md`. `meta` must come from `fs::metadata` (follows symlinks),
/// not `DirEntry::file_type`.
fn classify(file_name: &str, meta: &std::fs::Metadata) -> Option<ChildKind> {
    if file_name.starts_with('.') {
        return None;
    }
    if meta.is_dir() {
        Some(ChildKind::Category)
    } else if meta.is_file() && has_md_extension(file_name) {
        if file_name == "README.md" || file_name == "index.md" {
            None
        } else {
            Some(ChildKind::Skill)
        }
    } else {
        None
    }
}

/// Sort a level's entries: categories before skills, each alphabetical
/// (case-insensitive) — the navigable top-down ordering, applied at every depth.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        let a_cat = matches!(a, Entry::Category { .. });
        let b_cat = matches!(b, Entry::Category { .. });
        b_cat.cmp(&a_cat).then_with(|| {
            entry_name(a)
                .to_lowercase()
                .cmp(&entry_name(b).to_lowercase())
        })
    });
}

/// The navigable name of an entry (file stem or directory name).
fn entry_name(entry: &Entry) -> &str {
    match entry {
        Entry::Skill { name, .. } => name,
        Entry::Category { name, .. } => name,
    }
}

/// Format the parenthetical child-count label for a category, split honestly
/// by type so the agent can tell skills from subdirectories at a glance:
/// `(N skills)`, `(M subcategories)`, `(N skills, M subcategories)`, `(empty)`.
fn format_count(skills: usize, subcategories: usize) -> String {
    let skill_token = |n: usize| if n == 1 { "skill" } else { "skills" };
    let subcat_token = |m: usize| {
        if m == 1 {
            "subcategory"
        } else {
            "subcategories"
        }
    };
    match (skills, subcategories) {
        (0, 0) => "(empty)".to_owned(),
        (n, 0) => format!("({n} {})", skill_token(n)),
        (0, m) => format!("({m} {})", subcat_token(m)),
        (n, m) => format!("({n} {}, {m} {})", skill_token(n), subcat_token(m)),
    }
}

/// Build the ` — <desc>` suffix for an index line: ` — <desc>` (trailing
/// whitespace trimmed) when a description is present, empty otherwise. Shared
/// by skill and category lines so the "missing description = clean bullet"
/// contract lives in one place.
fn desc_suffix(description: Option<&str>) -> String {
    description
        .map(|d| format!(" — {}", d.trim_end()))
        .unwrap_or_default()
}

/// Count `(skills, subcategories)` directly under `dir` without building
/// entries. Applies [`classify`]'s skip rules and, like [`collect_entries`],
/// skips entries whose metadata can't be read (dangling symlink, etc.); an
/// unreadable `dir` reads as `(0, 0)`. The result matches what a drill-in
/// [`collect_entries`] on the same dir would classify.
fn count_children(dir: &Path) -> (usize, usize) {
    let mut skills = 0usize;
    let mut subcategories = 0usize;
    let Ok(readdir) = std::fs::read_dir(dir) else {
        return (0, 0); // unreadable dir reads as empty — match collect_entries' tolerance
    };
    for entry in readdir.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        match classify(&file_name, &meta) {
            Some(ChildKind::Skill) => skills += 1,
            Some(ChildKind::Category) => subcategories += 1,
            None => continue,
        }
    }
    (skills, subcategories)
}

/// Recursively collect entries under `dir` with a depth `budget` (levels to
/// expand below `dir`). A `budget` of 0 yields only `dir`'s direct children
/// with every category unexpanded; each extra level expands one more tier.
///
/// The top-level `read_dir` error propagates (the agent passed a bad path); an
/// unreadable *nested* subdir is tolerated as an empty category so one bad dir
/// never bricks the listing. Per-entry read failures (dangling symlink,
/// unreadable file/README) are skipped uniformly.
fn collect_entries(dir: &Path, budget: u32) -> Result<Vec<Entry>> {
    let mut out: Vec<Entry> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue, // dangling symlink / unreadable — skip
        };
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        match classify(&file_name, &meta) {
            None => continue,
            Some(ChildKind::Category) => {
                let description = std::fs::read_to_string(path.join("README.md"))
                    .ok()
                    .and_then(|c| parse_frontmatter_description(&c));
                let (children, expanded) = if budget > 0 {
                    // Tolerate an unreadable nested subdir as an empty category.
                    (collect_entries(&path, budget - 1).unwrap_or_default(), true)
                } else {
                    (Vec::new(), false)
                };
                out.push(Entry::Category {
                    name: file_name.into_owned(),
                    description,
                    children,
                    expanded,
                });
            }
            Some(ChildKind::Skill) => {
                let content = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("skill")
                    .to_owned();
                let description = parse_frontmatter(&content).and_then(|m| m.description);
                out.push(Entry::Skill { name, description });
            }
        }
    }
    sort_entries(&mut out);
    Ok(out)
}

/// Count `(skills, subcategories)` from an already-collected children slice —
/// free, no I/O (entries are already classified).
fn count_from_children(children: &[Entry]) -> (usize, usize) {
    let skills = children
        .iter()
        .filter(|e| matches!(e, Entry::Skill { .. }))
        .count();
    let subcategories = children
        .iter()
        .filter(|e| matches!(e, Entry::Category { .. }))
        .count();
    (skills, subcategories)
}

/// Render `entries` as a nested bullet list into `out`.
///
/// `dir_path` must be the parent directory the entries were collected from;
/// each category's child dir is resolved as `dir_path.join(name)` for count-only
/// reads. `unexpanded` is incremented once per category rendered at the deepest
/// level (those the budget did not expand), so the caller can decide whether to
/// emit the drill-in hint.
fn render_entries(
    entries: &[Entry],
    dir_path: &Path,
    level: usize,
    out: &mut String,
    unexpanded: &mut usize,
) {
    let indent = "  ".repeat(level);
    for entry in entries {
        match entry {
            Entry::Skill { name, description } => {
                let desc = desc_suffix(description.as_deref());
                out.push_str(&format!("{indent}- `{name}`{desc}\n"));
            }
            Entry::Category {
                name,
                description,
                children,
                expanded,
            } => {
                let child_dir = dir_path.join(name);
                let count = if *expanded {
                    let (s, c) = count_from_children(children);
                    format_count(s, c)
                } else {
                    *unexpanded += 1;
                    let (s, c) = count_children(&child_dir);
                    format_count(s, c)
                };
                let desc = desc_suffix(description.as_deref());
                out.push_str(&format!("{indent}- `{name}/` {count}{desc}\n"));
                if *expanded {
                    render_entries(children, &child_dir, level + 1, out, unexpanded);
                }
            }
        }
    }
}

/// Read one skill's metadata directly from its file. Accepts either the stem
/// (`<skills>/agent/kallip`) or the full `<skills>/agent/kallip.md`. Falls back
/// to the file stem for `name` when the file has no frontmatter, mirroring the
/// old server-side `skill_metadata` default.
pub fn read_skill_meta(path: &Path) -> Result<SkillMeta> {
    let file = match path.extension().and_then(|e| e.to_str()) {
        None => path.with_extension("md"),
        Some(ext) if ext.eq_ignore_ascii_case("md") => path.to_path_buf(),
        Some(other) => {
            anyhow::bail!(
                "skill path must be a stem or `.md`, got `.{other}`: {}",
                path.display()
            );
        }
    };
    let content = std::fs::read_to_string(&file)
        .with_context(|| format!("failed to read skill {}", file.display()))?;
    Ok(parse_frontmatter(&content).unwrap_or_else(|| {
        let name = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_owned();
        SkillMeta {
            name,
            description: None,
        }
    }))
}

/// Generate the skill index for `dir` as a nested bullet list, read directly
/// from the filesystem — optimized for LLM consumption and pinning (the agent
/// runs this once and pins the output).
///
/// `depth` is the number of levels to render (clamped to `[1, MAX_INDEX_DEPTH]`):
/// level 1 is `dir`'s direct children; each deeper level inlines one more tier
/// of category children. A category at the deepest rendered level is shown with
/// a count of its children but left unexpanded, and a trailing drill-in hint is
/// appended so the agent knows how to reveal the next level. Each `.md` file
/// (except `README.md`/`index.md`) is a skill from its frontmatter; each
/// subdirectory is a category described by its `README.md` frontmatter.
/// Categories sort before skills at every level, each alphabetical.
///
/// Unreadable entries are skipped uniformly (dangling symlink, unreadable
/// README, unreadable skill `.md`, unreadable nested subdir) so one bad file
/// never bricks the listing. `index.md` is explicitly skipped so stale copies
/// in already-deployed data dirs (the seed never clobbers a non-empty target)
/// stay invisible — they never pollute the generated index as bogus skills. A
/// missing top-level `dir` propagates as an error (the agent passed a bad path).
pub fn render_skill_index(dir: &Path, depth: u32) -> Result<String> {
    let budget = depth.clamp(1, MAX_INDEX_DEPTH) - 1;
    let entries = collect_entries(dir, budget)?;

    let mut out = format!("# Skill index for `{}`\n\n", dir.display());
    let mut unexpanded = 0usize;
    render_entries(&entries, dir, 0, &mut out, &mut unexpanded);
    if unexpanded > 0 {
        out.push('\n');
        out.push_str(
            "Some categories were not expanded here — run `kallip skill index <path>` on a \
             category (entries ending in `/`) to list the skills inside it.\n",
        );
    }
    Ok(out)
}

/// Case-insensitive `.md` extension check, so a skill named `kallip.MD` is
/// treated the same as `kallip.md` by both `skill index` and `skill meta`.
fn has_md_extension(name: &str) -> bool {
    name.len() >= 3 && name[name.len() - 3..].eq_ignore_ascii_case(".md")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a temp skill fixture: three top-level skills, a category subdir
    /// `agent/` holding two nested skills and a nested subcategory `sub/`, a
    /// stale `index.md`, and a symlink to a skill. `agent/` is thus mixed
    /// (2 skills + 1 subcategory), exercising the level-2 inline, the honest
    /// split count, and an unexpanded level-2 category that triggers the
    /// drill-in footer.
    fn build_fixture() -> tempfile::TempDir {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("alpha.md"),
            // frontmatter `name` deliberately differs from the file stem — the
            // index must show the stem (navigable), not the display label.
            "---\nname: Alpha Display\ndescription: first skill\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            root.join("beta.md"),
            "---\nname: beta\ndescription: second skill\n---\nbody\n",
        )
        .unwrap();
        // A skill with no frontmatter — name falls back to the file stem.
        fs::write(root.join("plain.md"), "no frontmatter\n").unwrap();

        let sub = root.join("agent");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("README.md"),
            "---\ndescription: Agent self-management\n---\n# Agent Skills\n",
        )
        .unwrap();
        // Two nested skills under agent/. `kallip`'s frontmatter `name` differs
        // from the stem to pin stem-not-display-name at level 2.
        fs::write(
            sub.join("kallip.md"),
            "---\nname: Kallip Display\ndescription: nested kallip skill\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            sub.join("zeta.md"),
            "---\nname: zeta\ndescription: nested zeta skill\n---\nbody\n",
        )
        .unwrap();
        // A nested subcategory under agent/ — unexpanded at depth 2, so it
        // surfaces as a counted drill-in target and fires the footer.
        let nested = sub.join("sub");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("README.md"),
            "---\ndescription: nested subcategory\n---\n# Sub\n",
        )
        .unwrap();

        // Stale deployed index.md — must NOT appear as a skill.
        fs::write(root.join("index.md"), "---\nname: Skill Index\n---\nold\n").unwrap();

        // Symlink to a skill — followed, lists as a skill.
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("alpha.md"), root.join("linked.md")).unwrap();

        dir
    }

    #[test]
    fn render_skill_index_lists_skills_and_categories() {
        let fixture = build_fixture();
        let out = render_skill_index(fixture.path(), 2).unwrap();

        // Self-describing H1 names the directory (the blob is pinned as-is).
        assert!(
            out.starts_with(&format!(
                "# Skill index for `{}`\n\n",
                fixture.path().display()
            )),
            "index must start with a self-describing H1: {out}"
        );

        // Category first (top-down): trailing `/`, honest split count of its
        // direct children (2 skills + 1 subcategory), then its README desc.
        let agent_pos = out
            .find("- `agent/` (2 skills, 1 subcategory) — Agent self-management\n")
            .expect("agent category line with split count: {out}");

        // agent's children are inlined one level deep (2-space indent), sorted
        // categories-before-skills: the nested subcategory `sub/` first as an
        // UNEXPANDED category with its own count, then the skills under stem.
        let sub_pos = out
            .find("  - `sub/` (empty) — nested subcategory\n")
            .expect("nested sub/ rendered unexpanded with its count: {out}");
        let kallip_pos = out
            .find("  - `kallip` — nested kallip skill\n")
            .expect("nested kallip skill under stem: {out}");
        let zeta_pos = out
            .find("  - `zeta` — nested zeta skill\n")
            .expect("nested zeta skill: {out}");
        assert!(
            sub_pos < kallip_pos,
            "subcategory sorts before skills: {out}"
        );
        assert!(kallip_pos < zeta_pos, "nested skills alphabetical: {out}");
        // depth 2 never renders a third level — no 4-space indent anywhere.
        assert!(
            !out.contains("\n    - "),
            "nothing expands past the rendered depth: {out}"
        );

        // Top-level skills appear under their FILE STEM (navigable), not the
        // frontmatter display name, with the description from frontmatter.
        let alpha_pos = out
            .find("- `alpha` — first skill\n")
            .expect("alpha skill line with description: {out}");
        assert!(
            agent_pos < alpha_pos,
            "category must sort before skills: {out}"
        );
        assert!(
            !out.contains("Alpha Display") && !out.contains("Kallip Display"),
            "frontmatter display name must not appear at any level: {out}"
        );
        assert!(out.contains("- `beta` — second skill\n"));
        // No-frontmatter skill: bullet with no trailing dash/description.
        assert!(
            out.contains("\n- `plain`\n"),
            "no-description skill must be stem-only: {out}"
        );

        // The unexpanded `sub/` fires the drill-in footer.
        assert!(
            out.contains("Some categories were not expanded here"),
            "footer must appear when an unexpanded category exists: {out}"
        );

        // Stale index.md and the dir's own README.md are NOT listed as skills.
        assert!(!out.contains("Skill Index"));
        assert!(!out.contains("README"));
    }

    #[test]
    fn read_skill_meta_accepts_stem_and_md_path() {
        let fixture = build_fixture();
        let alpha = fixture.path().join("alpha.md");
        // Stem form (no .md) and full .md form resolve to the same file.
        let by_stem = read_skill_meta(&alpha.with_extension("")).unwrap();
        let by_file = read_skill_meta(&alpha).unwrap();
        assert_eq!(by_stem.name, "Alpha Display");
        assert_eq!(by_stem.description.as_deref(), Some("first skill"));
        assert_eq!(by_stem.name, by_file.name);

        // A file with no frontmatter falls back to the file stem as `name`.
        let plain = read_skill_meta(&fixture.path().join("plain")).unwrap();
        assert_eq!(plain.name, "plain");
        assert!(plain.description.is_none());

        // A missing file errors.
        assert!(read_skill_meta(&fixture.path().join("nope")).is_err());
        // A non-`.md` extension is rejected with a clear error (not used as-is).
        assert!(read_skill_meta(&fixture.path().join("alpha.txt")).is_err());
    }

    #[test]
    fn has_md_extension_is_case_insensitive() {
        assert!(has_md_extension("kallip.md"));
        assert!(has_md_extension("kallip.MD"));
        assert!(has_md_extension("kallip.Md"));
        assert!(!has_md_extension("kallip.txt"));
        assert!(!has_md_extension("kallipmarkdown"));
        // An empty string is not a `.md` file.
        assert!(!has_md_extension(""));
    }

    /// The shipped category READMEs are load-bearing seed content for the
    /// generated index — a frontmatter typo would otherwise ship an empty
    /// category row silently. Pin that both parse to a non-empty description.
    #[test]
    fn shipped_category_readmes_parse_to_description() {
        use std::fs;

        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for category in ["agent", "code"] {
            let readme = repo_root.join("skills").join(category).join("README.md");
            let content = fs::read_to_string(&readme)
                .unwrap_or_else(|e| panic!("shipped {category}/README.md must exist: {e}"));
            assert!(
                parse_frontmatter_description(&content).is_some_and(|d| !d.trim().is_empty()),
                "shipped skills/{category}/README.md must carry a non-empty frontmatter description"
            );
        }
    }

    /// The parenthetical count is split honestly by type, with singular/plural
    /// forms — every arm of `format_count` covered by sibling categories.
    #[test]
    fn render_skill_index_count_label_honest_split() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let one = root.join("only_skills");
        fs::create_dir_all(&one).unwrap();
        fs::write(one.join("README.md"), "---\ndescription: one\n---\n").unwrap();
        fs::write(one.join("a.md"), "---\ndescription: a\n---\n").unwrap();

        let many = root.join("many_skills");
        fs::create_dir_all(&many).unwrap();
        fs::write(many.join("README.md"), "---\ndescription: many\n---\n").unwrap();
        fs::write(many.join("a.md"), "---\ndescription: a\n---\n").unwrap();
        fs::write(many.join("b.md"), "---\ndescription: b\n---\n").unwrap();

        let subs = root.join("only_subs");
        fs::create_dir_all(&subs).unwrap();
        fs::write(subs.join("README.md"), "---\ndescription: subs\n---\n").unwrap();
        fs::create_dir_all(subs.join("inner")).unwrap();

        let mixed = root.join("mixed");
        fs::create_dir_all(&mixed).unwrap();
        fs::write(mixed.join("README.md"), "---\ndescription: mixed\n---\n").unwrap();
        fs::write(mixed.join("a.md"), "---\ndescription: a\n---\n").unwrap();
        fs::create_dir_all(mixed.join("inner")).unwrap();

        let empty = root.join("empty");
        fs::create_dir_all(&empty).unwrap();
        fs::write(empty.join("README.md"), "---\ndescription: empty\n---\n").unwrap();

        let out = render_skill_index(root, 2).unwrap();
        assert!(out.contains("- `only_skills/` (1 skill) — one"));
        assert!(out.contains("- `many_skills/` (2 skills) — many"));
        assert!(out.contains("- `only_subs/` (1 subcategory) — subs"));
        assert!(out.contains("- `mixed/` (1 skill, 1 subcategory) — mixed"));
        assert!(out.contains("- `empty/` (empty) — empty"));
    }

    /// The repo `skills/` shape — categories of leaf skills only — produces no
    /// unexpanded category, so the drill-in footer is omitted (no noise).
    #[test]
    fn render_skill_index_footer_absent_when_no_unexpanded_categories() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cat = root.join("leaves");
        fs::create_dir_all(&cat).unwrap();
        fs::write(cat.join("README.md"), "---\ndescription: leaves\n---\n").unwrap();
        fs::write(cat.join("a.md"), "---\nname: a\ndescription: a\n---\n").unwrap();
        fs::write(cat.join("b.md"), "---\nname: b\ndescription: b\n---\n").unwrap();

        let out = render_skill_index(root, 2).unwrap();
        assert!(!out.contains("Some categories were not expanded"));
        // Sanity: the category DID expand (children inlined).
        assert!(out.contains("  - `a` — a"));
    }

    /// A minimal fixture isolating the footer trigger: one root category
    /// containing one subcategory (which is unexpanded at depth 2).
    #[test]
    fn render_skill_index_footer_present_for_unexpanded_level2() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cat = root.join("cat");
        fs::create_dir_all(cat.join("sub")).unwrap();
        fs::write(cat.join("README.md"), "---\ndescription: cat\n---\n").unwrap();

        let out = render_skill_index(root, 2).unwrap();
        assert!(out.contains("- `cat/` (1 subcategory) — cat"));
        assert!(out.contains("  - `sub/` (empty)"));
        assert!(out.contains("Some categories were not expanded here"));
    }

    /// `--depth` controls how many levels render. Depth 1 is the flat fallback
    /// (no inlining, every category unexpanded, footer present). Depth 3 on a
    /// 4-deep tree renders three levels and leaves the deepest unexpanded.
    #[test]
    fn render_skill_index_depth_flag_controls_levels() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // a/b/c/d.md  (depth 4 below root)
        fs::create_dir_all(root.join("a").join("b").join("c")).unwrap();
        fs::write(root.join("a/README.md"), "---\ndescription: a\n---\n").unwrap();
        fs::write(root.join("a/b/README.md"), "---\ndescription: b\n---\n").unwrap();
        fs::write(root.join("a/b/c/README.md"), "---\ndescription: c\n---\n").unwrap();
        fs::write(root.join("a/b/c/d.md"), "---\ndescription: d\n---\nbody\n").unwrap();

        // Depth 1: flat. `a/` is unexpanded (no `b/` inlined), footer present.
        let flat = render_skill_index(root, 1).unwrap();
        assert!(flat.contains("- `a/` (1 subcategory) — a"));
        assert!(!flat.contains("`b/`"));
        assert!(flat.contains("Some categories were not expanded here"));

        // Depth 3: a/ -> b/ -> c/ rendered, c/ unexpanded (d.md not inlined).
        let deep = render_skill_index(root, 3).unwrap();
        assert!(deep.contains("- `a/` (1 subcategory) — a"));
        assert!(deep.contains("  - `b/` (1 subcategory) — b"));
        assert!(deep.contains("    - `c/` (1 skill) — c"));
        assert!(!deep.contains("`d`")); // 4th level not reached
        assert!(deep.contains("Some categories were not expanded here"));

        // Oversize depth clamps to MAX_INDEX_DEPTH (no panic, still valid).
        let clamped = render_skill_index(root, 99).unwrap();
        assert!(clamped.starts_with("# Skill index for"));

        // depth 0 is rejected by the arg parser, but the renderer clamps it to
        // 1 rather than mis-rendering (defense for any direct caller).
        let zero = render_skill_index(root, 0).unwrap();
        assert!(zero.contains("- `a/` (1 subcategory) — a"));
        assert!(!zero.contains("`b/`"));
    }

    /// The 2-level window is self-similar: drilling into a deeper dir yields
    /// the next 2-level view. This is what halves the turns to reach deep skills.
    #[test]
    fn render_skill_index_deep_tree_self_similar() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // a/b/c/d/e.md
        fs::create_dir_all(root.join("a").join("b").join("c").join("d")).unwrap();
        fs::write(root.join("a/README.md"), "---\ndescription: a\n---\n").unwrap();
        fs::write(root.join("a/b/README.md"), "---\ndescription: b\n---\n").unwrap();
        fs::write(root.join("a/b/c/README.md"), "---\ndescription: c\n---\n").unwrap();
        fs::write(root.join("a/b/c/d/README.md"), "---\ndescription: d\n---\n").unwrap();
        fs::write(
            root.join("a/b/c/d/e.md"),
            "---\ndescription: e\n---\nbody\n",
        )
        .unwrap();

        // From root: a/ -> b/ (b unexpanded, footer present).
        let from_root = render_skill_index(root, 2).unwrap();
        assert!(from_root.contains("- `a/` (1 subcategory) — a"));
        assert!(from_root.contains("  - `b/` (1 subcategory) — b"));
        assert!(!from_root.contains("`c/`"));
        assert!(from_root.contains("Some categories were not expanded here"));

        // Drilling into a/b: c/ -> d/ (d unexpanded), same 2-level shape.
        let drilled = render_skill_index(&root.join("a").join("b"), 2).unwrap();
        assert!(drilled.contains("- `c/` (1 subcategory) — c"));
        assert!(drilled.contains("  - `d/` (1 skill) — d"));
        assert!(!drilled.contains("`e`"));
        assert!(drilled.contains("Some categories were not expanded here"));
    }

    /// A leaf skill never carries a parenthetical — guards against a future
    /// regression putting a count on a skill line.
    #[test]
    fn render_skill_index_skill_never_has_parenthetical() {
        let fixture = build_fixture();
        let out = render_skill_index(fixture.path(), 2).unwrap();
        for line in out.lines() {
            // A category bullet contains `` `<name>/` `` (trailing slash inside
            // the backticks); any other `` - ` `` bullet is a leaf skill and
            // must never be followed by the count parenthetical `` (` ``.
            if line.contains("- `") && !line.contains("/`") {
                assert!(
                    !line.contains("` ("),
                    "skill line must not carry a count parenthetical: {line}"
                );
            }
        }
    }

    /// The tolerance contract: a dangling symlink and an unreadable nested
    /// subdir are skipped (never brick the listing), matching what the
    /// `collect_entries` / `count_children` docs and the meta-skill prose lean
    /// on. Unix-only (chmod/symlink).
    #[cfg(unix)]
    #[test]
    fn render_skill_index_skips_unreadable_entries() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("good.md"),
            "---\nname: good\ndescription: intact skill\n---\n",
        )
        .unwrap();
        // A dangling symlink — `fs::metadata` fails, so it is skipped.
        std::os::unix::fs::symlink(root.join("missing.md"), root.join("dangling.md")).unwrap();

        // A nested subdir made unreadable (mode 0) — tolerated as an empty
        // category rather than aborting the whole index.
        let cat = root.join("cat");
        fs::create_dir_all(&cat).unwrap();
        fs::write(cat.join("README.md"), "---\ndescription: cat\n---\n").unwrap();
        let locked = cat.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let out = render_skill_index(root, 2).unwrap();
        // The good skill and the category still render.
        assert!(out.contains("- `good` — intact skill"));
        assert!(out.contains("- `cat/` (1 subcategory) — cat"));
        // The dangling symlink never appears as a skill.
        assert!(!out.contains("dangling"));
        // The unreadable subdir is counted as a subcategory of cat (its name is
        // listed by read_dir; only its *contents* are unreadable) and rendered
        // unexpanded with an empty count, not inlined.
        assert!(out.contains("  - `locked/` (empty)"));

        // Restore so tempfile cleanup can remove it.
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
    }
}
