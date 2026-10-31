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
