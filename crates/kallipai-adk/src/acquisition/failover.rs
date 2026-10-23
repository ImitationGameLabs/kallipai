//! Within-set failover driving and its vocabulary.
//!
//! `advance_failover` is the sole driver of chain advancement: it commits the advance,
//! re-applies the context window and compacts the carried context through
//! `summarize_and_evict` (a no-op when the context already fits; a failed compaction
//! goes through `note_compaction_failure`), and maps the outcome — the compaction
//! verdict feeding the budget-exhausted and stop exits. The step, exhaustion and stop
//! types here are what the acquisition loop reads to decide where a round ends.

use super::facts::{candidate_marked, emit_gateway_signal, rejection_payload};
use super::stream::AcquireResult;

use crate::agent_task::AgentContext;
use crate::context::{
    CompactOutcome, CompactionVerdict, compose_context, note_compaction_failure,
    summarize_and_evict,
};
use crate::event::{AgentEvent, AgentOutcome};
use crate::failover::FailoverOutcome;
use anyhow::Error;
use just_llm_client::types::generation::Message;
use kallipai_common::protocol::{FailoverChainExhaustion, QuotaBlockedInfo};
use kallipai_common::timefmt;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// One step of within-set failover, shared by the prepare/send/parse `Failover` arm and the
/// mid-stream-drop budget-exhausted path so both entry points behave identically.
pub(super) enum FailoverStep {
    /// The chain advanced to a new profile; the caller re-loops with the rebound `messages`.
    Advanced,
    /// A terminal acquire result (chain exhausted / cancelled / budget exceeded).
    Done(AcquireResult),
}

/// A send that failed, and the caller's reading of it: the trigger error, the
/// operator-facing reason captured before the error moves into the walk, and the
/// exhaustion reading that decides what an exhausted chain means.
pub(super) struct FailedSend {
    pub(super) trigger: Error,
    pub(super) reason: String,
    pub(super) exhaustion: Exhaustion,
}

