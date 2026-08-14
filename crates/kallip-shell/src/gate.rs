//! Per-agent execution gate: synchronizes a workspace carve-out against the
//! owning agent's in-flight shell forks.
//!
//! Spawning a subagent narrows this agent's writable set (a "carve-out"). But
//! filesystem enforcement is per-shell-fork: each spawned `bash` snapshots the
//! current lock state and bakes it into a landlock domain at fork. Without
//! coordination, a fork racing the carve-out could snapshot the *pre-carve*
//! (broader) writable set and keep writing the carved-out region.
//!
//! [`ExecGate`] closes that race. It wraps a `tokio::sync::RwLock<()>` held
//! READ across each shell fork (foreground `exec` snapshot+fork; background
//! spawn snapshot+fork) and taken WRITE -- non-blocking, via
//! [`ExecGate::try_write`] -- by the carve-out. **The RwLock is load-bearing**:
//! WRITE blocks new forks for the carve's critical section, so no fork can
//! snapshot stale state. A plain counter cannot express "block new forks for
//! the duration"; it would only detect forks already in flight, reopening the
//! snapshot TOCTOU.
//!
//! A running-background counter tallies live background tasks. Unlike the foreground
//! (one-shot, bounded by a timeout), a background task is long-lived and cannot
//! hold the permit READ for its whole life (a long build would deadlock every
//! carve). So backgrounds hold READ only across snapshot+fork; their lifetime is
//! tracked by the counter, and a carve refuses ([`ExecGateFailure::BgTasksRunning`])
//! while any background task runs.
//!
//! The gate is shared (as `Arc`) between the shell backend (READ side) and the
//! tagma (WRITE side, reached via `Agent.exec_gate`), so the two sides coordinate
//! without the tagma holding a reference to the backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::RwLock;

/// Per-agent execution gate. See the module docs for the invariant it upholds.
#[derive(Debug)]
pub struct ExecGate {
    /// Carve-out mutual exclusion. READ across each shell fork; WRITE across the
    /// carve. `tokio::sync` because the READ guard spans `.await` in `exec`.
    permit: RwLock<()>,
    /// Number of currently-running background tasks, each retaining launch-time
    /// writability for its whole life. A carve refuses while this is > 0.
    running_bg: AtomicU64,
    /// Monotonic count of carves that landed (acquired the WRITE side and
    /// shaped a new writable set). A foreground exec records the epoch at
    /// its fork; when a timeout converts its still-running child into a
    /// background task (an unbounded life), the adoption re-checks the
    /// epoch under a fresh READ — a mismatch means a carve landed in
    /// between, the child's baked landlock domain is older than the
    /// carve's access decision, and the child is killed instead. This
    /// closes what would otherwise be the conversion's one unbounded
    /// escape: a long-lived task running a pre-carve (broader) domain.
    carve_epoch: AtomicU64,
}

/// Why a carve-out could not take the WRITE side of an [`ExecGate`]. Callers map
/// this to their own policy (the tagma returns 409 on create, faults on restore).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecGateFailure {
    /// A foreground `exec` is in flight (holds the READ side).
    ForegroundExecInProgress,
    /// `n` background tasks are running (each retains launch-time writability
    /// for its whole life, so a carve cannot proceed until they finish).
    BgTasksRunning(u64),
}

