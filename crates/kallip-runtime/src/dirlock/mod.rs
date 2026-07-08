//! Directory write-lock coordinator (`DirLockManager`).
//!
//! Provides **write mutual exclusion** on filesystem directories across agents
//! running in the same daemon: at most one agent may hold the write-lock on a
//! given canonical directory at a time. Locks are held at the agent-session
//! granularity — they persist across many `bash_exec` commands and are released
//! only explicitly ([`DirLockManager::release`]) or when the agent task dies
//! ([`DirLockManager::release_all`]).
//!
//! This is the *advisory* coordination layer. Mandatory enforcement — making a
//! `bash` process physically unable to write a directory its agent has not
//! locked — is layered on top in `kallip-shell` (behind its `landlock`
//! feature) via landlock, which derives each command's writable set from
//! [`DirLockManager::write_paths`].
//!
//! # Delegation carve-out
//!
//! Locks form a *path* tree (hierarchical exclusion — see [`DirLockManager::acquire`]),
//! but the agent hierarchy is a *delegation* tree (`created_by`). A child agent
//! that locks a sub-directory of its supervisor's lock is **delegating**, not
//! conflicting: [`DirLockManager::acquire`] takes the caller's delegation
//! ancestor chain and allows a nested lock held under an ancestor. The carve-out
//! is realized automatically — the child's new lock appears in the ancestor's
//! [`readonly_paths`](DirLockManager::readonly_paths) view, which the landlock
//! layer bind-mounts read-only over the ancestor's otherwise-writable tree, so
//! both agents retain write to their own regions without overlap. Only the
//! ancestor direction is relaxed; a parent can never widen over a region its
//! child has locked.
//!
//! # Why a synchronous (`std::sync`) mutex
//!
//! The landlock enforcement layer reads [`DirLockManager::write_paths`] from a
//! **sync** closure (it runs in the spawn path). Every operation here is brief
//! (a `BTreeMap` lookup plus at most one `canonicalize` syscall) and never
//! `.await`s, so a `std::sync::Mutex` keeps the API synchronous and lets both
//! the HTTP handlers and the landlock closure call it directly.
//!
//! # Contention model
//!
//! [`DirLockManager::acquire`] never blocks indefinitely: it returns
//! [`AcquireOutcome::Busy`] naming the current holder so the caller (typically an
//! agent running `kallip dirlock acquire` through `bash_exec`) can resolve the
//! conflict by **inter-agent negotiation** — peer-messaging the holder. There is
//! deliberately no idle-timeout or max-hold watchdog; a forgotten lock is a
//! social problem, resolved socially.
//!
//! # Invariants
//!
//! - **A Normal agent holds a write-lock on its `workspace_root` for the
//!   lifetime of its task.** This is what makes the workspace appear in
//!   [`DirLockManager::write_paths`] and thus in the landlock writable set. It is
//!   acquired on *every* materialization path (create, restore, reactivation),
//!   not just create, so the workspace stays writable across daemon restarts and
//!   same-workspace mutual exclusion holds after a restore. The acquire is
//!   centralized in `kallip-daemon`'s `try_acquire_workspace_lock`; this module
//!   only provides the mechanism.
//! - **Release is coupled to task death, not registry removal.** Reactivation
//!   and `abort_agent` re-spawn or tear down an agent *without* removing its
//!   registry entry, so [`DirLockManager::release_all`] must be called explicitly
//!   on those paths (and reactivation then re-acquires the workspace lock above).
//! - **Lock order: registry always before lock-manager.** Callers that hold the
//!   agent-registry lock may then touch this manager; never the reverse. Methods
//!   here take only the manager's own mutex and return bare [`AgentId`]s, so the
//!   registry is never read while the manager mutex is held (enriching a holder
//!   with role/description must happen after dropping the manager lock).

use std::collections::BTreeMap;
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use kallip_common::AgentId;

/// Per-agent cap on simultaneously held write-locks, to bound griefing (a buggy
/// agent acquiring every directory it can name). The cap is advisory — there is
/// no global cap; operator intervention is the backstop.
const MAX_LOCKS_PER_AGENT: usize = 8;

