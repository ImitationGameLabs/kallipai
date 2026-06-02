use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use anyhow::Context as _;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_common::types::{AgentId, SseEvent, ToolPolicy};
use just_agent_runtime::config::{AgentConfig, PermissionProfile};
use just_agent_runtime::context::{AgenticContext, ContextStore, ContextSummarizer};
use just_agent_runtime::deferred::DeferredActionStore;
use just_agent_runtime::persistence;
use just_agent_runtime::policy::{AgentPolicy, AuthorizedToolExecutor};
use just_agent_runtime::provider::client_from_env;
use just_agent_runtime::session::{self, AgentContext};
use just_agent_runtime::tools::{build_tool_dispatch, ensure_meta_skill, load_skill};
use just_llm_client::types::chat::ChatMessage;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use just_agent_common::types::{CreateAgentRequest, CreateAgentResponse};

use super::ListAgentsResponse;
use crate::bridge::bridge_task;
use crate::state::{Agent, AgentEntry, AgentState, AgentSummary, SharedState};

pub(crate) struct SpawnArgs {
    pub store: Arc<tokio::sync::Mutex<ContextStore>>,
    pub deferred: Arc<tokio::sync::Mutex<DeferredActionStore>>,
    pub session_dir: PathBuf,
    pub config: AgentConfig,
    pub initial_prompt: Option<String>,
    pub shutdown_cancel: CancellationToken,
    pub events_tx: broadcast::Sender<SseEvent>,
    pub auth_token: String,
    pub env: HashMap<String, String>,
    pub shared_state: SharedState,
    pub tool_policy: Arc<std::sync::RwLock<ToolPolicy>>,
}

/// Reconstruct runtime resources shared by create and restore.
pub(crate) async fn spawn_agent(args: SpawnArgs) -> anyhow::Result<Agent> {
    let cancel = args.shutdown_cancel.child_token();

    let client = {
        let meta = ensure_meta_skill()?;
        let mut sp = args.config.system_prompt.clone();
        sp.push_str("\n\n");
        sp.push_str(&meta);
        client_from_env(&sp)?
    };

    let dispatch = build_tool_dispatch(args.store.clone(), args.env.clone()).await?;

    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::channel(16);

    let executor = AuthorizedToolExecutor::new(
        dispatch,
        AgentPolicy::new(args.tool_policy.clone()),
        args.deferred.clone(),
    );
    let tool_defs = executor.tool_definitions();
    args.store.lock().await.set_tool_definitions(tool_defs);
    let pinned_budget =
        (args.config.effective_budget() as f64 * args.config.pinned_budget_ratio) as usize;
    args.store.lock().await.set_pinned_budget(pinned_budget);
    let summarizer = ContextSummarizer::new(args.config.summary_max_tokens);

    let ctx = AgentContext {
        client,
        store: args.store.clone(),
        deferred: args.deferred.clone(),
        executor,
        summarizer,
        config: args.config.clone(),
        session_dir: Some(args.session_dir.clone()),
        cancel: cancel.clone(),
    };

    let agent_handle = tokio::spawn(session::agent_task(
        ctx,
        args.initial_prompt,
        prompt_rx,
        agent_tx,
    ));
    let state = Arc::new(AtomicU8::new(AgentState::IDLE));
    // TODO: extract agent_id from SpawnArgs as a required field so the type system
    // enforces this invariant instead of relying on callers to set config.agent_id.
    let agent_id = args
        .config
        .agent_id
        .clone()
        .expect("agent_id must be set before spawn");
    let bridge_handle = tokio::spawn(bridge_task(
        agent_id,
        agent_rx,
        args.events_tx.clone(),
        args.shutdown_cancel.clone(),
        state.clone(),
        args.shared_state.clone(),
    ));

    Ok(Agent {
        prompt_tx,
        events_tx: args.events_tx,
        deferred: args.deferred,
        config: args.config,
        agent_handle,
        bridge_handle,
        store: args.store,
        session_dir: Some(args.session_dir),
        cancel,
        state,
        auth_token: args.auth_token,
        env: args.env,
        tool_policy: args.tool_policy,
    })
}

