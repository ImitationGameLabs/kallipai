use anyhow::Result;
use just_agent_client::DaemonClient;
use just_agent_common::agentid::AgentId;

/// Holds the daemon connection and agent identity.
pub(crate) struct Session {
    pub client: DaemonClient,
    pub agent_id: AgentId,
}

impl Session {
    /// Stop the agent if `kill` is true.
    pub async fn cleanup(&self, kill: bool) {
        if kill {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.client.stop_agent(&self.agent_id),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("failed to stop agent on exit: {e}"),
                Err(_) => tracing::warn!("timed out stopping agent on exit"),
            }
        }
    }

    /// Connect to (or create) a root agent.
    ///
    /// Reuses an existing root agent (`created_by == None`) if one exists,
    /// otherwise spawns a new one.
    pub async fn connect(client: DaemonClient) -> Result<Self> {
        let agents = client.list_agents().await?;
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
            })
            .await?;
        Ok(Self { client, agent_id })
    }
}
