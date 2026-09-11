//! The instance backend: the transport seam behind the four management
//! routes. The local implementation is a thin wrapper over the daemon's
//! UDS client; any other source (an orchestration API) implements the
//! same trait without the HTTP layer knowing the difference.
//!
//! The trait stays in this crate (not a `-common` package) until a second
//! consumer exists -- same rule as the `BearerVerifier` seam.

use std::sync::Arc;

use async_trait::async_trait;
use kallip_daemon_client::DaemonClient;
use kallip_daemon_common::wire::{HealthReport, InstanceInfo, RequestBody};

use crate::api::{Spawned, Stopped};
use crate::wire::{BackendError, unwrap_health, unwrap_list, unwrap_spawn, unwrap_stop};

/// The full management surface, one method per HTTP route.
#[async_trait]
pub trait InstanceBackend: Send + Sync + 'static {
    /// Launch one instance under `user` (None = the daemon's
    /// implicit-launch rules); the outcome carries its port.
    async fn spawn(
        &self,
        slug: String,
        workspace: String,
        env: Vec<String>,
        user: Option<String>,
    ) -> Result<Spawned, BackendError>;

    /// Stop one instance by slug.
    async fn stop(&self, slug: String) -> Result<Stopped, BackendError>;

    /// Relaunch a stopped or dead instance from its persisted tree.
    /// The outcome is spawn-shaped: a fresh process with its listen port.
    async fn start(&self, slug: String) -> Result<Spawned, BackendError>;

    /// Every instance the backend sees.
    async fn list(&self) -> Result<Vec<InstanceInfo>, BackendError>;

    /// Liveness of the backend itself, or one instance by slug.
    async fn health(&self, slug: Option<String>) -> Result<HealthReport, BackendError>;
    /// Provisioning methods this backend supports, as the shared
    /// product vocabulary (designated-user / isolated-user /
    /// container). The web renders the create flow from this list;
    /// an empty set hides the create entry, not the page.
    fn capabilities(&self) -> Vec<String>;
}

/// The local backend: one UDS exchange per call against the host daemon.
pub struct UdsBackend {
    client: DaemonClient,
}

impl UdsBackend {
    pub fn arc(client: DaemonClient) -> Arc<dyn InstanceBackend> {
        Arc::new(Self { client })
    }
}

#[async_trait]
impl InstanceBackend for UdsBackend {
    async fn spawn(
        &self,
        slug: String,
        workspace: String,
        env: Vec<String>,
        user: Option<String>,
    ) -> Result<Spawned, BackendError> {
        // The env passes through verbatim: relay-URL defaults are the
        // daemon's fill (it owns the deployment's relay URLs), applied
        // before the record snapshot is written on its side.
        let wire = self
            .client
            .call(RequestBody::Spawn {
                slug,
                workspace,
                env,
                exe: None,
                user,
            })
            .await?;
        unwrap_spawn(wire)
    }

    async fn stop(&self, slug: String) -> Result<Stopped, BackendError> {
        let wire = self.client.call(RequestBody::Stop { slug }).await?;
        unwrap_stop(wire)
    }

    async fn start(&self, slug: String) -> Result<Spawned, BackendError> {
        // The daemon answers start with the shared spawn-shaped launch
        // payload, so the unwrap is verbatim.
        let wire = self
            .client
            .call(RequestBody::Start {
                slug,
                env: Vec::new(),
                exe: None,
            })
            .await?;
        unwrap_spawn(wire)
    }

    async fn list(&self) -> Result<Vec<InstanceInfo>, BackendError> {
        let wire = self.client.call(RequestBody::List).await?;
        unwrap_list(wire)
    }

    async fn health(&self, slug: Option<String>) -> Result<HealthReport, BackendError> {
        let wire = self.client.call(RequestBody::Health { slug }).await?;
        unwrap_health(wire)
    }
    fn capabilities(&self) -> Vec<String> {
        vec!["designated-user".to_string()]
    }
}

// Re-exported so handler code names one error type regardless of backend.
pub use crate::wire::BackendError as Error;
