//! Shared test helpers for tagma tests.
//!
//! This module is only compiled in test builds and provides utilities for
//! constructing agent entries and registries used across `state.rs`,
//! `bridge.rs`, and other test modules.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU8;

use ctor::ctor;
use kallip_common::agentid::AgentId;
use kallip_common::policy::{ExecPolicy, PolicyPreset};
use kallip_common::protocol::AgentState;
use kallip_runtime::approval::ApprovalStore;
use kallip_runtime::config::{AgentConfig, PermissionProfile};
use kallip_runtime::context::ContextStore;
use kallip_runtime::retry::RetryPolicy;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::state::{
    Agent, AgentEntry, AgentIdentity, AgentRegistry, AppState, FaultedEntry, RegistryEntry,
    SharedState,
};
use kallip_common::authtoken::TokenHash;

/// Construct a full `AgentEntry` with real channels, the default preset, and an
/// empty exec-policy.
pub fn make_entry(created_by: Option<AgentId>, auth_token: String) -> AgentEntry {
    make_entry_inner(
        created_by,
        auth_token,
        PolicyPreset::Default,
        ExecPolicy::default(),
    )
    .0
}

/// Like [`make_entry`], but returns the `prompt_rx` for capturing notifications.
pub fn make_entry_with_rx(
    created_by: Option<AgentId>,
    auth_token: String,
) -> (AgentEntry, mpsc::Receiver<String>) {
    make_entry_inner(
        created_by,
        auth_token,
        PolicyPreset::Default,
        ExecPolicy::default(),
    )
}

/// Like [`make_entry`], but installs a custom preset and exec-policy on the agent.
pub fn make_entry_with_policy(
    created_by: Option<AgentId>,
    auth_token: String,
    preset: PolicyPreset,
    exec_policy: ExecPolicy,
) -> AgentEntry {
    make_entry_inner(created_by, auth_token, preset, exec_policy).0
}

/// Like [`make_entry_with_policy`], but returns the `prompt_rx`.
pub fn make_entry_with_policy_rx(
    created_by: Option<AgentId>,
    auth_token: String,
    preset: PolicyPreset,
    exec_policy: ExecPolicy,
) -> (AgentEntry, mpsc::Receiver<String>) {
    make_entry_inner(created_by, auth_token, preset, exec_policy)
}

fn make_entry_inner(
    created_by: Option<AgentId>,
    auth_token: String,
    preset: PolicyPreset,
    exec_policy: ExecPolicy,
) -> (AgentEntry, mpsc::Receiver<String>) {
    let (prompt_tx, prompt_rx) = mpsc::channel(16);
    let (events_tx, _) = broadcast::channel(1);
    let config = AgentConfig {
        prompt: None,
        system_prompt: String::new(),
        max_tool_rounds: 1,
        max_heartbeat_rounds: 3,
        max_transient_retries: 3,
        workspace_root: PathBuf::from("/tmp"),
        context_window_tokens: 128_000,
        output_reserve_tokens: 8_192,
        summary_max_tokens: 1_200,
        tool_timeout_secs: 120,
        skills: vec![],
        retry_policy: RetryPolicy::default(),
        pinned_budget_ratio: 0.25,
        context_thresholds: vec![50, 80],
        token_budget_warnings: vec![80, 95],
        agent_id: None,
        created_by,
        permissions: PermissionProfile::new(PathBuf::from("/tmp")),
        profile_set: Some("default".into()),
        permissions_class: Default::default(),
        role: String::new(),
        description: String::new(),
        delegation_mode: kallip_runtime::config::DelegationMode::CarveOut,
    };
    let entry = AgentEntry {
        identity: AgentIdentity {
            config,
            agent_dir: None,
        },
        agent: Agent {
            prompt_tx,
            events_tx,
            approvals: Arc::new(Mutex::new(ApprovalStore::new())),
            agent_abort: tokio::spawn(async {}).abort_handle(),
            agent_watch: tokio::spawn(async {}),
            bridge_handle: tokio::spawn(async {}),
            store: Arc::new(Mutex::new(ContextStore::new())),
            cancel: CancellationToken::new(),
            round_cancel: Arc::new(std::sync::Mutex::new(None)),
            notify: Arc::new(tokio::sync::Notify::new()),
            state: Arc::new(AtomicU8::new(AgentState::IDLE)),
            state_since: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            activity: Arc::new(std::sync::Mutex::new(String::new())),
            parked: Arc::new(std::sync::Mutex::new(None)),
            retrying: Arc::new(std::sync::Mutex::new(None)),
            auth_token_hash: TokenHash::of(&auth_token),
            env: std::collections::HashMap::new(),
            preset,
            exec_policy: Arc::new(std::sync::RwLock::new(exec_policy)),
            exec_gate: kallip_runtime::ExecGate::new(),
            pending_profile_reset: Arc::new(std::sync::Mutex::new(None)),
            profile_snapshot: Arc::new(std::sync::Mutex::new(
                kallip_runtime::ProfileSnapshot::default(),
            )),
        },
        subagent_ids: vec![],
    };
    (entry, prompt_rx)
}

