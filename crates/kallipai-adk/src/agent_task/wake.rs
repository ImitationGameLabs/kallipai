//! A wake's verdict, and the one drain that may follow it.
//!
//! Two vocabularies name how a wake ended: the disposition the loop reads, and
//! the outcome the wake family counts; the map between them is written out because
//! it is not one to one. A wake that ended awake may deliver one buffered inbox
//! message into a second wake; nothing else does.

use super::context::AgentContext;
use super::rounds::run_and_report;
use crate::event::AgentEvent;
use crate::metrics::WakeOutcome;
use crate::retry::AttemptOrigin;

/// How the wake ended, as the outer loop needs to read it.
///
/// No value here is read back off the lifecycle state: each is declared where
/// the wake ends, because one state can serve two endings (`break(wait)` sits
/// in `Waiting` and still ends awake). `Awake` says the wake left nothing
/// holding the loop: no hard park, no armed backoff. It does not promise the next
/// round gets through — a gate can stop it, and that round ends `Held` on its
/// own. `Held` says something the wake must wait out holds the loop: a hard
/// park, a gate that re-checks itself (the wake modality gate, the token
/// budget, the pacing table), or the backoff this wake armed itself.
/// `Terminated` is a lifecycle cancel.
///
/// The split matters because a round's aftermath is not free: the post-round
/// inbox drain delivers buffered messages into a round, so the drain runs only
/// after the wake's first round, and only when that round ended awake. A wake
/// that ran two rounds reports the second one's verdict; the notify arm pulls
/// before its round, so its own verdict cannot gate the pull.
///
/// One boundary keeps each value a verdict about the wake, not a reading of
/// the task's state, and is worth naming:
///
/// - `Terminated` never follows from the task being parked. A fatal error, a
///   stopped compaction, the modality gate, a quota block and the max-rounds
///   cap settle a `Parked` state; the budget-probe gate instead settles
///   `Waiting` with a re-armed timer. Either way the task stays alive (a kick
///   or an external prompt still wakes it); only a lifecycle cancel ends it.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDisposition {
    /// The wake left nothing holding the loop: no hard park, no armed backoff.
    /// A rest fuse the agent chose is not a backoff, so `break(wait)` lands here.
    Awake,
    /// Something the wake must wait out holds the loop: a hard park, a gate that
    /// re-checks itself, or the backoff this wake armed. `break(wait)` is not
    /// this — that fuse is rest the agent chose, not a wait the wake owes.
    Held,
    /// A lifecycle cancel ended the run: the task terminates.
    Terminated,
}

/// How a wake ended, in the two vocabularies that ask about it: the
/// disposition the task's loop reads, and the outcome the wake family counts.
///
/// One wake, one count: [`run_and_report`] books the pair once, from its
/// entry to the point where it hands control back — a park, an idle, or a
/// termination. The turns a wake runs are the model and tool counters'
/// business, and the trace's `invoke_agent` span is the same unit as this
/// family.
///
/// The disposition and the outcome are two vocabularies, and the map between
/// them is written out because it is not one-to-one:
///
/// | how the wake ended | disposition | wake outcome |
/// | --- | --- | --- |
/// | `break`, `Idle`, the heartbeat guardrail | `Awake` | `Completed` |
/// | a turn an interrupt cancelled | `Awake` | `Cancelled` |
/// | a lifecycle cancel | `Terminated` | `Cancelled` |
/// | chain exhausted, quota block, token budget, max rounds, a fatal error,
///   the modality gate | `Held` | `Failed` |
///
/// A wake that holds the loop is not thereby a success: the park an operator
/// reads in the log and the failure this family counts have to agree.
pub(super) struct WakeEnd {
    pub(super) disposition: WakeDisposition,
    pub(super) outcome: WakeOutcome,
}

impl WakeEnd {
    /// The wake finished its work: `break`, `Idle`, or the heartbeat guardrail.
    pub(super) fn done() -> Self {
        Self {
            disposition: WakeDisposition::Awake,
            outcome: WakeOutcome::Completed,
        }
    }

    /// An interrupt cancelled the turn; the task itself stays alive.
    pub(super) fn interrupted() -> Self {
        Self {
            disposition: WakeDisposition::Awake,
            outcome: WakeOutcome::Cancelled,
        }
    }

    /// A lifecycle cancel ended the run: the task terminates.
    pub(super) fn terminated() -> Self {
        Self {
            disposition: WakeDisposition::Terminated,
            outcome: WakeOutcome::Cancelled,
        }
    }

    /// The wake did not finish its work and parks instead: a gate refusal, a
    /// spent chain, a quota window, budget, rounds, or a fatal error.
    pub(super) fn failed() -> Self {
        Self {
            disposition: WakeDisposition::Held,
            outcome: WakeOutcome::Failed,
        }
    }
}

/// Run one wake and, when it ended awake, deliver what the inbox buffered into
/// a second wake. Each of the two is a full wake — its own `invoke_agent` span,
/// its own count in the wake family — so one drain can book two.
///
/// A first wake that ended awake has left the agent free to run again — the
/// kick wake is the case that called for this — so messages that arrived while
/// parked (or during the wake) are pulled here instead of waiting for the next
/// notify: the parked guard consumes notify permits, so notify alone never
/// delivers them while the agent stays parked. `pull_undelivered` is an atomic
/// drain, so a racing notify arm simply finds nothing left to pull. Only a wake
/// that ended awake delivers here; any other verdict leaves its buffered
/// message in the inbox.
///
/// The second wake runs as `External`: the pulled message is new work from
/// outside, so it forces the pacing window open rather than obeying a record
/// this agent's own episode wrote.
///
/// One layer only: a message that arrives during the second wake waits for the
/// next one. Both wakes read the same [`WakeDisposition`] contract, so a
/// second-wake `Held` never delivers a third message.
///
/// Every entrance of the task loop reaches a wake through it; the loop and its
/// entrances are `task_loop`'s subject.
pub(crate) async fn wake_and_drain(
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    origin: AttemptOrigin,
) -> WakeDisposition {
    let disposition = run_and_report(ctx, agent_tx, prompt_rx, origin).await;
    if disposition == WakeDisposition::Awake
        && let Some(ref puller) = ctx.message_puller
        && let Some(msg) = puller.pull_undelivered().await
    {
        ctx.record_message_turn(&msg).await;
        return run_and_report(ctx, agent_tx, prompt_rx, AttemptOrigin::External).await;
    }
    disposition
}