pub async fn create_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Root agents require operator privilege.
    if req.created_by.is_none() {
        crate::auth::require_operator(auth.identity())?;
    }

    let id = AgentId::random();
    let auth_token = uuid::Uuid::new_v4().to_string();

    let mut config = {
        let ws = req.workspace_root.map(std::path::PathBuf::from);
        AgentConfig::load(req.prompt, req.skills, ws)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    };
    config.agent_id = Some(id.clone());
    let mut env = HashMap::new();
    env.insert("JUST_AGENT_ID".into(), id.to_string());
    env.insert("JUST_AGENT_AUTH_TOKEN".into(), auth_token.clone());

    let mut tool_policy = ToolPolicy::default();

    // Subagent: validate supervisor and delegation constraints.
    if let Some(ref supervisor_id) = req.created_by {
        let registry = state.registry.read().await;
        let supervisor = registry.require_supervisor(auth.identity(), supervisor_id)?;

        let supervisor_perms = &supervisor.agent.config.permissions;
        if supervisor_perms.max_depth == 0 {
            return Err((
                StatusCode::FORBIDDEN,
                "supervisor has no remaining delegation depth".into(),
            ));
        }
        let subagent_ws = config.workspace_root.canonicalize().map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid workspace_root: {e}"),
            )
        })?;
        if !subagent_ws.starts_with(&supervisor_perms.workspace_root) {
            return Err((
                StatusCode::FORBIDDEN,
                "workspace_root must be within supervisor's workspace".into(),
            ));
        }

        config.created_by = Some(supervisor_id.clone());
        config.permissions = PermissionProfile::subagent(subagent_ws, supervisor_perms.max_depth);
        tool_policy = supervisor
            .agent
            .tool_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
    }

    let tool_policy = Arc::new(std::sync::RwLock::new(tool_policy));

    let store = Arc::new(tokio::sync::Mutex::new(ContextStore::new()));
    let deferred = Arc::new(tokio::sync::Mutex::new(DeferredActionStore::new()));

    for skill_name in &config.skills {
        let content =
            load_skill(skill_name).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        store
            .lock()
            .await
            .pin(
                &format!("skill:{skill_name}"),
                ChatMessage::user(format!("[skill: {skill_name}]\n{content}")),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        info!(skill = skill_name, "loaded skill");
    }

    let session_dir =
        persistence::create_session(&id, &config.workspace_root, config.created_by.as_ref())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    persistence::persist_policy(
        &session_dir,
        &tool_policy.read().unwrap_or_else(|e| e.into_inner()),
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let prompt = config.prompt.take();
    let log_ws = config.workspace_root.display().to_string();
    let log_depth = config.permissions.max_depth;
    let (events_tx, _) = broadcast::channel(256);
    let agent = spawn_agent(SpawnArgs {
        store,
        deferred,
        session_dir,
        config,
        initial_prompt: prompt,
        shutdown_cancel: state.shutdown.clone(),
        events_tx,
        auth_token: auth_token.clone(),
        env,
        shared_state: state.clone(),
        tool_policy: tool_policy.clone(),
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    {
        let mut registry = state.registry.write().await;
        // Re-verify supervisor was not deleted during agent spawn.
        if let Some(ref supervisor_id) = req.created_by
            && !registry.contains_key(supervisor_id)
        {
            // Supervisor gone — clean up the orphaned subagent.
            agent.agent_handle.abort();
            agent.bridge_handle.abort();
            if let Some(ref dir) = agent.session_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "supervisor agent was deleted during creation".into(),
            ));
        }
        registry.register(
            id.clone(),
            auth_token,
            AgentEntry {
                agent,
                subagent_ids: vec![],
            },
        );
    }
    info!(id = %id, supervisor = ?req.created_by, ws = %log_ws, depth = log_depth, "created agent");

    Ok((StatusCode::CREATED, Json(CreateAgentResponse { id })))
}

/// Any authenticated identity (operator or agent) may list agents.
/// The response contains no secrets (only IDs, workspace paths, and state).
pub async fn list_agents(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
) -> Json<ListAgentsResponse> {
    let registry = state.registry.read().await;
    let summaries: Vec<AgentSummary> = registry
        .iter()
        .map(|(id, entry)| AgentSummary {
            id: id.clone(),
            workspace_root: entry.agent.config.workspace_root.display().to_string(),
            state: entry.agent.get_state(),
            created_by: entry.agent.config.created_by.clone(),
        })
        .collect();
    Json(ListAgentsResponse { agents: summaries })
}

pub async fn delete_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, (StatusCode, String)> {
    let entry = {
        let mut registry = state.registry.write().await;
        registry.require_superior(auth.identity(), &id)?;
        let Some(entry) = registry.get(&id) else {
            return Ok(StatusCode::NOT_FOUND);
        };
        // Agent must be idle and have no subagents.
        if entry.agent.get_state() != AgentState::Idle {
            return Err((
                StatusCode::CONFLICT,
                "agent is busy, interrupt it first".into(),
            ));
        }
        if !entry.subagent_ids.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                "agent has active subagents, delete or interrupt them first".into(),
            ));
        }
        // Unregister under the same write lock — should always succeed since
        // `get` above confirmed the agent exists. Defensive fallback in case
        // the invariant is violated by a future refactor.
        match registry.unregister(&id) {
            Some(e) => e,
            None => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "agent vanished during deletion".into(),
                ));
            }
        }
    };

    // Signal graceful cancellation.
    entry.agent.cancel.cancel();

    // Wait briefly for the agent to persist and exit.
    // Since JoinHandle is not Clone, we sleep and then abort.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    entry.agent.agent_handle.abort();
    entry.agent.bridge_handle.abort();

    if let Err(e) = persistence::cleanup_session(&id) {
        info!(id = %id, "session cleanup failed: {e:#}");
    }
    info!(id = %id, "deleted agent");
    Ok(StatusCode::NO_CONTENT)
}