/// Outcome of [`DirLockManager::acquire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// The caller now holds the write-lock on the directory.
    Acquired,
    /// The caller already held a write-lock on exactly this path (idempotent).
    AlreadyHeld,
    /// Another agent holds an **overlapping** write-lock — either the exact path
    /// or an ancestor/descendant of it (landlock enforcement is recursive, so the
    /// coordinator must exclude any overlap). `conflict` is the **canonical** path
    /// of the holder's lock (never equal to the requested path in the nested case),
    /// so the blocked agent knows which lock — and which holder — to negotiate with.
    Busy { holder: AgentId, conflict: PathBuf },
}

/// Internal per-directory state. v1 tracks only the writer; readonly isolation
/// for peer-locked dirs is NOT a per-dir readers set here — it is derived as the
/// complement of an agent's write-locks ([`DirLockManager::readonly_paths`]) and
/// realized externally by the mount-ns layer. A readers-counted lock is not
/// planned.
#[derive(Default)]
struct DirState {
    writer: Option<AgentId>,
}

/// Directory write-lock coordinator.
///
/// Paths are canonicalized immediately before entering the critical section
/// (callers pass the raw path); the key in the map is the canonical path, so the
/// canonicalize→lock window only affects which path string is matched, not the
/// in-memory exclusivity check.
///
/// **Hierarchical exclusion.** Locks form a tree: a write-lock on a directory
/// excludes any overlapping write-lock (ancestor **or** descendant) held by
/// another agent. This matches landlock's recursive `PathBeneath` enforcement, so
/// no two agents' writable sets ever overlap. Ancestor conflicts are found by
/// walking [`Path::ancestors`] (O(depth)); descendant conflicts by an ordered
/// `BTreeMap` range scan (O(log n + matches)) — both component-wise, so `/a/b`
/// never false-matches `/a/bb`.
pub struct DirLockManager {
    /// canonical dir → state. `BTreeMap` (not `HashMap`) so a descendant conflict
    /// check is an ordered range scan. Guarded by `lock`.
    dirs: Mutex<BTreeMap<PathBuf, DirState>>,
}

/// Acquire the inner mutex, tolerating poisoning — a panic in one critical
/// section must not freeze unrelated agents' locks (mirrors the registry's own
/// poison-tolerance convention).
fn locked(
    dirs: &Mutex<BTreeMap<PathBuf, DirState>>,
) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, DirState>> {
    dirs.lock().unwrap_or_else(|e| e.into_inner())
}

impl DirLockManager {
    pub fn new() -> Self {
        Self {
            dirs: Mutex::new(BTreeMap::new()),
        }
    }

