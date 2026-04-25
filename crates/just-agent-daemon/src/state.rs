use std::sync::Arc;

use just_agent_core::command::UserInput;
use just_agent_core::config::AgentConfig;
use just_agent_core::context::ContextStore;
use just_agent_core::deferred::DeferredQueue;
use just_agent_core::types::SseEvent;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub agents: RwLock<Vec<AgentEntry>>,
}

pub struct AgentEntry {
    pub id: String,
    pub agent: Agent,
}

pub struct Agent {
    pub prompt_tx: mpsc::Sender<UserInput>,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub deferred: Arc<Mutex<DeferredQueue>>,
    pub config: AgentConfig,
    pub agent_handle: JoinHandle<()>,
    pub bridge_handle: JoinHandle<()>,
    pub store: Arc<Mutex<ContextStore>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub workspace_root: String,
    pub skills: Vec<String>,
}

impl AppState {
    pub fn new() -> Self {
        Self { agents: RwLock::new(Vec::new()) }
    }
}
