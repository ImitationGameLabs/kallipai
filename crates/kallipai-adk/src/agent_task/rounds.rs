//! The heartbeat loop, the state each round ending settles to, and the one count
//! a wake books.
//!
//! The round token is minted per round from the lifecycle token, published for
//! the interrupt path, and cleared afterwards; the heartbeat loop keeps a bare
//! assistant round alive with a nudge and a no-progress cap. `run_and_report` runs
//! a wake's rounds and books the count; naming how that wake ended is `wake`'s
//! subject.

use super::context::{AgentContext, apply_pending_profile_reset};
use super::fuses::{ParkArm, arm_pacing_wake};
use super::task_loop::terminate_cancelled;
use super::wake::{WakeDisposition, WakeEnd};
use crate::context::assistant_message;
use crate::event::{AgentEvent, AgentOutcome};
use crate::lifecycle::LifecycleState;
use crate::retry::AttemptOrigin;
use crate::runner;
use crate::tools::DEFAULT_BREAK_TIMEOUT_SECS;
use just_llm_client::types::generation::Message;
use kallipai_common::protocol::{ParkedReason, TransientRetryInfo};
use kallipai_common::telemetry::{Settle, record_attribute};
use kallipai_common::timefmt;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// A cancellation token scoped to a single round, always a child of the agent's lifecycle
/// token ([`AgentContext::cancel`]). The tagma interrupts a round by cancelling it,
/// which aborts that round without terminating the task.
///
/// Because it is a child, a lifecycle cancel propagates to it —
/// so **lifecycle-cancelled ⟹ round-cancelled**. The converse (round cancelled but
/// lifecycle not) is exactly what distinguishes an interrupt from a lifecycle cancel. This
/// holds iff the token is always minted via [`RoundToken::new`] from the lifecycle token;
/// the newtype makes that invariant structural rather than conventional.
#[derive(Clone)]
pub struct RoundToken(CancellationToken);

impl RoundToken {
    /// Mint a round token as a child of the agent lifecycle token.
    pub fn new(lifecycle: &CancellationToken) -> Self {
        Self(lifecycle.child_token())
    }

    /// Cancel this round: the tagma interrupts a running round.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// The underlying token, for the runner (and its callees `consume_stream` /
    /// `stream_with_retry`) to select on, and for any caller to inspect
    /// (`handle().is_cancelled()`). The runner legitimately needs the raw
    /// `CancellationToken` for its `select!` arms, so this is the intended seam —
    /// not an encapsulation leak.
    pub fn handle(&self) -> &CancellationToken {
        &self.0
    }
}

/// How to treat a round-token cancel, classified by whether the lifecycle (parent)
/// token is also cancelled. Correct iff the round token is always a child of the
/// lifecycle token — see [`RoundToken`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CancelKind {
    /// The lifecycle token was cancelled → terminate the task.
    Lifecycle,
    /// Only the round token was cancelled (interrupt) → keep the task alive.
    Interrupt,
}

impl CancelKind {
    /// Classify a round-token cancel by inspecting the lifecycle token.
    ///
    /// A lifecycle cancel propagates to its children, so if the lifecycle token is
    /// cancelled the round-token cancel was a consequence of it (terminate);
    /// otherwise the round token alone fired (interrupt).
    pub(super) fn classify(lifecycle: &CancellationToken) -> Self {
        if lifecycle.is_cancelled() {
            Self::Lifecycle
        } else {
            Self::Interrupt
        }
    }
}