    /// Attempt to acquire the write-lock on `path` for `agent`.
    ///
    /// `chain` is the caller's strict delegation ancestors (the `created_by`
    /// chain above `agent`, root-bound). A lock held by one of those ancestors
    /// is **delegation, not conflict**: the nested acquire is allowed, and the
    /// carve-out machinery realizes mutual exclusion by surfacing the new lock
    /// in the ancestor's [`readonly_paths`](DirLockManager::readonly_paths)
    /// view (bind-mounted read-only over the ancestor's writable tree). Pass an
    /// empty slice for a root agent.
    ///
    /// Returns:
    /// - [`AcquireOutcome::Acquired`] — the caller now holds the lock.
    /// - [`AcquireOutcome::AlreadyHeld`] — the caller already held a lock on
    ///   exactly `path` (idempotent).
    /// - [`AcquireOutcome::Busy { holder, conflict }`] — another agent holds an
    ///   overlapping lock (exact path, an ancestor, or a descendant), and that
    ///   agent is not a delegation ancestor of the caller. `conflict` is the
    ///   holder's canonical lock path, for negotiation.
    /// - `Err` — the path cannot be canonicalized (create-then-acquire); the
    ///   per-agent cap would be exceeded; **or** the path overlaps a lock the
    ///   caller itself already holds (release that lock first to change scope —
    ///   acquiring a nested or wider lock is never silently absorbed, so an agent
    ///   can't widen `/a/b/c` to `/a/b` and end up with a narrower lock than it
    ///   intended).
    pub fn acquire(
        &self,
        agent: &AgentId,
        path: &Path,
        chain: &[AgentId],
    ) -> io::Result<AcquireOutcome> {
        let canon = canonicalize(path)?;
        let mut dirs = locked(&self.dirs);

        // Exact-path: idempotent for self, conflict for another agent. (An
        // ancestor holding the *exact* same path is not delegation — two agents
        // writing the same directory is never desired — so this stays `Busy`
        // regardless of `chain`.)
        match dirs.get(&canon) {
            Some(state) if state.writer.as_ref() == Some(agent) => {
                return Ok(AcquireOutcome::AlreadyHeld);
            }
            Some(state) if state.writer.is_some() => {
                return Ok(AcquireOutcome::Busy {
                    holder: state.writer.clone().unwrap(),
                    conflict: canon.clone(),
                });
            }
            _ => {}
        }

        // Hierarchical: any overlapping ancestor or descendant lock held by
        // another agent blocks; held by self is a self-overlap error. An
        // *ancestor* lock held by a delegation ancestor (`chain`) is allowed —
        // see `overlap_conflict`.
        //
        // SAFETY(lock-order): `chain` is computed by the caller while holding
        // the registry read lock; this method takes only `self.dirs` and never
        // touches the registry, preserving the registry-before-manager order.
        if let Some(res) = overlap_conflict(&dirs, &canon, agent, chain) {
            return res;
        }

        // Enforce the per-agent cap before granting a *new* lock (re-acquiring an
        // already-held dir is AlreadyHeld, handled above).
        if count_held(&dirs, agent) >= MAX_LOCKS_PER_AGENT {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "lock cap reached: an agent may hold at most {MAX_LOCKS_PER_AGENT} \
                     directory write-locks; release one before acquiring another"
                ),
            ));
        }

        dirs.entry(canon).or_default().writer = Some(agent.clone());
        Ok(AcquireOutcome::Acquired)
    }

    /// Release `agent`'s write-lock on `path`, if it holds one. Idempotent: a
    /// no-op if the agent did not hold it. A path that no longer exists (deleted
    /// out from under a holder) is also a no-op — a holder must never be stuck
    /// unable to shed a lock because its directory vanished.
    pub fn release(&self, agent: &AgentId, path: &Path) -> io::Result<()> {
        let Ok(canon) = canonicalize(path) else {
            return Ok(());
        };
        let mut dirs = locked(&self.dirs);
        if let Some(state) = dirs.get_mut(&canon)
            && state.writer.as_ref() == Some(agent)
        {
            state.writer = None;
        }
        // Drop empty entries so `holder`/`status` don't report idle dirs.
        dirs.retain(|_, s| s.writer.is_some());
        Ok(())
    }

    /// Release every lock held by `agent`. Called on task death (reactivation,
    /// `abort_agent`, shutdown drain) — not only registry removal.
    pub fn release_all(&self, agent: &AgentId) {
        let mut dirs = locked(&self.dirs);
        for state in dirs.values_mut() {
            if state.writer.as_ref() == Some(agent) {
                state.writer = None;
            }
        }
        dirs.retain(|_, s| s.writer.is_some());
    }

    /// Snapshot of the canonical directories `agent` currently holds a
    /// write-lock on — the writable set the landlock enforcement layer derives
    /// each command's domain from. Point-in-time; see the plan's "Known
    /// limitations" (one-command overlap after release).
    pub fn write_paths(&self, agent: &AgentId) -> io::Result<Vec<PathBuf>> {
        let dirs = locked(&self.dirs);
        Ok(dirs
            .iter()
            .filter(|(_, s)| s.writer.as_ref() == Some(agent))
            .map(|(p, _)| p.clone())
            .collect())
    }

    /// Snapshot of the canonical directories `agent` does NOT hold but some
    /// OTHER agent holds a write-lock on — the readonly-hole set (the DirLock
    /// reader view, §4.2). These are the paths a mount-ns layer bind-mounts
    /// read-only so no agent can mutate a workspace a peer has locked. Empty for
    /// an agent that holds every lock it touches. For a Guest (readonly: its
    /// landlock writable set is the skills carve only) these are redundant — a
    /// Guest cannot write peers' workspaces anyway — but harmless to compute.
    /// Point-in-time, complement of [`write_paths`](Self::write_paths).
    pub fn readonly_paths(&self, agent: &AgentId) -> io::Result<Vec<PathBuf>> {
        let dirs = locked(&self.dirs);
        Ok(dirs
            .iter()
            .filter(|(_, s)| s.writer.is_some() && s.writer.as_ref() != Some(agent))
            .map(|(p, _)| p.clone())
            .collect())
    }

    /// Who holds the write-lock on `path`, if anyone. Returns only the bare
    /// [`AgentId`] — enrichment (role/description) is the caller's job and must
    /// happen *after* this returns, to preserve the registry-before-manager lock
    /// order (this method never touches the registry).
    pub fn holder(&self, path: &Path) -> io::Result<Option<AgentId>> {
        let canon = canonicalize(path)?;
        let dirs = locked(&self.dirs);
        Ok(dirs.get(&canon).and_then(|s| s.writer.clone()))
    }
}

