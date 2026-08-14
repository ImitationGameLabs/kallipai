//! Auto-generated `kallip --reference` output: the full command tree rendered
//! from the clap definitions, so the listing can never drift from the binary.

use clap::{Arg, ArgAction, Command, CommandFactory};

use crate::args::Cli;

/// The complete kallip command reference as markdown.
///
/// Walks the clap tree defined by [`Cli`]. Leaves render under their parent
/// group; the flattened per-agent ops (`message`/`status`/`activity`, which
/// have no group node because `Agent` is `#[command(flatten)]`) render under a
/// synthetic "Per-agent ops" heading so they aren't orphans. clap's
/// auto-injected `help` subcommand and `--help`/`--version` flags are filtered
/// out — the reference shows only real commands and their real arguments.
pub fn render() -> String {
    let root = Cli::command();
    let mut out = String::new();
    out.push_str("# kallip reference\n\n");
    out.push_str(
        "Auto-generated from the installed binary; always matches the kallip you are \
         running. Pin this (label `kallip:reference`) for command syntax — skills carry \
         the when/why judgment.\n\n",
    );

    // Partition the root's children into ungrouped leaves (flattened agent ops)
    // and groups (commands with their own subcommands). Declaration order is
    // preserved within each partition.
    let mut ungrouped: Vec<&Command> = Vec::new();
    let mut groups: Vec<&Command> = Vec::new();
    for child in root.get_subcommands() {
        if is_help_subcommand(child) {
            continue;
        }
        if has_real_subcommands(child) {
            groups.push(child);
        } else {
            ungrouped.push(child);
        }
    }

    if !ungrouped.is_empty() {
        out.push_str("## Per-agent ops (self + peers)\n\n");
        for leaf in &ungrouped {
            render_leaf(&mut out, leaf, &[]);
        }
    }
    for group in &groups {
        out.push_str(&format!("## {}\n", group.get_name()));
        if let Some(about) = group.get_about() {
            out.push_str(&format!("\n{about}\n"));
        }
        out.push('\n');
        for leaf in group.get_subcommands() {
            if is_help_subcommand(leaf) {
                continue;
            }
            render_leaf(&mut out, leaf, &[group.get_name()]);
        }
    }

    out
}

/// clap injects a `help` subcommand into any command that has subcommands.
fn is_help_subcommand(cmd: &Command) -> bool {
    cmd.get_name() == "help"
}

fn has_real_subcommands(cmd: &Command) -> bool {
    cmd.get_subcommands().any(|c| !is_help_subcommand(c))
}

fn render_leaf(out: &mut String, leaf: &Command, group_path: &[&str]) {
    let full = std::iter::once("kallip")
        .chain(group_path.iter().copied())
        .chain(std::iter::once(leaf.get_name()))
        .collect::<Vec<_>>()
        .join(" ");
    match leaf.get_about() {
        Some(about) => out.push_str(&format!("### {full} — {about}\n")),
        None => out.push_str(&format!("### {full}\n")),
    }
    out.push_str(&format!("`{full}`\n"));

    let real_args: Vec<&Arg> = leaf.get_arguments().filter(|a| !is_auto_arg(a)).collect();
    if real_args.is_empty() {
        out.push_str("- (no arguments)\n");
    } else {
        for arg in real_args {
            out.push_str(&format!("- {}\n", render_arg(arg)));
        }
    }
    out.push('\n');
}

/// clap injects `--help`/`-h`/`--version`/`-V` into every command.
fn is_auto_arg(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
    )
}

fn render_arg(arg: &Arg) -> String {
    let descriptor = arg_descriptor(arg);
    let required = if arg.is_required_set() {
        " (required)"
    } else {
        ""
    };
    let help = arg
        .get_help()
        .map(|h| format!(" — {h}"))
        .unwrap_or_default();
    format!("{descriptor}{required}{help}")
}

/// The arg's invocation form: `<POS>` for positionals, `--flag` for booleans,
/// `--long <VALUE>` (or `-s, --long <VALUE>`) for valued args.
fn arg_descriptor(arg: &Arg) -> String {
    let long = arg.get_long();
    let short = arg.get_short();
    // No long and no short => positional (the auto help/version args are
    // already filtered and do have long/short).
    if long.is_none() && short.is_none() {
        return format!("<{}>", value_name(arg));
    }
    let prefix = match (short, long) {
        (Some(s), Some(l)) => format!("-{s}, --{l}"),
        (_, Some(l)) => format!("--{l}"),
        (Some(s), _) => format!("-{s}"),
        // Unreachable: at least one of long/short is Some here.
        (None, None) => String::new(),
    };
    match arg.get_action() {
        // Boolean flags take no value.
        ArgAction::SetTrue | ArgAction::SetFalse => prefix,
        _ => format!("{prefix} <{}>", value_name(arg)),
    }
}

/// The metavariable for a valued arg: its declared value name, or the uppercased
/// arg id as a fallback (clap populates value names from `value_name`/type).
fn value_name(arg: &Arg) -> String {
    if let Some(names) = arg.get_value_names()
        && let Some(first) = names.first()
    {
        return first.to_string();
    }
    arg.get_id().to_string().to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_starts_with_h1_and_blurb() {
        let out = render();
        assert!(
            out.starts_with("# kallip reference\n\n"),
            "reference must open with the H1 + blurb: {out}"
        );
        assert!(out.contains("Auto-generated from the installed binary"));
    }

    /// Every leaf command path appears. Hard-coded as a regression net against
    /// silent tree changes (a renamed/removed command drops its line and fails
    /// here).
    #[test]
    fn render_contains_every_leaf() {
        let out = render();
        let leaves = [
            // Per-agent ops (flattened, ungrouped).
            "kallip message",
            "kallip status",
            "kallip activity",
            // approval
            "kallip approval list",
            "kallip approval get",
            "kallip approval approve",
            "kallip approval deny",
            // policy
            "kallip policy show",
            "kallip policy exec-get",
            "kallip policy exec-set",
            // skill
            "kallip skill index",
            "kallip skill meta",
            // budget
            "kallip budget get",
            "kallip budget increase",
            "kallip budget decrease",
            "kallip budget set",
            // subagent
            "kallip subagent spawn",
            "kallip subagent list",
            "kallip subagent remove",
            "kallip subagent interrupt",
            "kallip subagent metadata",
            // dirlock
            "kallip dirlock acquire",
            "kallip dirlock release",
            "kallip dirlock status",
            "kallip dirlock who",
            // lesche
            "kallip lesche send",
            // inbox
            "kallip inbox list",
            "kallip inbox read",
            "kallip inbox summary",
            "kallip inbox done",
            "kallip inbox clear",
        ];
        for leaf in leaves {
            assert!(
                out.contains(&format!("### {leaf}")),
                "reference must list leaf `{leaf}`: {out}"
            );
        }
    }

    #[test]
    fn render_omits_auto_help_and_version() {
        let out = render();
        // No `--help`/`--version` arg lines and no `### kallip help` entry.
        for needle in ["--help", "--version", "-h,", "### kallip help"] {
            assert!(
                !out.contains(needle),
                "reference must omit clap auto arg `{needle}`: {out}"
            );
        }
    }

    /// Every rendered leaf has a one-line description after the ` — ` separator
    /// (catches a TERSE variant doc slipping through as a bare label).
    #[test]
    fn every_leaf_has_about() {
        let out = render();
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("### kallip ") {
                assert!(
                    rest.contains(" — "),
                    "leaf heading must carry a description: {line}"
                );
            }
        }
    }
}
