use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use just_agent_core::command::UserInput;
use just_agent_core::config::AgentConfig;
use just_agent_core::context::ContextStore;
use just_agent_core::deferred::DeferredQueue;
pub use just_agent_core::types::AgentState;
use just_agent_core::types::SseEvent;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub agents: RwLock<Vec<AgentEntry>>,
    pub shutdown: CancellationToken,
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
    pub session_dir: Option<PathBuf>,
    pub cancel: CancellationToken,
    pub state: Arc<AtomicU8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub workspace_root: String,
    pub state: AgentState,
}

impl Agent {
    pub fn get_state(&self) -> AgentState {
        match self.state.load(Ordering::Relaxed) {
            AgentState::BUSY => AgentState::Busy,
            _ => AgentState::Idle,
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self { agents: RwLock::new(Vec::new()), shutdown: CancellationToken::new() }
    }
}
