use anyhow::Result;
use kallip_client::DaemonClient;
use kallip_common::agentid::AgentId;

/// How the daemon event stream ended, when it stops delivering events.
///
/// Both variants represent an *involuntary* end — the client did not choose to
/// quit, the daemon went away — so [`StreamEnd::into_error`] always produces an
/// error and the client exits non-zero. Only the message differs.
///
/// This lives with [`Session`] because it is a session-lifecycle outcome.
#[derive(Debug)]
pub enum StreamEnd {
    /// The daemon closed the stream: graceful shutdown or agent removal.
    Graceful,
    /// The connection failed (daemon crash / network drop) **or** a stream/
    /// decode error occurred. The latter is rare in practice — the daemon
    /// drops malformed events server-side — but the underlying
    /// `JsonEventStream` terminates on any `parse_event` error, so this arm
    /// covers more than just a dropped connection.
    Failed(anyhow::Error),
}

impl StreamEnd {
    /// Build the error to propagate for this stream end.
    ///
    /// Always returns `Err`: a clean shutdown is still an involuntary
    /// termination of the client, so it exits non-zero like a failure. The
    /// caller is responsible for restoring the terminal *before* propagating so
    /// the message is not garbled by the alt-screen / raw mode.
    pub fn into_error(self) -> anyhow::Error {
        match self {
            Self::Graceful => anyhow::anyhow!("daemon shut down; session ended"),
            Self::Failed(e) => e.context("lost connection to daemon"),
        }
    }
}

/// Holds the daemon connection and the daemon-owned root agent's id.
///
/// The TUI never creates or removes the root; it binds to the daemon's single
/// root agent (eagerly created at daemon startup) for the process lifetime.
pub(crate) struct Session {
    pub client: DaemonClient,
    pub agent_id: AgentId,
}

impl Session {
    /// Connect to the daemon's single root agent.
    ///
    /// The daemon owns exactly one root (eagerly created at startup via
    /// `ensure_root_agent`); the TUI binds to it directly instead of the old
    /// list-then-spawn dance.
    pub async fn connect(client: DaemonClient) -> Result<Self> {
        let root = client.get_root_agent().await?;
        Ok(Self {
            client,
            agent_id: root.id,
        })
    }
}