impl ExecGate {
    /// Mint a fresh per-agent gate, wrapped in `Arc` for sharing between the
    /// backend (READ) and the tagma (WRITE).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            permit: RwLock::new(()),
            running_bg: AtomicU64::new(0),
            carve_epoch: AtomicU64::new(0),
        })
    }

    /// Take the READ side for the lifetime of a shell fork. A no-op guard when
    /// `gate` is `None` (a backend built without coordination, e.g. unit tests),
    /// so callers need not branch on the gate's presence.
    pub async fn read(gate: &Option<Arc<Self>>) -> ExecReadGuard<'_> {
        ExecReadGuard(match gate {
            Some(g) => Some(g.permit.read().await),
            None => None,
        })
    }

    /// Attempt the WRITE side (non-blocking). Returns a guard on success, or the
    /// reason the carve cannot proceed. Never blocks -> deadlock-free: a carve
    /// that would contend simply refuses and the caller retries later.
    ///
    /// The WRITE permit is taken FIRST and the background counter re-checked
    /// under it. The two must not straddle a background `inc`: once WRITE is
    /// held no new fork can start (READ is blocked, and `inc_bg` runs under the
    /// READ permit -- see `BackgroundRegistry::spawn`), so the counter is stable
    /// here. Checking it before the permit would let a background task inc
    /// between the check and a successful `try_write`, admitting a carve while a
    /// counted background task is alive.
    pub fn try_write(&self) -> Result<ExecWriteGuard<'_>, ExecGateFailure> {
        let guard = match self.permit.try_write() {
            Ok(g) => g,
            // `try_write` fails if any READ is held or another WRITE is held --
            // either way a foreground fork is in progress.
            Err(_) => return Err(ExecGateFailure::ForegroundExecInProgress),
        };
        let bg = self.running_bg.load(Ordering::Relaxed);
        if bg > 0 {
            drop(guard);
            return Err(ExecGateFailure::BgTasksRunning(bg));
        }

        // A carve is landing (the guard will shape the new writable set):
        // bump the epoch so a concurrent timeout->background conversion —
        // which compares the epoch it recorded at fork, under a fresh
        // READ — notices and refuses. `Relaxed` suffices: the RwLock's
        // own acquire/release ordering makes the bump visible to anyone
        // who takes the READ side after this WRITE drops.
        self.carve_epoch.fetch_add(1, Ordering::Relaxed);
        Ok(ExecWriteGuard(guard))
    }

    /// A background task entered the running tally. Call AFTER the task is
    /// registered in the registry so the drain paths (which iterate registered
    /// tasks) cover it: the invariant "counter > 0 => task is tracked" must hold.
    pub fn inc_bg(&self) {
        self.running_bg.fetch_add(1, Ordering::Relaxed);
    }

    /// A background task left the running tally. The caller MUST ensure this is
    /// called at most once per task (e.g. via a CAS on the task's state atomic
    /// from `STATE_RUNNING` to a terminal). Detects a double-decrement loudly:
    /// a release-visible error log plus a silent clamp at zero (never wraps).
    ///
    /// Logged rather than `panic!`d (this crate's usual idiom for impossible
    /// states) because `dec_bg` runs from `Drop` paths, where a panic would
    /// abort the process; a leaked tally is non-fatal and the clamp prevents the
    /// only real harm (a `u64` wrap).
    pub fn dec_bg(&self) {
        loop {
            let cur = self.running_bg.load(Ordering::Relaxed);
            if cur == 0 {
                tracing::error!(
                    target: "kallip_shell::gate",
                    "running_bg underflow: dec_bg with no running background task \
                     (double-decrement bug)"
                );
                return;
            }
            if self
                .running_bg
                .compare_exchange(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Current running-background count (diagnostic; the carve checks it via
    /// [`Self::try_write`]).
    pub fn running_bg(&self) -> u64 {
        self.running_bg.load(Ordering::Relaxed)
    }

    /// Current carve epoch (see the `carve_epoch` field). Exec records it
    /// at fork under the READ permit; adoption compares it later under a
    /// fresh READ.
    pub fn carve_epoch(&self) -> u64 {
        self.carve_epoch.load(Ordering::Relaxed)
    }
}

/// READ guard; a no-op when the gate was absent. Held across a shell fork. The
/// inner guard is load-bearing for its `Drop` (it is never read directly).
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "load-bearing for its Drop; the inner field is never read"
)]
pub struct ExecReadGuard<'a>(Option<tokio::sync::RwLockReadGuard<'a, ()>>);

/// WRITE guard held across a carve-out. Its `Drop` re-enables forks; the inner
/// guard is never read directly.
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "load-bearing for its Drop; the inner field is never read"
)]
pub struct ExecWriteGuard<'a>(tokio::sync::RwLockWriteGuard<'a, ()>);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn try_write_succeeds_when_idle() {
        let gate = ExecGate::new();
        let _g = gate.try_write().expect("idle gate is writable");
    }

    #[tokio::test]
    async fn try_write_fails_while_read_held() {
        let gate = ExecGate::new();
        let gate_for_read = Some(gate.clone());
        let _read = ExecGate::read(&gate_for_read).await;
        assert_eq!(
            gate.try_write().unwrap_err(),
            ExecGateFailure::ForegroundExecInProgress
        );
    }

    #[tokio::test]
    async fn try_write_fails_after_inc_bg() {
        let gate = ExecGate::new();
        gate.inc_bg();
        assert_eq!(
            gate.try_write().unwrap_err(),
            ExecGateFailure::BgTasksRunning(1)
        );
        gate.dec_bg();
        assert!(gate.try_write().is_ok());
    }

    #[tokio::test]
    async fn read_is_noop_when_gate_absent() {
        let _read = ExecGate::read(&None).await;
        // No gate to contend with; nothing to assert beyond compilation + drop.
    }

    #[tokio::test]
    async fn carve_epoch_bumps_on_every_successful_write() {
        let gate = ExecGate::new();
        assert_eq!(gate.carve_epoch(), 0);
        drop(gate.try_write().unwrap());
        assert_eq!(gate.carve_epoch(), 1);
        // A carve refused on running-bg never landed: no epoch bump, so
        // an adoption racing a merely-attempted carve is unaffected.
        gate.inc_bg();
        assert!(gate.try_write().is_err());
        assert_eq!(gate.carve_epoch(), 1);
        gate.dec_bg();
        drop(gate.try_write().unwrap());
        assert_eq!(gate.carve_epoch(), 2);
    }

    #[tokio::test]
    async fn dec_bg_clamps_at_zero_and_logs() {
        // A stray dec on a zero counter must not wrap to u64::MAX; it clamps.
        let gate = ExecGate::new();
        gate.dec_bg();
        assert_eq!(gate.running_bg(), 0);
    }

    #[tokio::test]
    async fn inc_dec_balance() {
        let gate = ExecGate::new();
        for _ in 0..3 {
            gate.inc_bg();
        }
        assert_eq!(gate.running_bg(), 3);
        for _ in 0..3 {
            gate.dec_bg();
        }
        assert_eq!(gate.running_bg(), 0);
        // Gate is writable again once balanced.
        assert!(gate.try_write().is_ok());
    }
}