/// Interrupt the current agent operation without deleting it.
pub async fn interrupt_agent(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<StatusCode, (StatusCode, String)> {
    let registry = state.registry.read().await;
    registry.require_superior(auth.identity(), &id)?;
    let Some(entry) = registry.get(&id) else {
        return Ok(StatusCode::NOT_FOUND);
    };
    entry.agent.cancel.cancel();
    Ok(StatusCode::ACCEPTED)
}

/// One node in a supervisor chain, fully loaded from disk.
struct ChainNode {
    agent_id: AgentId,
    meta: persistence::SessionMeta,
    policy: ToolPolicy,
}

/// Pre-loaded session data for all agents being restored.
/// Eliminates redundant disk reads during supervisor chain validation
/// by caching meta and policy loaded during the scan phase.
struct RestoreIndex {
    meta: HashMap<AgentId, persistence::SessionMeta>,
    policy: HashMap<AgentId, ToolPolicy>,
}

impl RestoreIndex {
    /// Look up session metadata. Falls back to disk read on cache miss.
    fn get_meta(&self, id: &AgentId) -> anyhow::Result<persistence::SessionMeta> {
        match self.meta.get(id) {
            Some(m) => Ok(m.clone()),
            None => persistence::read_meta(id),
        }
    }

    /// Look up tool policy. Falls back to disk read on cache miss.
    fn get_policy(&self, id: &AgentId) -> anyhow::Result<ToolPolicy> {
        match self.policy.get(id) {
            Some(p) => Ok(p.clone()),
            None => {
                let dir = persistence::session_dir(id).context("cannot resolve session dir")?;
                persistence::load_policy(&dir).context("failed to load policy")
            }
        }
    }
}

/// Walk the supervisor chain starting from `supervisor_id`, resolving each
/// ancestor via the pre-loaded index (with transparent disk fallback on miss).
/// Returns nodes ordered from immediate supervisor to the root.
/// Fails on missing data or circular chains.
fn load_supervisor_chain(
    supervisor_id: &AgentId,
    index: &RestoreIndex,
) -> anyhow::Result<Vec<ChainNode>> {
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut current_id = supervisor_id.clone();

    loop {
        if !visited.insert(current_id.clone()) {
            anyhow::bail!("circular supervisor chain detected");
        }

        let meta = index
            .get_meta(&current_id)
            .context("incomplete supervisor chain")?;
        let policy = index
            .get_policy(&current_id)
            .context("cannot load supervisor policy")?;

        let parent_id = meta.created_by.clone();
        chain.push(ChainNode {
            agent_id: current_id,
            meta,
            policy,
        });

        match parent_id {
            Some(pid) => current_id = pid,
            None => break,
        }
    }

    Ok(chain)
}

/// Compute remaining delegation depth from a pre-loaded supervisor chain.
fn validate_depth_from_chain(
    workspace_root: &std::path::Path,
    chain: &[ChainNode],
) -> anyhow::Result<u8> {
    let supervisor = chain.first().context("subagent has no supervisor chain")?;

    if !workspace_root.starts_with(&supervisor.meta.workspace_root) {
        anyhow::bail!("workspace outside supervisor boundary");
    }

    let chain_depth = u8::try_from(chain.len()).unwrap_or(u8::MAX);
    Ok(just_agent_runtime::config::DEFAULT_MAX_DEPTH.saturating_sub(chain_depth))
}

/// Validate that `policy` is at least as strict as every ancestor's policy
/// in the pre-loaded chain. Checks adjacent-pair monotonicity to match the
/// original recursive semantics.
fn validate_policy_from_chain(
    agent_id: &AgentId,
    policy: &ToolPolicy,
    chain: &[ChainNode],
) -> anyhow::Result<()> {
    // Agent's policy must be >= immediate supervisor's policy.
    if let Some(supervisor) = chain.first() {
        policy
            .validate_at_least_as_strict_as(&supervisor.policy)
            .map_err(|violations| {
                anyhow::anyhow!(
                    "agent {agent_id}: policy is less strict than supervisor: {}",
                    violations.join("; ")
                )
            })?;
    }

    // Chain monotonicity: each ancestor's policy must be >= its own supervisor's.
    for window in chain.windows(2) {
        window[0]
            .policy
            .validate_at_least_as_strict_as(&window[1].policy)
            .map_err(|violations| {
                anyhow::anyhow!(
                    "agent {}: policy is less strict than supervisor: {}",
                    window[0].agent_id,
                    violations.join("; ")
                )
            })?;
    }

    Ok(())
}

/// Restore a single persisted session to a running agent.
async fn restore_one(
    p: persistence::PendingRestore,
    shutdown: CancellationToken,
    shared_state: SharedState,
    index: &RestoreIndex,
) -> anyhow::Result<(AgentId, String, Agent)> {
    let sess = persistence::restore_session(&p.agent_id, &p.session_dir)?;

    let mut config = AgentConfig::load(None, vec![], Some(p.meta.workspace_root.clone()))?;
    config.agent_id = Some(p.agent_id.clone());
    config.created_by = p.meta.created_by.clone();

    let tool_policy = index
        .get_policy(&p.agent_id)
        .context("failed to load policy")?;

    if let Some(ref supervisor_id) = p.meta.created_by {
        let chain = load_supervisor_chain(supervisor_id, index)?;
        config.permissions.max_depth = validate_depth_from_chain(&p.meta.workspace_root, &chain)?;
        validate_policy_from_chain(&p.agent_id, &tool_policy, &chain)?;
    }

    let store = Arc::new(tokio::sync::Mutex::new(sess.store));
    let deferred = Arc::new(tokio::sync::Mutex::new(sess.deferred));
    let (events_tx, _) = broadcast::channel(256);

    let auth_token = uuid::Uuid::new_v4().to_string();
    let mut env = HashMap::new();
    env.insert("JUST_AGENT_ID".into(), p.agent_id.to_string());
    env.insert("JUST_AGENT_AUTH_TOKEN".into(), auth_token.clone());

    let tool_policy = Arc::new(std::sync::RwLock::new(tool_policy));

    let agent = spawn_agent(SpawnArgs {
        store,
        deferred,
        session_dir: sess.session_dir,
        config,
        initial_prompt: None,
        shutdown_cancel: shutdown,
        events_tx,
        auth_token: auth_token.clone(),
        env,
        shared_state,
        tool_policy,
    })
    .await?;

    Ok((sess.agent_id, auth_token, agent))
}

/// Restore persisted sessions top-down, level by level.
///
/// Root agents (no supervisor) are restored first, then their children, and
/// so on.  Siblings within each level are restored concurrently.  If an agent
/// fails to restore, its entire subtree is skipped — no orphans are created.
pub async fn restore_sessions(state: &SharedState) {
    let pending = persistence::scan_sessions();
    if pending.is_empty() {
        return;
    }

    // Use "agents" in logs to avoid confusing users with the internal "session" concept.
    info!(count = pending.len(), "restoring agents");

    // Build index: meta from scan, policy loaded once per session.
    let mut meta_map = HashMap::new();
    let mut policy_map = HashMap::new();
    for p in &pending {
        meta_map.insert(p.agent_id.clone(), p.meta.clone());
        if let Ok(policy) = persistence::load_policy(&p.session_dir) {
            policy_map.insert(p.agent_id.clone(), policy);
        }
    }
    let index = RestoreIndex {
        meta: meta_map,
        policy: policy_map,
    };

    // Build restore tree from created_by relationships.
    let pending_set: HashSet<AgentId> = pending.iter().map(|p| p.agent_id.clone()).collect();
    let mut pending_map: HashMap<AgentId, persistence::PendingRestore> = pending
        .into_iter()
        .map(|p| (p.agent_id.clone(), p))
        .collect();

    let mut children_of: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
    let mut roots = Vec::new();
    let mut direct_skips = Vec::new();

    for (id, p) in &pending_map {
        match &p.meta.created_by {
            None => {
                roots.push(id.clone());
            }
            Some(supervisor_id) if pending_set.contains(supervisor_id) => {
                children_of
                    .entry(supervisor_id.clone())
                    .or_default()
                    .push(id.clone());
            }
            Some(supervisor_id) => {
                // Supervisor not in restore set (crash-loop or deleted).
                // This agent and its descendants will not be restored.
                tracing::error!(
                    id = %id,
                    supervisor = %supervisor_id,
                    "skipping agent: supervisor not in restore set"
                );
                direct_skips.push(id.clone());
            }
        }
    }

    // Remove directly-skipped agents so the post-BFS pass does not double-log.
    for id in &direct_skips {
        pending_map.remove(id);
    }

    // Deterministic ordering within each level.
    roots.sort();

    // Level-by-level BFS restore.  Siblings within each level are restored
    // concurrently; children are only queued after their parent succeeds.
    let mut current_level = roots;
    while !current_level.is_empty() {
        // Take ownership of PendingRestores for this level.
        let tasks: Vec<(AgentId, persistence::PendingRestore)> = current_level
            .iter()
            .filter_map(|id| pending_map.remove(id).map(|p| (id.clone(), p)))
            .collect();

        // Restore all siblings concurrently.
        type RestoreOutcome = (AgentId, String, Agent);
        let results: Vec<(AgentId, anyhow::Result<RestoreOutcome>)> =
            futures_util::future::join_all(tasks.into_iter().map(|(id, p)| async {
                let result = restore_one(p, state.shutdown.clone(), state.clone(), &index).await;
                (id, result)
            }))
            .await;

        // Batch-register successes under a single lock, collect children.
        let mut next_level = Vec::new();
        let mut successes = Vec::new();
        for (id, result) in results {
            match result {
                Ok((registered_id, auth_token, agent)) => {
                    successes.push((registered_id, auth_token, agent));
                    if let Some(children) = children_of.get(&id) {
                        next_level.extend(children.iter().cloned());
                    }
                    info!(id = %id, "restored agent");
                }
                Err(e) => {
                    // Subtree is implicitly pruned — children not queued.
                    tracing::error!(id = %id, "restore failed: {e:#}");
                }
            }
        }

        if !successes.is_empty() {
            let mut registry = state.registry.write().await;
            for (id, auth_token, agent) in successes {
                registry.register(
                    id,
                    auth_token,
                    AgentEntry {
                        agent,
                        subagent_ids: vec![],
                    },
                );
            }
        }

        next_level.sort();
        current_level = next_level;
    }

    // Log transitively skipped agents (ancestors failed or cycles).
    for (id, p) in &pending_map {
        tracing::error!(
            id = %id,
            supervisor = ?p.meta.created_by,
            "skipping agent: ancestor was not restored"
        );
    }
}