/// Construct a faulted entry (no running task) with the given reason.
pub fn make_faulted_entry(created_by: Option<AgentId>, reason: &str) -> FaultedEntry {
    let config = AgentConfig {
        created_by,
        workspace_root: PathBuf::from("/tmp"),
        ..AgentConfig::default()
    };
    FaultedEntry {
        identity: AgentIdentity {
            config,
            agent_dir: None,
        },
        subagent_ids: vec![],
        reason: reason.to_string(),
        at: 0,
    }
}

/// Register a root agent (no `created_by`).
///
/// Bypasses [`AgentRegistry::register_root`] (uses the raw inserter), so it can
/// build the intentionally-invalid multi-root states some registry-primitive
/// tests need (e.g. two unrelated roots to exercise peer/relation semantics).
/// Production code must never register a root this way.
pub fn add_root(registry: &mut AgentRegistry, id: &AgentId) {
    registry.register(
        id.clone(),
        RegistryEntry::Live(make_entry(None, format!("agent-{id}"))),
    );
}

/// Register a sub-agent under a supervisor.
pub fn add_sub(registry: &mut AgentRegistry, id: &AgentId, supervisor: &AgentId) {
    registry.register(
        id.clone(),
        RegistryEntry::Live(make_entry(Some(supervisor.clone()), format!("agent-{id}"))),
    );
}

/// Register a root agent with a custom preset and exec-policy.
pub fn add_root_with_policy(
    registry: &mut AgentRegistry,
    id: &AgentId,
    preset: PolicyPreset,
    exec_policy: ExecPolicy,
) {
    registry.register(
        id.clone(),
        RegistryEntry::Live(make_entry_with_policy(
            None,
            format!("agent-{id}"),
            preset,
            exec_policy,
        )),
    );
}

/// Register a faulted root agent with the given reason.
pub fn add_faulted_root(registry: &mut AgentRegistry, id: &AgentId, reason: &str) {
    registry.register(
        id.clone(),
        RegistryEntry::Faulted(make_faulted_entry(None, reason)),
    );
}

/// Register a faulted sub-agent under a supervisor.
pub fn add_faulted_sub(
    registry: &mut AgentRegistry,
    id: &AgentId,
    supervisor: &AgentId,
    reason: &str,
) {
    registry.register(
        id.clone(),
        RegistryEntry::Faulted(make_faulted_entry(Some(supervisor.clone()), reason)),
    );
}

/// Enqueue and commit an approval on the target agent, return the approval ID.
pub async fn enqueue_committed_approval(
    registry: &tokio::sync::RwLockReadGuard<'_, AgentRegistry>,
    agent_id: &AgentId,
    tool_name: &str,
    arguments: &str,
) -> String {
    let entry = registry.get(agent_id).expect("agent exists");
    let live = entry.as_live().expect("agent is live");
    let mut store = live.agent.approvals.lock().await;
    let id = store.enqueue(tool_name, arguments, None);
    store.commit(&id, "test commit").expect("commit");
    id
}

