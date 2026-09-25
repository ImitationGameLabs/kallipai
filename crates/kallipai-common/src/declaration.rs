//! Team declaration file (`tagma.toml`) parsing.
//!
//! A declaration states what the team should look like: one `[[role]]`
//! table per role, naming the role and pinning its spawn shape (prompt,
//! skills, permission class, profile set). The tagma's status face and
//! the CLI's converge face both parse through this module so they can
//! never disagree about what a declaration says. The lock file
//! (`tagma.lock`, role-to-id records) is deliberately not parsed here:
//! it is a CLI-side archive the tagma never holds or reads — it reaches
//! the status face as plain role-id pairs on the request.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// One `[[role]]` entry: the desired shape of a single team role.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoleDeclaration {
    /// Registry-unique role label (`"dev"`). Required, non-empty: the
    /// lock file and the registry both key on it.
    pub name: String,
    /// What the role is for; converge aligns the live agent's
    /// description to it.
    #[serde(default)]
    pub description: String,
    /// System prompt for the role. Absent = the spawn default applies.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Skills the role is spawned with.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Profile set the role resolves against, by exact name. Optional
    /// here only so a partially-specified declaration still loads for
    /// the status face; converge cannot spawn a role without one.
    #[serde(default)]
    pub profile_set: Option<String>,
    /// FS-access permission class, lowercase wire spelling (`"normal"` /
    /// `"guest"`) — same vocabulary as
    /// `CreateAgentRequest::permission_class`. Optional for the same
    /// reason as `profile_set`.
    #[serde(default)]
    pub permission_class: Option<String>,
    /// Converge exemption flag: live agents under an unmanaged role are
    /// left entirely alone (no deactivation, no metadata alignment) and
    /// listed separately on the status face. Exemption is explicit,
    /// never silent.
    #[serde(default)]
    pub unmanaged: bool,
}

/// A parsed declaration file: the desired team.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamDeclaration {
    /// Roles in file order. Empty is well-formed (an empty team).
    #[serde(default, rename = "role")]
    pub roles: Vec<RoleDeclaration>,
}

/// Parse declaration file contents into a [`TeamDeclaration`].
///
/// Unknown fields are ignored on purpose: declarations gain fields
/// across releases, and an older binary must still read a newer file —
/// the fields it does not know simply exert no convergence pressure.
/// Role names must be non-empty, unique, and free of leading or
/// trailing whitespace: they key the lock file and the registry, and a
/// padded spelling would silently fork from its clean one.
pub fn parse_declaration(input: &str) -> anyhow::Result<TeamDeclaration> {
    let declaration: TeamDeclaration =
        toml::from_str(input).context("invalid team declaration TOML")?;
    let mut seen = std::collections::HashSet::with_capacity(declaration.roles.len());
    for role in &declaration.roles {
        if role.name.trim().is_empty() {
            bail!("declaration contains a role with an empty name");
        }
        if role.name.trim() != role.name {
            bail!(
                "declaration role name {:?} has leading or trailing whitespace",
                role.name
            );
        }
        // The role becomes a path component (<root workspace>/team/<role>):
        // confining it to ASCII letters, digits, hyphen, and underscore
        // keeps `..`, separators, and whitespace out of that path by
        // construction rather than by a downstream starts_with check.
        // This is a charset, not a Windows-safe name set: reserved device
        // names (CON, NUL, …) pass on purpose — deployment targets are POSIX.
        if !role
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!(
                "declaration role name {:?} must use only ASCII letters, digits, '-', or '_'",
                role.name
            );
        }
        if !seen.insert(role.name.as_str()) {
            bail!("declaration contains duplicate role name '{}'", role.name);
        }
    }
    Ok(declaration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_fully_specified_role() {
        let declaration = parse_declaration(
            r#"
[[role]]
name = "dev"
description = "the sole writer of kallipai-dev"
prompt = "you are the surgeon"
skills = ["code/development", "code/reviewing"]
profile_set = "team"
permission_class = "normal"
unmanaged = false
"#,
        )
        .unwrap();
        assert_eq!(declaration.roles.len(), 1);
        let role = &declaration.roles[0];
        assert_eq!(role.name, "dev");
        assert_eq!(role.description, "the sole writer of kallipai-dev");
        assert_eq!(role.prompt.as_deref(), Some("you are the surgeon"));
        assert_eq!(role.skills, vec!["code/development", "code/reviewing"]);
        assert_eq!(role.profile_set.as_deref(), Some("team"));
        assert_eq!(role.permission_class.as_deref(), Some("normal"));
        assert!(!role.unmanaged);
    }

    #[test]
    fn a_name_only_role_takes_the_defaults() {
        let declaration = parse_declaration("[[role]]\nname = \"scout\"\n").unwrap();
        let role = &declaration.roles[0];
        assert_eq!(role.name, "scout");
        assert_eq!(role.description, "");
        assert_eq!(role.prompt, None);
        assert!(role.skills.is_empty());
        assert_eq!(role.profile_set, None);
        assert_eq!(role.permission_class, None);
        assert!(!role.unmanaged);
    }

    #[test]
    fn an_empty_file_is_an_empty_team() {
        let declaration = parse_declaration("").unwrap();
        assert!(declaration.roles.is_empty());
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        let declaration = parse_declaration(
            "revision = 3\n\n[[role]]\nname = \"dev\"\nfuture_field = \"whatever\"\n",
        )
        .unwrap();
        assert_eq!(declaration.roles.len(), 1);
        assert_eq!(declaration.roles[0].name, "dev");
    }

    #[test]
    fn duplicate_role_names_are_rejected() {
        let err = parse_declaration("[[role]]\nname = \"dev\"\n\n[[role]]\nname = \"dev\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("duplicate role name 'dev'"));
    }

    #[test]
    fn an_empty_role_name_is_rejected() {
        let err = parse_declaration("[[role]]\nname = \"  \"\n").unwrap_err();
        assert!(err.to_string().contains("empty name"));
    }

    #[test]
    fn a_padded_role_name_is_rejected_rather_than_trimmed() {
        let err = parse_declaration("[[role]]\nname = \" dev \"\n").unwrap_err();
        assert!(err.to_string().contains("leading or trailing whitespace"));
    }

    #[test]
    fn role_names_cannot_escape_the_workspace_path_component() {
        for bad in ["../etc", "a/b", "a\\b", "a.b", "a b", "résumé"] {
            let src = format!("[[role]]\nname = \"{bad}\"\n");
            let err = parse_declaration(&src).unwrap_err();
            assert!(
                err.to_string().contains("must use only ASCII letters"),
                "unexpected error for {bad:?}: {err:#}"
            );
        }
    }

    #[test]
    fn windows_reserved_device_names_pass_the_charset_guard() {
        // Cross-platform semantics note: the guard confines a role to a
        // charset, not to a Windows-safe name set — CON and friends
        // parse by design (deployment targets are POSIX). Pins the
        // documented behavior so a future tighten is a conscious change.
        parse_declaration("[[role]]\nname = \"con\"").unwrap();
    }
}
