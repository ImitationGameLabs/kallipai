use anyhow::Result;
use just_agent_client::DaemonClient;
use just_agent_common::agentid::AgentId;

/// How the daemon event stream ended, when it stops delivering events.
///
/// Both variants represent an *involuntary* end — the client did not choose to
/// quit, the daemon went away — so [`StreamEnd::into_error`] always produces an
/// error and the client exits non-zero. Only the message differs.
///
/// This lives with [`Session`] because it is a session-lifecycle outcome, the
/// peer of [`Session::cleanup`](Self::cleanup).
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

/// Role for the root agent the TUI auto-creates. Labels the top-level agent
/// that supervises all subagents and holds the highest policy privileges, so
/// it stands out in logs/lists next to its subordinates rather than blending
/// in as a bare UUID.
const ROOT_ROLE: &str = "root";

/// Default description for the auto-created root agent.
const ROOT_DESCRIPTION: &str =
    "Top-level agent: supervises all subagents and holds the highest policy privileges.";

/// Holds the daemon connection and agent identity.
pub(crate) struct Session {
    pub client: DaemonClient,
    pub agent_id: AgentId,
}

impl Session {
    /// Remove the agent when requested on exit.
    pub async fn cleanup(&self, kill: bool) {
        if kill {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.client.remove_agent(&self.agent_id),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("failed to remove agent on exit: {e}"),
                Err(_) => tracing::warn!("timed out deleting agent on exit"),
            }
        }
    }

    /// Connect to (or create) a root agent.
    ///
    /// Reuses an existing root agent (`created_by == None`) if one exists,
    /// otherwise spawns a new one labelled with the default [`ROOT_ROLE`] /
    /// [`ROOT_DESCRIPTION`] so it is identifiable in logs and the agent list.
    pub async fn connect(client: DaemonClient) -> Result<Self> {
        let agents = client.list_agents(None).await?;
        if let Some(root) = agents.into_iter().find(|a| a.created_by.is_none()) {
            return Ok(Self {
                client,
                agent_id: root.id,
            });
        }

        let agent_id = client
            .spawn(just_agent_common::protocol::CreateAgentRequest {
                workspace_root: None,
                skills: vec![],
                prompt: None,
                created_by: None,
                role: ROOT_ROLE.to_string(),
                description: ROOT_DESCRIPTION.to_string(),
                max_tool_rounds: None,
            })
            .await?;
        Ok(Self { client, agent_id })
    }
}