/// What a chain exhaustion means for the trigger that caused it — why the chain
/// was being walked at all, and which carrier the round ends with.
///
/// The carrier is chosen where the class is known (the caller), so the mapping
/// from cause to exit is readable at the call site; this enum only carries it
/// down to the exhaustion arm.
#[derive(Clone, Debug)]
pub(super) enum Exhaustion {
    /// The whole chain is transiently down: the outer re-run budget decides
    /// whether the round is retried later.
    Transient,
    /// A class another send cannot help with, and no payload to carry:
    /// exhaustion stops the round with the class's own wording.
    ExternalOnly { stop_reason: &'static str },
    /// A quota or payment block: exhaustion parks the agent with the structured
    /// reason — which profile, the moment the upstream said service
    /// resumes, the evidence — and arms nothing.
    QuotaBlocked(QuotaBlockedInfo),
}

/// Why a local configuration failure stops without walking the chain: it fails
/// on every profile, so a switch cannot help.
pub(super) const LOCAL_CONFIG_NOTE: &str =
    "no failover attempted: a local misconfiguration fails on every profile";

/// The operator-facing message for a stop no payload carries: which endpoint and
/// profile stopped serving, why the chain did not take over, and the rendered
/// evidence.
///
/// No structured carrier exists for these stops (a quota or payment block parks
/// with `QuotaBlockedInfo` instead), so this message is the whole payload. The
/// classes it serves have no resumption moment to report — the one that does
/// carries it in the payload.
pub(super) fn stop_message(
    stop_reason: &'static str,
    ctx: &AgentContext,
    error: &(dyn std::error::Error + 'static),
    chain_note: &str,
) -> String {
    let profile = ctx.failover.current_profile();
    format!(
        "{}: endpoint {} (profile {}); {}; evidence: {}",
        stop_reason,
        profile.endpoint,
        profile.id,
        chain_note,
        crate::llm_error::render_error(error)
    )
}

/// Drive within-set failover for `trigger`: advance the chain, emit a `Failover` event on
/// advance, and map the [`FailoverOutcome`] to a [`FailoverStep`]. `reason` is the operator-facing
/// diagnostic captured from `trigger` before it is moved into [`advance_failover`].
///
/// `exhaustion` is the caller's reading of why the chain was walked: a transient
/// trigger leaves an exhausted chain to the outer re-run budget, while a class
/// another send cannot help with stops the round here.
pub(super) async fn step_failover(
    ctx: &mut AgentContext,
    messages: &mut Vec<Message>,
    failed: FailedSend,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
    round: usize,
) -> FailoverStep {
    let FailedSend {
        trigger,
        reason,
        exhaustion,
    } = failed;
    match advance_failover(ctx, std::mem::take(messages), trigger, round_cancel, round).await {
        FailoverOutcome::Advanced {
            from,
            to,
            messages: new_messages,
        } => {
            *messages = new_messages;
            // Under skip, `from`→`to` may jump over unbuildable intermediates; those are
            // warned inside advance_failover (not surfaced here).
            warn!(from = %from, to = %to, reason = %reason, "within-set failover");
            tx.send(AgentEvent::Failover { from, to, reason })
                .await
                .ok();
            // A different profile is a fresh execution: its send budget and its
            // deadline restart with it.
            ctx.start_episode();
            FailoverStep::Advanced
        }
        FailoverOutcome::ChainExhausted { reason, trigger } => {
            // An auth rejection at exhaustion drives the tagma's profile-source
            // refresh; both exits below keep reporting it. A fact-driven walk
            // brings the fact's own wording here, not a rejection, so its signal
            // comes from the arm that drove the walk.
            if let Some(payload) = rejection_payload(&trigger) {
                emit_gateway_signal(ctx, payload);
            }
            match exhaustion {
                // A class another send cannot help with: the chain is out of
                // candidates, and re-running would repeat the same failure.
                // The message names why the chain ran out, not just that it did.
                Exhaustion::ExternalOnly { stop_reason } => {
                    let note = reason.to_string();
                    return FailoverStep::Done(AcquireResult::Stop {
                        message: stop_message(stop_reason, ctx, trigger.as_ref(), &note),
                    });
                }
                // A quota or payment block: the round ends as a structured park
                // rather than as an error. Nothing is armed — the payload is the
                // whole reason, and recovery comes from an external trigger.
                Exhaustion::QuotaBlocked(info) => {
                    return FailoverStep::Done(AcquireResult::Outcome(AgentOutcome::QuotaBlocked(
                        info,
                    )));
                }
                // Transient: fall through to the round-end the re-run budget owns.
                Exhaustion::Transient => {}
            }
            // Chain exhaustion is a defined round-end (sibling of MaxRoundsExceeded),
            // surfaced as a distinguishable terminal outcome rather than a generic
            // `Err`. `trigger` is an `anyhow::Error`; `render_error` de-duplicates its
            // source chain and surfaces any HTTP error body (`as_ref` is required —
            // `anyhow::Error` does not impl `std::error::Error`).
            FailoverStep::Done(AcquireResult::Outcome(
                AgentOutcome::FailoverChainExhausted {
                    reason,
                    detail: crate::llm_error::render_error(trigger.as_ref()),
                },
            ))
        }
        FailoverOutcome::Cancelled => {
            FailoverStep::Done(AcquireResult::Outcome(AgentOutcome::Cancelled))
        }
        FailoverOutcome::BudgetExceeded { consumed, budget } => {
            FailoverStep::Done(AcquireResult::Outcome(AgentOutcome::TokenBudgetExceeded {
                consumed,
                budget,
            }))
        }
        FailoverOutcome::Stop { message } => FailoverStep::Done(AcquireResult::Stop { message }),
    }
}

/// Advance the within-set failover chain on a terminal endpoint failure (`trigger`).
///
/// Walks the chain forward from the active profile and lands on the first candidate that is
/// **both buildable and window-feasible**. **Skip:** a candidate whose
/// `build_client` fails, *or* whose declared `max_context_window` violates the configured budget
/// shape (`AgentConfig::try_context_window`), is `warn!`-ed and skipped. The window check runs
/// **before** `advance_to` (which is forward-only and cannot roll back), so the agent is never
/// left with the client swapped to a model whose window can't serve the budget. On a successful
/// build + feasible window it commits the advance, swaps the client, re-applies the new profile's
/// context window, re-syncs the pinned-budget guard, and compacts the carried context if it now
/// exceeds the (possibly smaller) window. The (possibly recomputed) `messages` return via
/// [`FailoverOutcome::Advanced`] so the caller rebinds its round-local.
///
/// This is the sole driver of `FailoverState` advancement; the round loop just dispatches on the
/// outcome. The `AgentEvent::Failover` emission and the retry-record flush stay in the caller
/// (the arm) — keeping this function free of channel side effects so it is unit-testable without
/// an mpsc sender. `summarize_and_evict` has no internal cancel select, so a cancel fired *during*
/// compaction completes it and returns `Advanced` (the cancel is observed on the next loop
/// iteration).
pub(crate) async fn advance_failover(
    ctx: &mut AgentContext,
    prior_messages: Vec<Message>,
    trigger: anyhow::Error,
    round_cancel: &CancellationToken,
    round: usize,
) -> FailoverOutcome {
    // Honor a cancel that raced in between the failover decision and the chain advance — takes
    // precedence over chain-exhaustion.
    if round_cancel.is_cancelled() {
        return FailoverOutcome::Cancelled;
    }
    // No candidate ahead — single-profile set, or already at the chain tail. Distinguish the
    // two: a single-profile set means failover was never configured, while a multi-profile
    // set at its tail means the chain was advanced through and now the last profile failed.
    if !ctx.failover.can_advance() {
        let reason = if ctx.failover.profile_count() == 1 {
            FailoverChainExhaustion::NoFailoverConfigured
        } else {
            FailoverChainExhaustion::AllBackupsExhausted
        };
        return FailoverOutcome::ChainExhausted { reason, trigger };
    }
    let from = ctx.failover.current_profile().id.clone();
    let mut offset = 1usize;
    // Track why candidates were skipped so the terminal exhaustion reason steers the operator:
    // a window-infeasible chain means the budget shape needs tuning; an unbuildable chain means
    // credentials/endpoint config; a chain the shared table has marked means another
    // profile has to be configured. (See the coalescing rule on
    // `FailoverChainExhaustion`.)
    let mut skipped_infeasible = false;
    let mut skipped_unbuildable = false;
    let mut skipped_marked = false;
    // The shared table's own reading is what keeps the walk off a marked
    // profile; the fact being escaped was written before the walk starts, so a
    // mark on a candidate already answers for it.
    let now = timefmt::now_epoch();
    while let Some(candidate) = ctx.failover.candidate_profile(offset) {
        // A candidate the table has already marked cannot serve: switching into it
        // would burn an attempt to learn what another agent already paid for.
        if let Some(health) = ctx.endpoint_health.as_deref()
            && candidate_marked(health, &candidate.id, now)
        {
            warn!(
                from = %from,
                candidate = %candidate.id,
                "failover candidate marked in the shared fact table, skipping"
            );
            skipped_marked = true;
            offset += 1;
            continue;
        }
        match ctx.failover.build_client(&candidate) {
            Ok(new_client) => {
                // The candidate builds — but its declared window must also fit the budget shape.
                // Pre-check BEFORE committing (advance_to is forward-only, no rollback): an
                // infeasible window is skipped like an unbuildable backend, so the agent never
                // ends up with the client swapped to a model whose window can't serve the budget.
                if let Err(err) = ctx.config.try_context_window(candidate.max_context_window) {
                    warn!(
                        from = %from,
                        candidate = %candidate.id,
                        window = candidate.max_context_window,
                        "failover candidate window infeasible for budget shape, skipping: {err:#}"
                    );
                    skipped_infeasible = true;
                    offset += 1;
                    continue;
                }
                // Commit only after a successful build + feasible window: the index advances
                // and the client swaps once we know the new profile is usable.
                let target_idx = ctx.failover.profile_idx() + offset;
                ctx.failover.advance_to(target_idx);
                ctx.client = new_client;
                // Fresh chain for the fresh client: response ids issued by the old
                // provider are meaningless to the new, so the next request resends fully.
                // The fresh chain's store flag rides the profile we just advanced to
                // (`None` = client default, store on).
                ctx.conversation = ctx
                    .client
                    .conversation()
                    .with_store(candidate.store.unwrap_or(true));
                reapply_window(ctx, &from).await;
                // The new endpoint replaces the ledger: the stretch so far was about
                // the endpoint that just failed, and this one has not been tried yet.
                ctx.compaction.clear(timefmt::now_epoch());
                // The carried context may now exceed the (possibly smaller) window — compact so
                // the rebuilt request fits. summarize_and_evict no-ops when the context already
                // fits (it checks before any LLM call); it uses ctx.client, already swapped to
                // the working profile above.
                let messages = match summarize_and_evict(ctx).await {
                    Ok(CompactOutcome::Compacted) => compose_context(ctx.store.clone()).await,
                    Ok(CompactOutcome::NothingToCompact) => prior_messages,
                    Ok(CompactOutcome::BudgetExceeded { consumed, budget }) => {
                        return FailoverOutcome::BudgetExceeded { consumed, budget };
                    }
                    Err(err) => {
                        warn!("failover compaction failed on the new endpoint: {err:#}");
                        match note_compaction_failure(ctx, &err, round).await {
                            CompactionVerdict::Stop { message } => {
                                return FailoverOutcome::Stop { message };
                            }
                            CompactionVerdict::Retry { .. } | CompactionVerdict::Defer { .. } => {
                                prior_messages
                            }
                        }
                    }
                };
                return FailoverOutcome::Advanced {
                    from,
                    to: candidate.id.clone(),
                    messages,
                };
            }
            Err(err) => {
                warn!(
                    from = %from,
                    candidate = %candidate.id,
                    "failover candidate backend unbuildable, skipping: {err:#}"
                );
                skipped_unbuildable = true;
                offset += 1;
            }
        }
    }
    // Every remaining candidate was skipped (each warned above). Surface the original trigger —
    // it is why failover was attempted, and the actionable cause for the operator. Prefer the
    // infeasible reason when present (the subtler, more actionable mode); per-candidate warns
    // carry each skip's precise cause.
    let reason = if skipped_infeasible {
        FailoverChainExhaustion::AllCandidatesInfeasible
    } else if skipped_unbuildable {
        FailoverChainExhaustion::AllCandidatesUnbuildable
    } else if skipped_marked {
        // Every candidate ahead is marked in the shared table: the chain has no
        // endpoint left that could serve this request, which is what "all backups
        // exhausted" says. The per-candidate warns name the marking itself.
        FailoverChainExhaustion::AllBackupsExhausted
    } else {
        FailoverChainExhaustion::AllCandidatesUnbuildable
    };
    FailoverOutcome::ChainExhausted { reason, trigger }
}

/// Re-apply the active profile's declared context window to the config and re-sync the store's
/// pinned-budget guard. Called after a failover advance swaps to a profile that may declare a
/// different window (within-set heterogeneous windows are supported).
///
/// The window was already pre-checked feasible in `advance_failover` (before the commit), so
/// `set_context_window` is expected to succeed here. The `warn!`-and-keep-prior branch stays as
/// defense-in-depth: if an invariant somehow still fails post-commit, keeping the prior window is
/// safer than `?`-propagating into a half-applied state (index advanced, window stale). The
/// pinned-budget re-sync + `mark_needs_full_estimate` are unconditional and idempotent.
async fn reapply_window(ctx: &mut AgentContext, from: &str) {
    let new_window = ctx.failover.current_profile().max_context_window;
    if let Err(err) = ctx.config.set_context_window(new_window) {
        warn!(
            from = %from,
            target_window = new_window,
            "failed to re-apply context window on failover, keeping prior: {err:#}"
        );
    }
    // Failover swapped the active profile: the new provider's tokenizer renders the same prompt
    // to a different count, so the persisted `last_prompt_tokens` anchor is invalid — force a
    // full estimate on the next gate until a response re-anchors.
    let mut store = ctx.store.lock().await;
    store.set_pinned_budget(ctx.config.pinned_budget());
    store.mark_needs_full_estimate();
}
