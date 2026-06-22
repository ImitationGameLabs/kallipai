use anyhow::Result;
use just_agent_client::DaemonClient;
use just_agent_common::agentid::AgentId;

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