impl Default for DirLockManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Count how many write-locks `agent` currently holds. Caller holds the mutex.
fn count_held(dirs: &BTreeMap<PathBuf, DirState>, agent: &AgentId) -> usize {
    dirs.values()
        .filter(|s| s.writer.as_ref() == Some(agent))
        .count()
}

/// Check `canon` against existing locks for an ancestor/descendant overlap
/// (exact-path overlap is handled by the caller before this). Returns:
/// - `Some(Ok(Busy { holder, conflict }))` — another agent holds an overlapping
///   ancestor or descendant lock;
/// - `Some(Err)` — the *same* agent holds an overlapping lock (self-overlap:
///   release it first to change scope — nested or wider acquires are never
///   silently absorbed);
/// - `None` — no overlap, the acquire may proceed.
///
/// `chain` is the caller's delegation ancestors. An *ancestor* lock held by one
/// of them is **delegation, not conflict**: the loop skips it (`continue`) and
/// keeps walking upward so a higher-up non-delegation ancestor still blocks.
/// This is the only direction relaxed: a *descendant* lock held by a child is
/// never delegated through (a parent must not widen over a region it has
/// delegated and the child is actively writing).
///
/// Ancestors are walked via [`Path::ancestors`] (O(depth)); descendants via an
/// ordered `BTreeMap` range scan that stops at the first non-descendant
/// (component-wise `starts_with`, so `/a/b` does not match `/a/b0`). Caller
/// holds the mutex.
fn overlap_conflict(
    dirs: &BTreeMap<PathBuf, DirState>,
    canon: &Path,
    agent: &AgentId,
    chain: &[AgentId],
) -> Option<io::Result<AcquireOutcome>> {
    let blocked_by = |writer: &AgentId, conflict: PathBuf| -> io::Result<AcquireOutcome> {
        if writer == agent {
            Err(io::Error::other(format!(
                "{} overlaps your existing lock on {}; release it first to change scope",
                canon.display(),
                conflict.display(),
            )))
        } else {
            Ok(AcquireOutcome::Busy {
                holder: writer.clone(),
                conflict,
            })
        }
    };

    // Ancestors of `canon` (skip `canon` itself): exact `get` per ancestor.
    // Order matters: self-overlap must be judged before the delegation skip, so
    // a chain entry can never swallow a self-overlap error. A delegation
    // ancestor is skipped (`continue`) to keep scanning for a higher non-chain
    // conflict.
    for anc in canon.ancestors().skip(1) {
        if let Some(state) = dirs.get(anc)
            && let Some(writer) = &state.writer
        {
            if writer == agent {
                return Some(blocked_by(writer, anc.to_path_buf()));
            }
            if chain.iter().any(|a| a == writer) {
                continue;
            }
            return Some(blocked_by(writer, anc.to_path_buf()));
        }
    }
    // Descendants of `canon`: ordered range after `canon`, stop at the first key
    // that is not a component-wise descendant.
    for (k, state) in dirs.range::<Path, _>((Bound::Excluded(canon), Bound::Unbounded)) {
        if !k.starts_with(canon) {
            break;
        }
        if let Some(writer) = &state.writer {
            return Some(blocked_by(writer, k.clone()));
        }
    }
    None
}

/// Canonicalize `path`, mapping the not-found case to a clear error so callers
/// know to create-then-acquire rather than guess.
fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| {
        io::Error::other(format!(
            "cannot lock {path_display}: {e}. \
             directory write-locks require an existing path — create it first, \
             then acquire.",
            path_display = path.display()
        ))
    })
}

#[cfg(test)]
mod tests;