/// Minimal single-profile bundle for tests that need an `AppState` but won't
/// spawn real agents. No declared window (env-path semantics).
pub fn make_profile_bundle() -> Arc<arc_swap::ArcSwap<crate::state::ProfileBundle>> {
    ensure_test_data_dir();
    use just_llm_client::family;
    use kallip_runtime::profile::{Profile, ProfileConfig, ProfileRegistry, ProfileSet, Provider};
    use std::collections::{BTreeMap, HashMap};
    let mut endpoints = HashMap::new();
    endpoints.insert(
        "test".into(),
        Provider {
            id: "test".into(),
            family: family::DEEPSEEK.into(),
            api_key: "test".into(),
            base_url: None,
        },
    );
    let cfg = ProfileConfig {
        sets: BTreeMap::from([(
            "default".to_string(),
            ProfileSet {
                name: "default".into(),
                description: None,
                profiles: vec![Profile {
                    id: "test".into(),
                    endpoint: "test".into(),
                    model: "test".into(),
                    max_context_window: 128_000,
                    store: None,
                    effort: None,
                    modalities: Profile::default_modalities(),
                }],
            },
        )]),
        default: "default".into(),
        endpoints,
        parking: vec![],
    };
    let source = crate::backend::build_backends(
        &cfg,
        just_llm_client::client::BackendFactory::new(),
        crate::backend::DEFAULT_USER_AGENT,
    )
    .expect("test backends build");
    let registry =
        Arc::new(ProfileRegistry::new(cfg.sets.clone(), source).expect("valid test registry"));
    Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::state::ProfileBundle {
            config: cfg,
            registry,
        },
    ))
}

/// Two-set bundle (`default` + `alt` on a second endpoint) for set-management
/// tests — bind/default/remove all need a second set to move between.
pub fn make_profile_bundle_two_sets() -> Arc<arc_swap::ArcSwap<crate::state::ProfileBundle>> {
    ensure_test_data_dir();
    use just_llm_client::family;
    use kallip_runtime::profile::{Profile, ProfileConfig, ProfileRegistry, ProfileSet, Provider};
    use std::collections::{BTreeMap, HashMap};
    let mut endpoints = HashMap::new();
    endpoints.insert(
        "test".into(),
        Provider {
            id: "test".into(),
            family: family::DEEPSEEK.into(),
            api_key: "test".into(),
            base_url: None,
        },
    );
    endpoints.insert(
        "alt".into(),
        Provider {
            id: "alt".into(),
            family: family::DEEPSEEK.into(),
            api_key: "alt".into(),
            base_url: None,
        },
    );
    let set = |name: &str, ep: &str| {
        (
            name.to_string(),
            ProfileSet {
                name: name.into(),
                description: None,
                profiles: vec![Profile {
                    id: ep.into(),
                    endpoint: ep.into(),
                    model: ep.into(),
                    max_context_window: 128_000,
                    store: None,
                    effort: None,
                    modalities: Profile::default_modalities(),
                }],
            },
        )
    };
    let cfg = ProfileConfig {
        sets: BTreeMap::from([set("default", "test"), set("alt", "alt")]),
        default: "default".into(),
        endpoints,
        parking: vec![],
    };
    let source = crate::backend::build_backends(
        &cfg,
        just_llm_client::client::BackendFactory::new(),
        crate::backend::DEFAULT_USER_AGENT,
    )
    .expect("test backends build");
    let registry =
        Arc::new(ProfileRegistry::new(cfg.sets.clone(), source).expect("valid test registry"));
    Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::state::ProfileBundle {
            config: cfg,
            registry,
        },
    ))
}

/// Like [`make_state`], but over [`make_profile_bundle_two_sets`].
pub fn make_state_two_sets() -> SharedState {
    ensure_test_data_dir();
    let mut state = AppState::new_with_preset(
        TokenHash::of("op-token"),
        make_profile_bundle_two_sets(),
        PolicyPreset::Default,
    );
    pin_test_budget(&mut state);
    Arc::new(state)
}

/// Create a fresh `SharedState` (default preset) for testing. The operator token
/// plaintext is `"op-token"` (hashed into `AppState`); tests present it as a
/// bearer token.
pub fn make_state() -> SharedState {
    make_state_with_preset(PolicyPreset::Default)
}

/// Like [`make_state`], but with a custom tagma-global preset.
pub fn make_state_with_preset(preset: PolicyPreset) -> SharedState {
    ensure_test_data_dir();
    let mut state =
        AppState::new_with_preset(TokenHash::of("op-token"), make_profile_bundle(), preset);
    pin_test_budget(&mut state);
    Arc::new(state)
}

/// Like [`make_state`], but with a custom spawn entry (delivery's slow-path
/// test seam; see `AppState::spawn_fn`).
pub fn make_state_with_spawn(spawn_fn: crate::lifecycle::SpawnFn) -> SharedState {
    ensure_test_data_dir();
    let mut state = AppState::new_with_preset(
        TokenHash::of("op-token"),
        make_profile_bundle(),
        PolicyPreset::Default,
    );
    pin_test_budget(&mut state);
    state.spawn_fn = spawn_fn;
    Arc::new(state)
}

