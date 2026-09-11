//! Delegated administration: the system accounts allowed to act on behalf
//! of other declared users across the management verbs (spawn, adopt,
//! stop, start, remove, log). The list is deployment configuration —
//! the daemon neither discovers nor invents delegates.
//!
//! `KALLIP_DAEMON_DELEGATES` carries a comma-separated user-name list.
//! Startup resolves each name through passwd and refuses to start on an
//! unresolvable name, a root entry, or the daemon's own account: a
//! silently narrower grant is a trap, so the failure is loud. An absent
//! or empty value is the default and means no delegation at all — the
//! single-user story is untouched.

use std::collections::HashSet;
use std::sync::OnceLock;

static DELEGATES: OnceLock<HashSet<u32>> = OnceLock::new();

/// The delegated peer uid set; empty until startup installs the parsed
/// env list (and always empty when the env is absent — the default).
pub(crate) fn current() -> &'static HashSet<u32> {
    DELEGATES.get_or_init(HashSet::new)
}

/// Parse `KALLIP_DAEMON_DELEGATES` and install the uid set for the
/// process lifetime. Called once from `main`; any error aborts startup.
pub(crate) fn init_from_env() -> Result<(), String> {
    let raw = match std::env::var("KALLIP_DAEMON_DELEGATES") {
        // Not-unicode is a deployment mistake, not an empty list:
        // refuse loudly instead of silently granting nothing.
        Err(std::env::VarError::NotUnicode(value)) => {
            return Err(format!(
                "KALLIP_DAEMON_DELEGATES is not valid unicode: {value:?}"
            ));
        }
        Err(std::env::VarError::NotPresent) => String::new(),
        Ok(raw) => raw,
    };
    let set = resolve_list(&raw)?;
    let euid = unsafe { libc::geteuid() };
    if set.contains(&euid) {
        return Err(format!(
            "delegate list may not contain the daemon's own account (uid {euid})"
        ));
    }
    DELEGATES
        .set(set)
        .map_err(|_| "delegate list already initialized".to_string())
}

/// Name list -> uid set. Empty/whitespace items are skipped, so an
/// unset or empty env yields the empty (no-delegation) set. Root is
/// rejected here: delegation grants instance-management authority, and
/// root already holds all of it — listing root can only be a mistake.
fn resolve_list(raw: &str) -> Result<HashSet<u32>, String> {
    let mut set = HashSet::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let user = crate::spawn::passwd_by_name(name)
            .ok_or_else(|| format!("delegate user {name:?} does not exist on this host"))?;
        if user.uid == 0 {
            return Err("delegate list may not contain root".to_string());
        }
        set.insert(user.uid);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runner's own passwd name (guaranteed to exist and to resolve
    /// to the current euid), or None on a host without a passwd entry.
    fn own_name() -> Option<String> {
        crate::spawn::cached_passwd_identity().map(|(name, _)| name.to_string_lossy().into_owned())
    }

    #[test]
    fn empty_and_blank_lists_resolve_to_no_delegation() {
        assert!(resolve_list("").unwrap().is_empty());
        assert!(resolve_list(" , ,").unwrap().is_empty());
    }

    #[test]
    fn unknown_name_refuses() {
        let err = resolve_list("no-such-kallip-delegate-account").unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn root_refuses() {
        let err = resolve_list("root").unwrap_err();
        assert!(err.contains("may not contain root"), "{err}");
    }

    #[test]
    fn existing_names_resolve_to_uids() {
        // A root runner's own name is 'root', which resolve_list
        // rejects before resolving; fall back to the conventional
        // unprivileged account. Skips cleanly when nothing on this
        // host resolves.
        let euid = unsafe { libc::geteuid() };
        let name = if euid == 0 {
            "nobody".to_string()
        } else {
            let Some(name) = own_name() else {
                return;
            };
            name
        };
        let Some(user) = crate::spawn::passwd_by_name(&name) else {
            return;
        };
        let set = resolve_list(&name).unwrap();
        assert!(set.contains(&user.uid), "{name:?} -> {set:?}");
    }

    #[test]
    fn init_rejects_the_daemon_account_itself() {
        // Failure paths leave the OnceLock untouched, so this is safe
        // to run in any order; the success path installs process-wide
        // state and is exercised by the daemon binary, not by tests.
        let euid = unsafe { libc::geteuid() };
        if euid == 0 {
            // Under a root runner the self-name is 'root', rejected
            // earlier by the root arm; the self arm is reachable only
            // from a non-root daemon, whose name differs from root.
            return;
        }
        let Some(name) = own_name() else {
            return;
        };
        let err = init_from_env_with(&name).unwrap_err();
        assert!(err.contains("own account"), "{err}");
    }

    #[test]
    fn init_rejects_root() {
        let err = init_from_env_with("root").unwrap_err();
        assert!(err.contains("may not contain root"), "{err}");
    }

    /// init_from_env minus the env read: the same resolution plus the
    /// self-check plus the install, driven by an explicit list so tests
    /// can address the failure paths without process env mutation.
    fn init_from_env_with(raw: &str) -> Result<(), String> {
        let set = resolve_list(raw)?;
        let euid = unsafe { libc::geteuid() };
        if set.contains(&euid) {
            return Err(format!(
                "delegate list may not contain the daemon's own account (uid {euid})"
            ));
        }
        DELEGATES
            .set(set)
            .map_err(|_| "delegate list already initialized".to_string())
    }
}