/// Run agent rounds for one external wake and send results via channel.
///
/// Owns the heartbeat loop: a bare-assistant round (no `break`, no tool calls) no
/// longer terminates the run — the harness records the assistant turn, injects a
/// heartbeat prompt, and re-enters the round loop. The no-progress guardrail
/// (`config.max_heartbeat_rounds`) force-idles after a bounded storm. Only `break`
/// (or a non-deliberate park reason) returns.
///
/// Owns the round-token lifecycle per iteration: mints a fresh child of the
/// lifecycle token, publishes it into `ctx.round_cancel` (so the tagma's interrupt
/// path can reach it), runs the round, then clears the slot. It returns
/// [`WakeDisposition::Terminated`] only on a lifecycle cancel (the task should
/// terminate); every other outcome keeps the task alive.
pub async fn run_and_report(
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    origin: AttemptOrigin,
) -> WakeDisposition {
    let span = crate::spans::wake_span(
        &ctx.config.role,
        &crate::spans::agent_label(ctx.config.agent_id.as_ref()),
        match origin {
            AttemptOrigin::External => "external",
            AttemptOrigin::System => "system",
        },
    );
    let settle = Settle::new(&span);
    let started = std::time::Instant::now();
    let end = run_and_report_inner(ctx, agent_tx, prompt_rx, origin)
        .instrument(span.clone())
        .await;
    // One wake, one count: from this entry to the point where it hands control
    // back, a park, an idle, or a termination. The turns inside it belong to the
    // model and tool families, and the trace draws the same boundary at the
    // `invoke_agent` span.
    ctx.metrics.record_wake(end.outcome, started.elapsed());
    match end.disposition {
        WakeDisposition::Terminated => {
            record_attribute(&span, "kallipai.wake.outcome", "terminated");
            settle.cancelled();
        }
        WakeDisposition::Awake => {
            record_attribute(&span, "kallipai.wake.outcome", "awake");
            settle.done();
        }
        WakeDisposition::Held => {
            record_attribute(&span, "kallipai.wake.outcome", "held");
            settle.done();
        }
    }
    end.disposition
}

