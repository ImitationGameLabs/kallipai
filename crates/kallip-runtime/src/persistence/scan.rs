use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::layout::{agents_base, inactive_base};
use super::legacy::RESTART_MESSAGE;
use super::legacy::{migrate_legacy_to_split, strip_restart_turns};
use super::meta::AgentMeta;
use super::meta::read_meta_from_dir;
use super::store::{Degradation, DegradationKind, load_store};
use crate::approval::ApprovalStore;
use crate::context::ContextStore;
use just_llm_client::types::generation::Message;
use kallip_common::AgentId;

// ---------------------------------------------------------------------------
// Restore (read path)
// ---------------------------------------------------------------------------

/// Lightweight handle produced by scanning the agents directory.
pub struct PendingRestore {
    pub agent_id: AgentId,
    pub agent_dir: PathBuf,
    pub meta: AgentMeta,
}
/// An agent directory whose meta.json could not be read or parsed at scan
/// time. The tagma registers these as faulted (visible and manageable) so
/// unreadable agents never silently vanish from the fleet.
pub struct RefusedRestore {
    pub agent_id: AgentId,
    pub agent_dir: PathBuf,
    pub error: String,
}

/// An agent fully deserialized and ready to resume.
pub struct RestorableAgent {
    pub agent_id: AgentId,
    pub agent_dir: PathBuf,
    pub store: ContextStore,
    pub approvals: ApprovalStore,
    /// Non-fatal damage absorbed during restore (missing turns, skipped
    /// corrupt lines). Empty on a clean restore. Consumed by the caller for
    /// structured warning logs.
    pub degraded: Vec<Degradation>,
}

/// List the agents directory, mapping "missing" (a fresh install) to `None`.
/// Any other read failure is an error: an unreadable agents directory must
/// not be mistaken for an empty fleet, or a second root gets minted over
/// the unreadable one.
fn read_agents_dir() -> Result<Option<fs::ReadDir>> {
    let base = agents_base().context("cannot resolve agents base")?;
    match fs::read_dir(&base) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!(
            "cannot read agents directory at {}",
            base.display()
        ))),
    }
}

/// Find the on-disk root agent: a directory whose meta.json has no `created_by`.
/// Returns the first hit (a second root is a legacy/corrupt state the tagma's
/// restore already refuses), `None` when the data dir holds no root at all.
/// Meta-read failures are skipped: an unreadable root surfaces through
/// [`scan_agents`] as a refused restore instead. A directory that exists
/// but cannot be read is an error, not "no root" -- callers must refuse
/// to mint rather than guess.
pub fn find_disk_root() -> Result<Option<AgentId>> {
    let entries = match read_agents_dir()? {
        Some(entries) => entries,
        None => return Ok(None),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_root = read_meta_from_dir(&path)
            .map(|meta| meta.created_by.is_none())
            .unwrap_or(false);
        if is_root {
            return Ok(path
                .file_name()
                .map(|n| AgentId::from(n.to_string_lossy().into_owned())));
        }
    }
    Ok(None)
}

/// Scan the agents directory and return agents eligible for restore.
///
/// Reads only `meta.json` per agent (lightweight). Directories whose meta is
/// unreadable are returned as [`RefusedRestore`] so the caller can surface
/// them as faulted agents instead of dropping them from view. An agents
/// directory that exists but cannot be listed is an error rather than an
/// empty scan -- see `read_agents_dir`.
pub fn scan_agents() -> Result<(Vec<PendingRestore>, Vec<RefusedRestore>)> {
    let entries = match read_agents_dir()? {
        Some(entries) => entries,
        None => return Ok((Vec::new(), Vec::new())),
    };

    let mut pending = Vec::new();
    let mut refused = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let agent_id = match path
            .file_name()
            .map(|n| AgentId::from(n.to_string_lossy().into_owned()))
        {
            Some(id) => id,
            None => continue,
        };
        match read_meta_from_dir(&path) {
            Ok(meta) => pending.push(PendingRestore {
                agent_id,
                agent_dir: path,
                meta,
            }),
            Err(e) => {
                tracing::warn!(id = %agent_id, "agent directory unreadable: {e:#}");
                refused.push(RefusedRestore {
                    agent_id,
                    agent_dir: path,
                    error: format!("{e:#}"),
                });
            }
        }
    }
    Ok((pending, refused))
}

/// Scan the inactive area for parked bodies: the declarative-team
/// lock-rebuild substrate. Each entry is (agent id, on-disk metadata);
/// unreadable directories are skipped with a warning (a parked body
/// whose meta cannot be read cannot be re-bound by role anyway).
/// Mirrors [`scan_agents`] over the inactive base — the live scan
/// deliberately never enters this area, keeping the two areas disjoint.
pub fn scan_inactive() -> Result<Vec<(AgentId, AgentMeta)>> {
    let base = inactive_base()?;
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "cannot list the inactive area {}: {err}",
                base.display()
            ));
        }
    };
    let mut parked = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let agent_id = match path
            .file_name()
            .map(|n| AgentId::from(n.to_string_lossy().into_owned()))
        {
            Some(id) => id,
            None => continue,
        };
        match read_meta_from_dir(&path) {
            Ok(meta) => parked.push((agent_id, meta)),
            Err(e) => {
                tracing::warn!(id = %agent_id, "inactive directory unreadable: {e:#}");
            }
        }
    }
    Ok(parked)
}

