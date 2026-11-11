//! Dynamic completion for slug arguments.

use clap_complete::engine::CompletionCandidate;
use kallipai_daemon_client::DaemonClient;
use std::ffi::OsStr;

/// Dynamic completion for slug arguments: list the daemon's managed
/// instances and offer their slugs. Every failure path (no reachable
/// socket, IPC error) degrades to an empty candidate set: a completion
/// hook must never print an error into the shell's input line.
pub(crate) fn complete_slug(current: &OsStr) -> Vec<CompletionCandidate> {
    use kallipai_daemon_common::wire::RequestBody;
    use kallipai_daemon_common::wire::ResponseBody;

    // The hook runs before any tokio runtime is up, so a throwaway
    // current-thread runtime drives the async UDS client here.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return Vec::new(),
    };
    let prefix = current.to_string_lossy();
    rt.block_on(async move {
        let candidates = kallipai_daemon_common::socket::candidates_from_env(None);
        let socket = match kallipai_daemon_common::socket::probe(&candidates) {
            Ok(socket) => socket,
            Err(_) => return Vec::new(),
        };
        let client = DaemonClient::new(socket);
        let response = match client.call(RequestBody::List).await {
            Ok(response) => response,
            Err(_) => return Vec::new(),
        };
        let ResponseBody::Ok {
            payload: kallipai_daemon_common::wire::OkPayload::List { instances },
        } = response.body
        else {
            return Vec::new();
        };
        instances
            .into_iter()
            .filter(|instance| instance.slug.starts_with(prefix.as_ref()))
            .map(|instance| {
                CompletionCandidate::new(instance.slug).help(Some(instance.state.as_str().into()))
            })
            .collect()
    })
}

/// Dynamic completion for `polis` names: the register's names, filtered by
/// what has been typed, with the address as the help line. A pure file read
/// -- no daemon, no network -- and every failure degrades to an empty set,
/// for the same reason the slug completer does.
pub(crate) fn complete_polis_name(current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let Ok(registry) = crate::registry::Registry::load() else {
        return Vec::new();
    };
    matching_names(&registry, &prefix)
        .into_iter()
        .map(|(name, address)| {
            CompletionCandidate::new(name).help(Some(clap::builder::StyledStr::from(address)))
        })
        .collect()
}

/// The register's `(name, address)` pairs starting with `prefix`, in the
/// register's own order.
fn matching_names(registry: &crate::registry::Registry, prefix: &str) -> Vec<(String, String)> {
    registry
        .entries()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, entry)| (name.to_owned(), entry.url.clone()))
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use super::matching_names;
    use crate::registry::Registry;

    #[test]
    fn names_complete_by_prefix_with_their_address() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("polis-registry.toml");
        let mut registry = Registry::load_from(&path).expect("an empty register");
        registry
            .add("local", "http://127.0.0.1:8080", false)
            .expect("local");
        registry
            .add("cloud", "https://api.example.com", false)
            .expect("cloud");

        let all = matching_names(&registry, "");
        assert_eq!(
            all,
            vec![
                ("cloud".to_owned(), "https://api.example.com".to_owned()),
                ("local".to_owned(), "http://127.0.0.1:8080".to_owned()),
            ]
        );
        let lo = matching_names(&registry, "lo");
        assert_eq!(lo.len(), 1, "{lo:?}");
        assert_eq!(lo[0].0, "local");
        assert!(matching_names(&registry, "zz").is_empty());
    }
}