/// The wake's work, under [`run_and_report`]'s span: the heartbeat loop, the
/// rounds it runs, and the park or exit the loop leaves behind.
async fn run_and_report_inner(
    ctx: &mut AgentContext,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    origin: AttemptOrigin,
) -> WakeEnd {
    // The wake that caused this execution decides how its first attempt reads:
    // an external one is served once even against a marked endpoint, a timer one
    // is not. Taken by the first pre-send reading below.
    ctx.attempt_origin = origin;
    // An external wake restarts the compaction count: a prompt from outside is new
    // work, so the failure count and its deadline start over with it. What the last
    // verdict left behind — an armed backoff, a stop waiting for a gate — stays,
    // because both answer for the context a round would send, not for the count.
    if origin == AttemptOrigin::External {
        ctx.compaction.restart(timefmt::now_epoch());
    }
    // Drain any pending profile reset before starting the round loop.
    apply_pending_profile_reset(ctx).await;
    // This entry owns the lifecycle move: the wake is served from here on, so
    // the agent is Running for its span and a refusal below parks it over the
    // table's legal Running -> Parked edge; no other state enters Parked.
    // The entry is reached only from a non-Running state, and the move sits
    // after attempt_origin is set above, so the Running log line names this wake.
    ctx.settle(LifecycleState::Running, "wake entry");
    // Wake modality gate: a restored context whose assembled content needs
    // modalities the bound set cannot serve must not start a round — the
    // model would silently never see the referenced content. Surface one
    // actionable error and park; a rebind (profile apply) clears the gate
    // on the next wake. The requirement is computed from what the store
    // actually holds (window + pins), never from a history rescan.
    let required = {
        let store = ctx.store.lock().await;
        crate::context::required_modalities(&store)
    };
    if let Err(blocked) = ctx.failover.set().ensure_supports(&required) {
        tracing::warn!(error = %blocked, "wake blocked by modality gate");
        agent_tx
            .send(AgentEvent::Error(blocked.to_string()))
            .await
            .ok();
        ctx.settle(
            LifecycleState::Parked {
                reason: ParkedReason::FatalError {
                    message: blocked.to_string(),
                },
                at: std::time::Instant::now(),
            },
            "modality gate",
        );
        return WakeEnd::failed();
    }
    let mut no_progress: u32 = 0;
    loop {
        let round = RoundToken::new(&ctx.cancel);
        // Publish the round token for the duration of this round so the tagma's
        // interrupt path can cancel it. `Some` only while the round is in flight.
        *ctx.round_cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(round.clone());

        agent_tx.send(AgentEvent::Busy).await.ok();
        let result =
            runner::run_rounds_and_release_window(ctx, agent_tx, prompt_rx, round.handle()).await;

        // Always clear the slot: a stale token cancelled by a later interrupt would be a
        // no-op (nobody selects on it), but clearing keeps the invariant tight.
        *ctx.round_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;

        match result {
            Ok(runner::RoundOutcome::Break(until)) => {
                // Deliberate yield: park as Idle (`until:"idle"`) or Waiting
                // (armed wake timer, the default). Any tool calls preceding
                // `break` were already recorded inside the round loop.
                ctx.transient_fails = 0;
                match until {
                    runner::BreakUntil::Idle => {
                        ctx.settle(LifecycleState::Idle, "break(idle)");
                        agent_tx.send(AgentEvent::Idle).await.ok();
                    }
                    runner::BreakUntil::Wait { timeout_secs } => {
                        let park = ctx.arm_park(ParkArm::Wait { timeout_secs });
                        ctx.settle_park(park, "break(wait)");
                        agent_tx
                            .send(AgentEvent::Waiting { timeout_secs })
                            .await
                            .ok();
                    }
                }
                return WakeEnd::done();
            }
            Ok(runner::RoundOutcome::BareAssistant {
                content,
                reasoning,
                attempt,
            }) => {
                // No `break`, no tool calls — heartbeat and continue. Reset the
                // transient-retry counter (a real response is progress).
                ctx.transient_fails = 0;
                no_progress += 1;
                if no_progress > ctx.config.max_heartbeat_rounds {
                    // Self-monologue guardrail: force-idle instead of looping forever.
                    ctx.settle(LifecycleState::Idle, "heartbeat limit");
                    agent_tx.send(AgentEvent::Idle).await.ok();
                    return WakeEnd::done();
                }
                ctx.record_turn_with_attempt(
                    vec![assistant_message(Some(content), Vec::new(), &reasoning)],
                    *attempt,
                )
                .await;
                ctx.record_turn(vec![Message::user(HEARTBEAT_TEXT)]).await;
                continue;
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::Cancelled)) => {
                ctx.transient_fails = 0;
                match CancelKind::classify(&ctx.cancel) {
                    // Lifecycle cancel propagated to the round token → terminate.
                    CancelKind::Lifecycle => {
                        tracing::info!(
                            "agent task: lifecycle cancel mid-round, persisting and exiting"
                        );
                        terminate_cancelled(ctx, agent_tx).await;
                        return WakeEnd::terminated();
                    }
                    // Only the round token was cancelled (interrupt) → keep living.
                    // An interrupt cancels this round, not the queue: a buffered
                    // message is work already queued for it, so the round
                    // ends awake, and the drain after a first round serves it. The
                    // park form (no round in flight) takes the other route: it
                    // wakes the loop once more instead of falling through to the
                    // drain.
                    CancelKind::Interrupt => {
                        agent_tx.send(AgentEvent::Interrupted).await.ok();
                        ctx.settle(LifecycleState::Idle, "interrupt");
                        return WakeEnd::interrupted();
                    }
                }
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::FailoverChainExhausted {
                reason,
                detail,
                ..
            })) => {
                // Transient: the whole chain is down. Schedule a timed retry
                // (bounded by `max_transient_retries`); the outer loop's retry
                // arm re-enters after the backoff. On the last attempt,
                // hard-park and surface so the operator can reconfigure
                // failover. The arming rides the terminal event's payload so
                // the bridge can mark RETRYING (not PARKED) without flicker.
                ctx.transient_fails = ctx.transient_fails.saturating_add(1);
                let armed = ctx.transient_fails <= ctx.config.max_transient_retries;
                let retry_park = armed.then(|| ctx.arm_park(ParkArm::Retry));
                agent_tx
                    .send(AgentEvent::FailoverChainExhausted {
                        reason,
                        detail,
                        transient_retry: retry_park.as_ref().and_then(|park| {
                            park.retry_in_secs.map(|retry_in_secs| TransientRetryInfo {
                                attempt: ctx.transient_fails,
                                max_attempts: ctx.config.max_transient_retries,
                                retry_in_secs,
                            })
                        }),
                    })
                    .await
                    .ok();
                match retry_park {
                    Some(park) => ctx.settle_park(park, "transient retry armed"),
                    None => {
                        ctx.settle(
                            LifecycleState::Parked {
                                reason: ParkedReason::TransientRetryExhausted,
                                at: std::time::Instant::now(),
                            },
                            "transient retries exhausted",
                        );
                    }
                }
                // The retry fuse is a backoff this wake armed, so the wake is
                // held: draining now would spend the next attempt without the
                // backoff and charge the transient count twice.
                return WakeEnd::failed();
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::QuotaBlocked(info))) => {
                // A quota or payment block: park with the structured reason and
                // wake at the end of the window the fact opened. The park lasts
                // as long as that window and no longer: the round comes back on
                // its own. `transient_fails` is left where it stands (this is not
                // a transient park), and an external trigger still wakes the
                // agent first when it arrives before the window ends.
                agent_tx
                    .send(AgentEvent::QuotaBlocked(info.clone()))
                    .await
                    .ok();
                // The alarm answers to the fact this park wrote, read here: a
                // window another instance recorded paces this wake too.
                let _ = arm_pacing_wake(ctx, &info.profile);
                ctx.settle(
                    LifecycleState::Parked {
                        reason: ParkedReason::QuotaBlocked(info),
                        at: std::time::Instant::now(),
                    },
                    "quota blocked",
                );
                return WakeEnd::failed();
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::TokenBudgetExceeded {
                consumed,
                budget,
            })) => {
                // Non-fatal: the task stays alive — and per the
                // budget-probe decision it stays *Waiting* with a re-armed
                // timer rather than parking: the next timer wake re-checks
                // the budget before any LLM call (the round gate runs
                // first), so the armed fuse doubles as a zero-cost
                // budget-recovery probe. The fused arm and the mirrored state
                // go through `arm_park`/`settle_park` like the other two park
                // paths: the Waiting state must never name a park whose
                // generation is still unpublished.
                ctx.transient_fails = 0;
                let park = ctx.arm_park(ParkArm::Wait {
                    timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
                });
                ctx.settle_park(park, "budget probe re-arm");
                agent_tx
                    .send(AgentEvent::TokenBudgetExceeded { consumed, budget })
                    .await
                    .ok();
                // The budget gate holds the wake in either shape its verdict
                // takes — a pre-call reading, or a response already consumed and
                // charged — and it re-checks itself on the next probe wake, so
                // the buffered inbox message stays in the inbox.
                return WakeEnd::failed();
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::MaxRoundsExceeded)) => {
                ctx.transient_fails = 0;
                ctx.settle(
                    LifecycleState::Parked {
                        reason: ParkedReason::MaxRoundsExceeded,
                        at: std::time::Instant::now(),
                    },
                    "max rounds",
                );
                agent_tx.send(AgentEvent::MaxRoundsExceeded).await.ok();
                return WakeEnd::failed();
            }
            Ok(runner::RoundOutcome::Park(AgentOutcome::Idle)) => {
                // Not produced by the round loop today (only Break/BareAssistant/Park
                // are); parked here for exhaustiveness and forward-compat.
                ctx.transient_fails = 0;
                ctx.settle(LifecycleState::Idle, "park(idle)");
                agent_tx.send(AgentEvent::Idle).await.ok();
                return WakeEnd::done();
            }
            Err(e) => {
                // Permanent (Fatal) error — park and surface; the operator acts.
                ctx.transient_fails = 0;
                let rendered = crate::llm_error::render_error(e.as_ref());
                ctx.settle(
                    LifecycleState::Parked {
                        reason: ParkedReason::FatalError {
                            message: rendered.clone(),
                        },
                        at: std::time::Instant::now(),
                    },
                    "fatal error",
                );
                agent_tx.send(AgentEvent::Error(rendered)).await.ok();
                // A fatal park holds the wake: a drained round would meet the
                // same gate and report the failure twice.
                return WakeEnd::failed();
            }
        }
    }
}

/// The synthetic user turn injected after a bare-assistant round, nudging the
/// agent to either make progress, reply, or call `break`. Persisted like an
/// approval notification (bounded by the no-progress guardrail).
const HEARTBEAT_TEXT: &str = "[system] You produced a response with no tool action and did not \
call `break`. If you are done, call `break` with `{\"until\":\"idle\"}`; if you are blocked \
waiting on something, call `break` (the default parks you with a wake timer). Otherwise \
continue your work.";