/// Deserialize a single agent from its directory.
/// Loads the context via split persistence when `manifest.json` is present
/// (hydrating the conversation window from history by turn ID), otherwise
/// through the legacy `context.json` path — which then migrates in place to
/// the split format. Either way the on-load migrations, token re-estimation,
/// and restart-message injection still run. Non-fatal damage is collected in
/// `degraded` rather than failing the restore. `tail_budget` caps the
/// conversation window rebuilt from the history tail when manifest.json and
/// its backup are both unreadable (see [`DegradationKind::TailRecovery`]).
pub fn restore_agent(
    agent_id: &AgentId,
    dir: &Path,
    tail_budget: usize,
) -> Result<RestorableAgent> {
    let mut degraded = Vec::new();
    let (mut store, migrate_pending) = load_store(dir, tail_budget, &mut degraded)?;

    let approvals: ApprovalStore = match fs::read_to_string(dir.join("approvals.json")) {
        Ok(json) => serde_json::from_str(&json).context("parsing approvals.json")?,
        Err(_) => ApprovalStore::new(),
    };

    // Fold the legacy `pinned` vec (pre-unification format) into pinned turns at the front of
    // `turns`. No-op for new-format stores.
    store.migrate_legacy_pinned();

    // Migrate legacy summary field to a pinned turn.
    store.migrate_legacy_summary();

    // Tool-call/result pairing is guaranteed at record time
    // (`tool_execution::synthesize_unanswered_results`); damage persisted before that
    // guarantee (or written by hand) surfaces here as degradations. Report-only:
    // repairing is a separate explicit operation, never something a restore does
    // silently.
    check_tool_pairing(&store, &mut degraded);
    // Recompute every cached token estimate via the current estimator, so persisted estimates
    // (possibly from a prior estimator version, e.g. the old char/4 heuristic or stale legacy
    // pins) are brought up to date. Idempotent.
    store.reestimate_cached_tokens();

    // A restore may follow an agent-version upgrade that changed the system prompt or tool set,
    // invalidating the persisted `last_prompt_tokens` anchor. Force a full estimate on the first
    // post-restore round so the gate recomputes from the current config rather than trusting a
    // cross-version anchor. (migrate/pin/unpin above also set the flag; this is the canonical
    // restore statement and covers the clean-restore case.) See ContextStore::needs_full_estimate.
    store.mark_needs_full_estimate();
    // Deferred legacy→split migration: load_store only reads the legacy
    // document. Writing the split files here, after the folds above, is
    // what makes them carry the folded pins and summary instead of the
    // raw legacy shape.
    if migrate_pending {
        migrate_legacy_to_split(dir, &store)?;
    }
    // Drop any restart notice carried over from a prior restore (legacy
    // stores via `load_store`, split manifests written by older binaries
    // whose projection did not exclude injected turns) so each restore
    // leaves exactly one fresh notice below.
    strip_restart_turns(&mut store);

    let restart_msgs = vec![Message::user(RESTART_MESSAGE)];
    let (restart_id, estimated_tokens) = store.push_turn(restart_msgs.clone());
    // The restart notice is an on-the-spot prompt with no history record;
    // keep its ID out of the manifest projection (see `injected_turn_ids`).
    store.register_injected_turn(restart_id);

    // Record agent restore event in history.
    // Uses direct HistoryWriter (no AgentContext exists at restore time).
    {
        let history = crate::history::HistoryWriter::new(dir.to_owned());
        if let Err(e) = history.append(
            None,
            &restart_msgs,
            estimated_tokens,
            crate::history::RecordKind::System,
            Some(crate::history::SystemEvent::AgentRestore),
            &[],
        ) {
            tracing::warn!("history restore record failed: {e:#}");
        }
    }

    Ok(RestorableAgent {
        agent_id: agent_id.clone(),
        agent_dir: dir.to_owned(),
        store,
        approvals,
        degraded,
    })
}

/// Validate tool-call/result pairing across every turn and record violations
/// as degradations — at most one finding per turn per direction, naming the
/// turn and the offending ids. Report-only by design: a dangling call would
/// be rejected by the provider on the next round (which is what the warning
/// buys time to notice), and the explicit repair operation —
/// [`repair_agent_context`], not restore — is what rewrites history to
/// fix it. Pinned turns carry no tool traffic, so they pass through clean.
fn check_tool_pairing(store: &ContextStore, degraded: &mut Vec<Degradation>) {
    for turn in store.turns() {
        let unanswered = crate::tool_execution::unanswered_call_ids(&turn.messages);
        if !unanswered.is_empty() {
            let ids: Vec<&str> = unanswered.iter().map(|(id, _)| id.as_str()).collect();
            degraded.push(Degradation {
                kind: DegradationKind::UnansweredToolCalls,
                detail: format!(
                    "turn {} declares tool calls with no result: {ids:?}",
                    turn.id.0
                ),
            });
        }
        let orphan = crate::tool_execution::orphan_result_ids(&turn.messages);
        if !orphan.is_empty() {
            degraded.push(Degradation {
                kind: DegradationKind::OrphanToolResults,
                detail: format!(
                    "turn {} carries tool results answering no call: {orphan:?}",
                    turn.id.0
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests;