/// Like [`make_profile_bundle`], but the `default` set also serves
/// image — for tests exercising the image-ingest gates' happy path.
pub fn make_profile_bundle_image_set() -> Arc<arc_swap::ArcSwap<crate::state::ProfileBundle>> {
    ensure_test_data_dir();
    use just_llm_client::family;
    use kallip_common::protocol::Modality;
    use kallip_runtime::profile::{Profile, ProfileConfig, ProfileRegistry, ProfileSet, Provider};
    use std::collections::{BTreeMap, HashMap};
    let mut endpoints = HashMap::new();
    endpoints.insert(
        "test".into(),
        Provider {
            id: "test".into(),
            family: family::DEEPSEEK.into(),
            api_key: "test".into(),
            base_url: None,
        },
    );
    let cfg = ProfileConfig {
        sets: BTreeMap::from([(
            "default".to_string(),
            ProfileSet {
                name: "default".into(),
                description: None,
                profiles: vec![Profile {
                    id: "test".into(),
                    endpoint: "test".into(),
                    model: "test".into(),
                    max_context_window: 128_000,
                    store: None,
                    effort: None,
                    modalities: vec![Modality::Text, Modality::Image],
                }],
            },
        )]),
        default: "default".into(),
        endpoints,
        parking: vec![],
    };
    let source = crate::backend::build_backends(
        &cfg,
        just_llm_client::client::BackendFactory::new(),
        crate::backend::DEFAULT_USER_AGENT,
    )
    .expect("test backends build");
    let registry =
        Arc::new(ProfileRegistry::new(cfg.sets.clone(), source).expect("valid test registry"));
    Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::state::ProfileBundle {
            config: cfg,
            registry,
        },
    ))
}

/// Like [`make_state_with_preset`], but over [`make_profile_bundle_image_set`].
pub fn make_state_with_image_set() -> SharedState {
    ensure_test_data_dir();
    let mut state = AppState::new_with_preset(
        TokenHash::of("op-token"),
        make_profile_bundle_image_set(),
        PolicyPreset::Default,
    );
    pin_test_budget(&mut state);
    Arc::new(state)
}

/// Pin the tagma-wide budget for tests: a known finite starting point that
/// budget assertions can lean on (the tagma binary itself resolves
/// `KALLIP_TOKEN_BUDGET` at startup and passes the value in).
fn pin_test_budget(state: &mut AppState) {
    state.token_budget = kallip_runtime::token_budget::TokenBudget::new(1_000_000, 0);
}

/// Install an in-memory inbox store on a test `SharedState`.
/// Required for any test that exercises the off-duty duty gate.
pub async fn install_inbox_store(state: &SharedState) {
    state
        .inboxes
        .set(crate::inbox::InboxStore::open_in_memory().await)
        .ok();
}
/// Process-wide pinning of `KALLIP_TAGMA_SLUG` + `XDG_DATA_HOME` to name a throwaway slug-derived data root for the
/// whole test run. Idempotent: the first call allocates the tempdir and swaps
/// the env; later calls are no-ops.
///
/// Why: profile-handler tests reach `persist_config` →
/// `kallip_runtime::profile::config_path`, which resolves against the process
/// environment. `cargo test` inherits the caller's real `KALLIP_TAGMA_SLUG`/`XDG_DATA_HOME`, so
/// an unguarded persist overwrites the live profiles file (2026-09-02).
///
/// Parallel safety: `OnceLock::get_or_init` runs the initializer exactly once
/// and every other thread blocks until the env is swapped, so no test
/// observes a half-installed state. Every state/bundle constructor in this
/// module calls this first, and disk-persisting handler calls happen only
/// after those constructors return.
pub fn ensure_test_data_dir() {
    static GUARD: OnceLock<PathBuf> = OnceLock::new();
    let path = GUARD.get_or_init(|| {
        let tmp = tempfile::Builder::new()
            .prefix("kallip-tagma-test-data-")
            // Inside the managed root so the one deliberate
            // process-lifetime leak stays under operator cleanup.
            .tempdir_in({
                let root = std::env::temp_dir().join("kallipai-dev");
                std::fs::create_dir_all(&root).expect("create /tmp/kallipai-dev");
                root
            })
            .expect("create test data dir");
        let path = tmp.path().to_path_buf();
        // Leak the TempDir so the directory outlives every test in the
        // process; the env points at it for the process lifetime anyway.
        std::mem::forget(tmp);
        // SAFETY: runs exactly once per process under `OnceLock`; concurrent
        // `getenv` from threads that never call this helper is the same
        // theoretical race this crate's existing `temp_env` helper already
        // accepts (edition 2024 makes `set_var` unsafe globally). The trio
        // below names the instance, points the XDG data home at the leaked
        // dir, and points the config home at it too: declared config
        // (profiles.toml) resolves under `<config home>/kallipai/tagmata/test`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &path);
            std::env::set_var("KALLIP_TAGMA_SLUG", "test");
            std::env::set_var("XDG_DATA_HOME", &path);
        }
        path
    });
    // Mirror the host boot: point the runtime's instance roots at the
    // pinned identity, so runtime path resolution goes through the
    // injection (the runtime no longer reads the environment). Re-assert
    // on every call: the OnceLock only guards the tempdir allocation, and
    // disk-fixture tests re-point the roots per test — a once-only install
    // would leave them dangling on a deleted fixture dir.
    let leaf = path.join("kallipai").join("tagmata").join("test");
    kallip_runtime::persistence::set_instance_roots_for_tests(Some(
        kallip_runtime::persistence::InstanceRoots {
            data: leaf.clone(),
            config: leaf,
            state: path.join("state").join("kallipai"),
        },
    ));
}

/// Register a live agent bound to the `alt` set (the dangling-bindings
/// scenarios' subject). Returns the agent id.
pub(crate) async fn alt_bound_sub(state: &SharedState) -> AgentId {
    let sub = AgentId::random();
    let supervisor = AgentId::random();
    let (mut entry, rx) = make_entry_with_rx(Some(supervisor), format!("agent-{sub}"));
    entry.identity.config.profile_set = Some("alt".into());
    drop(rx);
    state
        .registry
        .write()
        .await
        .register(sub.clone(), RegistryEntry::Live(entry));
    sub
}
#[cfg(test)]
mod guard_tests {
    use super::*;

    /// Regression lock for the 2026-09-02 live-profiles pollution: the guard
    /// must point the config home (`XDG_CONFIG_HOME`, which the slug-derived
    /// config root hangs off) at a leaked tempdir, and a real handler persist
    /// must land there — never in the inherited (live) config dir.
    #[tokio::test]
    #[serial_test::serial]
    async fn data_dir_guard_redirects_handler_persist() {
        ensure_test_data_dir();
        let dir = std::env::var("XDG_CONFIG_HOME").expect("guard active");
        assert!(
            std::path::Path::new(&dir).starts_with(std::env::temp_dir()),
            "XDG_CONFIG_HOME must be a tempdir, got {dir}"
        );

        let state = make_state_two_sets();
        let _result = crate::routes::profiles::set_default_profile_set(
            axum::extract::State(state),
            crate::auth::AuthIdentity::test_new(crate::auth::Identity::Operator),
            axum::Json(kallip_common::protocol::SetDefaultRequest {
                default: "alt".into(),
            }),
        )
        .await
        .expect("default transfer succeeds");

        let persisted = std::path::Path::new(&dir)
            .join("kallipai")
            .join("tagmata")
            .join("test")
            .join("profiles")
            .join("profiles.toml");
        let body = std::fs::read_to_string(persisted).expect("written under guard dir");
        assert!(body.contains("default = \"alt\""));
    }
}

/// Install a process-wide default tracing subscriber before any test runs.
///
/// A callsite's interest is registered against whatever dispatcher is
/// current the first time any thread reaches it, and the result is cached
/// for the whole process. In the test binary, threads without a scoped
/// subscriber resolve to `Dispatch::none`, so a parallel test hitting a
/// callsite first caches `Interest::never` and that event becomes
/// undeliverable process-wide — even for tests holding their own scoped
/// subscriber (the flaky lock-evaporation WARN family). A global default
/// gives every first registration a real subscriber; per-test scoped
/// subscribers still take precedence on top of it. Output goes to
/// `io::sink`: this subscriber exists for interest registration, capture
/// stays with each test's own subscriber.
#[ctor(unsafe)]
fn install_global_test_subscriber() {
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .with_max_level(tracing::Level::WARN)
        .finish();
    // Unreachable in practice: nothing else sets a global default in the
    // test binary, and ctor runs once before any test thread exists.
    tracing::subscriber::set_global_default(subscriber).ok();
}
