//! The verdict: [`Facts`] in, one [`Decision`] out, plus the wait that decision
//! opens (the backoff schedule, its jitter cap, and the deadline clamp).

use std::time::Duration;

use kallipai_common::facts::{Block, BlockKind, BlockedFact};

use super::attempt::{Alternative, AttemptOrigin, Phase};
use super::failure_class::{FailureClass, ShapeKey, THROTTLE_SENDS_ALLOWED, shape_sends_allowed};

use crate::retry::{RetryPolicy, max_sends};

/// The verdict input. Its fields are readable across this module's facets, so
/// the ledger can write the projection; today the one place that builds one is
/// [`Episode::facts`](super::ledger::Episode::facts).
#[derive(Clone, Debug)]
pub(crate) struct Facts {
    pub(super) attempts: u32,
    pub(super) elapsed: Duration,
    pub(super) previous_shape: Option<ShapeKey>,
    pub(super) phase: Phase,
    pub(super) failure: Option<FailureClass>,
    pub(super) shape: Option<ShapeKey>,
    pub(super) jitter: Duration,
    pub(super) origin: AttemptOrigin,
    pub(super) blocked: Option<BlockedFact>,
    pub(super) alternative: Option<Alternative>,
}

/// What the shell does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The pre-send reading passed: this send may happen.
    Proceed,
    /// Send again after `after`, which is already clamped to what is left of the
    /// episode's deadline — the shell sleeps it as it stands.
    Retry { after: Duration },
    /// The endpoint cannot serve this request: advance the failover chain. A new
    /// episode starts with the new endpoint.
    AdvanceFailover { reason: &'static str },
    /// Stop sending. `kind` says whether the chain ran out — the outer re-run
    /// budget may still retry the round later — or whether no send can help.
    Stop {
        kind: StopKind,
        reason: &'static str,
    },
    /// Fail the round: no profile can serve this request.
    Fatal { reason: &'static str },
}

/// Which stopping verdict this is: the shell arms a re-run only for the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopKind {
    /// The episode's own budget or deadline ran out: the shell walks the chain,
    /// and a chain with no candidate left ends the round.
    ChainExhausted,
    /// A local configuration error: it fails on every profile, so a switch
    /// cannot help and the round is not re-run.
    ExternalOnly,
}

/// Why the episode stopped sending: its own budget, in the phase it was spent.
const BUDGET_EXHAUSTED: &str = "retry budget exhausted";

const BUDGET_EXHAUSTED_MID_STREAM: &str = "mid-stream retry budget exhausted";

const DEADLINE_EXHAUSTED: &str = "retry deadline exhausted";

/// The shape row with no key to compare: the failure cannot be named, so the
/// row reads as a repeat.
const SHAPE_UNIDENTIFIED: &str = "unidentifiable response shape, so another send would repeat it";

/// The upper bound a wait's jitter is drawn from. The draw itself happens in the
/// shell: this module keeps the bound as a value so the schedule stays pure.
pub(crate) const RETRY_JITTER_MAX: Duration = Duration::from_secs(1);

/// The backoff schedule before clamping: `min(base * 2^n, max_delay) + jitter`,
/// with `n` the exponent the caller supplies: the sends already made in the
/// episode for a retry, or the outer re-run index for the round-level wait.
/// The jitter rides outside the cap, so the capped plateau carries it too;
/// `max_delay` stays the schedule's own upper bound.
pub(crate) fn backoff_schedule(policy: &RetryPolicy, exponent: u32, jitter: Duration) -> Duration {
    let exp_delay = policy
        .base_delay
        .saturating_mul(1u32.checked_shl(exponent).unwrap_or(u32::MAX));
    exp_delay.min(policy.max_delay).saturating_add(jitter)
}

/// The wait a retry performs: the schedule, clamped to what is left of the
/// episode's deadline. The clamp lives here, not in the shell, so no caller can
/// sleep past the deadline — and `saturating_sub` keeps a backwards clock from
/// underflowing it.
fn retry_after(facts: &Facts, policy: &RetryPolicy) -> Duration {
    let exponent = facts.attempts.saturating_sub(1);
    backoff_schedule(policy, exponent, facts.jitter)
        .min(policy.retry_timeout.saturating_sub(facts.elapsed))
}

/// The verdict for one attempt: what the shell does next. The single exit of
/// the retry logic.
///
/// Order, fixed: the shared table first, then the pre-send reading (nothing has
/// failed yet), then the class rows, and inside the retry row the episode's
/// budget before its deadline. A class row outranks that gate: a failure another
/// send cannot help with leaves through its own exit even on the episode's last
/// send. A fact outranks the class rows, because it is what another agent
/// already paid for.
pub(crate) fn decide(facts: &Facts, policy: &RetryPolicy) -> Decision {
    // A fact the endpoint already carries: this attempt does not send. The one
    // exception is the wake-triggered execution's first attempt, which sends
    // exactly once anyway — a user asking is always served, at the price of one
    // bounded attempt on an endpoint that may still refuse.
    if let Some(fact) = facts.blocked.as_ref()
        && !(facts.origin == AttemptOrigin::External && facts.failure.is_none())
    {
        return blocked_exit(facts, &fact.block);
    }
    let Some(class) = facts.failure else {
        return if facts.attempts >= max_sends(policy) {
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: budget_exhausted_reason(facts.phase),
            }
        } else {
            Decision::Proceed
        };
    };
    match class {
        FailureClass::Transient(_) => retry_or_stop(facts, policy),
        // A shape-class failure is endpoint-level once the same shape has
        // already failed in this episode; a shape whose key cannot be computed
        // has no comparison to make, so it reads the same way.
        FailureClass::ResponseShape => match facts.shape {
            Some(key) if shape_sends_allowed(facts.previous_shape, key) > 0 => {
                retry_or_stop(facts, policy)
            }
            Some(_) => Decision::AdvanceFailover {
                reason: class.label(),
            },
            None => Decision::AdvanceFailover {
                reason: SHAPE_UNIDENTIFIED,
            },
        },
        // A throttle the upstream left open-ended spends this episode's sends
        // before the chain is charged. The allowance is clamped by the budget
        // the send path's head guard enforces, so a verdict past it can never
        // come back as a spent budget.
        FailureClass::Throttled => {
            if facts.attempts < THROTTLE_SENDS_ALLOWED.min(max_sends(policy)) {
                retry_or_stop(facts, policy)
            } else {
                Decision::AdvanceFailover {
                    reason: class.label(),
                }
            }
        }
        FailureClass::QuotaBlocked | FailureClass::EndpointPermanent => Decision::AdvanceFailover {
            reason: class.label(),
        },
        FailureClass::LocalConfig => Decision::Stop {
            kind: StopKind::ExternalOnly,
            reason: class.label(),
        },
        FailureClass::RequestPermanent | FailureClass::Unclassified => Decision::Fatal {
            reason: class.label(),
        },
    }
}

/// The exit a fact leaves: advance when the chain has a candidate, stop when it
/// does not. The candidate was resolved by the shell against the table, which
/// keys one fact per profile, so a fact only ever rules out the profile it
/// names.
fn blocked_exit(facts: &Facts, block: &Block) -> Decision {
    let reason = fact_reason(block.kind);
    match facts.alternative {
        Some(_) => Decision::AdvanceFailover { reason },
        None => Decision::Stop {
            kind: StopKind::ExternalOnly,
            reason,
        },
    }
}

/// What a fact says, in the operator's words: the reason a stop or a switch
/// reports. One wording per kind, because the reader acts differently on each —
/// an exhausted account is waited out, a rejected endpoint is replaced.
fn fact_reason(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Quota => "upstream quota exhausted for this account",
        BlockKind::Payment => "upstream payment required for this account",
        BlockKind::EndpointAuth => "endpoint credentials rejected",
        BlockKind::EndpointShape => "endpoint response shape rejected",
    }
}

/// The retry row's own gate: the episode's budget first, its deadline second,
/// and a wait only if both allow it. A failure that ends the episode schedules
/// no retry, so nothing is recorded for a send that will not happen.
fn retry_or_stop(facts: &Facts, policy: &RetryPolicy) -> Decision {
    if facts.attempts >= max_sends(policy) {
        return Decision::Stop {
            kind: StopKind::ChainExhausted,
            reason: budget_exhausted_reason(facts.phase),
        };
    }
    if facts.elapsed >= policy.retry_timeout {
        return Decision::Stop {
            kind: StopKind::ChainExhausted,
            reason: DEADLINE_EXHAUSTED,
        };
    }
    Decision::Retry {
        after: retry_after(facts, policy),
    }
}

/// The budget reason names the phase: a mid-stream drop spends the same
/// allowance as a prepare/send failure, but reads differently in the log.
fn budget_exhausted_reason(phase: Phase) -> &'static str {
    match phase {
        Phase::Connect => BUDGET_EXHAUSTED,
        Phase::MidStream => BUDGET_EXHAUSTED_MID_STREAM,
    }
}

#[cfg(test)]
mod tests {
    use super::super::attempt::{Attempt, AttemptOrigin, Phase, TableReading};
    use super::super::classify::{classify, shape_key};
    use super::super::failure_class::{FailureClass, TransientKind};
    use super::super::ledger::Episode;
    use super::super::test_support::{policy, provider_status};
    use super::*;
    use crate::retry::RetryPolicy;
    use reqwest::StatusCode;
    use std::time::Duration;

    #[test]
    fn a_pre_send_verdict_reads_the_ledger_alone() {
        let policy = policy(2, 1, 5, 60);
        let fresh = Episode::new(1_000);
        assert_eq!(
            decide(
                &facts(&fresh, Phase::Connect, None, None, Duration::ZERO),
                &policy
            ),
            Decision::Proceed
        );

        let spent = fresh.after_attempt().after_attempt().after_attempt();
        assert_eq!(
            decide(
                &facts(&spent, Phase::Connect, None, None, Duration::ZERO),
                &policy
            ),
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: "retry budget exhausted"
            },
            "a spent episode may not send"
        );
    }

    #[test]
    fn a_transient_failure_retries_until_the_episode_is_spent() {
        let policy = policy(2, 1, 5, 60);
        let transient = FailureClass::Transient(TransientKind::Server);
        let first = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &first,
                    Phase::Connect,
                    Some(transient),
                    None,
                    Duration::from_millis(500)
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_millis(1_500)
            },
            "the first retry waits one base delay plus the injected jitter"
        );

        // The last send of the episode: its failure exhausts the budget, and the
        // reason names the phase it failed in.
        let last = Episode::new(1_000)
            .after_attempt()
            .after_attempt()
            .after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &last,
                    Phase::MidStream,
                    Some(transient),
                    None,
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: "mid-stream retry budget exhausted"
            }
        );

        // An episode older than `retry_timeout` stops before any wait.
        let stale = Episode::new(900).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &stale,
                    Phase::Connect,
                    Some(transient),
                    None,
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: "retry deadline exhausted"
            }
        );
    }

    #[test]
    fn the_wait_is_clamped_to_the_remaining_deadline() {
        // The schedule asks for 30s; two seconds of the episode's deadline are left.
        let policy = policy(3, 30, 30, 10);
        let episode = Episode::new(992).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &episode,
                    Phase::Connect,
                    Some(FailureClass::Transient(TransientKind::Transport)),
                    None,
                    Duration::from_secs(5)
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(2)
            }
        );
    }

    /// A late status rejection carries a key, so the shape row's one retry is
    /// real: the first late 400 retries, the same status after that advances.
    #[test]
    fn a_late_status_rejection_gets_one_shape_retry() {
        let policy = policy(3, 1, 5, 60);
        let error = provider_status(StatusCode::BAD_REQUEST, "");
        assert_eq!(
            shape_key(&error),
            Some(ShapeKey::StatusRejected(StatusCode::BAD_REQUEST))
        );
        let class = classify(&error, Phase::MidStream);
        assert_eq!(class, FailureClass::ResponseShape);

        let first = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &first,
                    Phase::MidStream,
                    Some(class),
                    shape_key(&error),
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(1)
            },
            "the first late rejection gets the shape row's one retry"
        );

        // The slot now holds that status, so the same rejection is endpoint-level.
        let spent = first.shape_failed(ShapeKey::StatusRejected(StatusCode::BAD_REQUEST));
        assert_eq!(
            decide(
                &facts(
                    &spent,
                    Phase::MidStream,
                    Some(class),
                    shape_key(&error),
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::AdvanceFailover {
                reason: class.label()
            }
        );
    }

    #[test]
    fn the_jitter_rides_outside_the_cap() {
        let policy = policy(3, 1, 1, 60);
        let episode = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &episode,
                    Phase::Connect,
                    Some(FailureClass::Transient(TransientKind::Server)),
                    None,
                    Duration::from_millis(700)
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_millis(1_700)
            },
            "the capped plateau carries the jitter too"
        );
    }

    #[test]
    fn the_class_rows_pick_the_exit() {
        let policy = policy(3, 1, 5, 60);
        let episode = Episode::new(1_000).after_attempt();
        let verdict = |class: FailureClass| {
            decide(
                &facts(&episode, Phase::Connect, Some(class), None, Duration::ZERO),
                &policy,
            )
        };

        assert_eq!(
            verdict(FailureClass::QuotaBlocked),
            Decision::AdvanceFailover {
                reason: "upstream quota or payment rejection"
            }
        );
        assert_eq!(
            verdict(FailureClass::EndpointPermanent),
            Decision::AdvanceFailover {
                reason: "endpoint or credentials rejected the request"
            }
        );
        assert_eq!(
            verdict(FailureClass::LocalConfig),
            Decision::Stop {
                kind: StopKind::ExternalOnly,
                reason: "local client configuration error"
            },
            "a local misconfiguration fails on every profile: stop, do not switch"
        );
        assert_eq!(
            verdict(FailureClass::RequestPermanent),
            Decision::Fatal {
                reason: "the request itself was rejected"
            }
        );
        assert_eq!(
            verdict(FailureClass::Unclassified),
            Decision::Fatal {
                reason: "unclassified failure"
            },
            "an unknown failure is not retried"
        );
    }

    /// The exit a second shape failure earns, and the one a shape with no
    /// computable key earns: the subject is the verdict, the ledger is the
    /// input it reads.
    #[test]
    fn a_shape_failure_retries_once_then_reads_as_endpoint_level() {
        let policy = policy(3, 1, 5, 60);
        let episode = Episode::new(1_000).after_attempt();
        let shape = FailureClass::ResponseShape;
        let body = Some(ShapeKey::BodyStructure);
        assert_eq!(
            decide(
                &facts(&episode, Phase::Connect, Some(shape), body, Duration::ZERO),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(1)
            },
            "the first failure of a shape retries once"
        );

        let spent = episode.shape_failed(ShapeKey::BodyStructure);
        assert_eq!(
            decide(
                &facts(&spent, Phase::Connect, Some(shape), body, Duration::ZERO),
                &policy
            ),
            Decision::AdvanceFailover {
                reason: "the response shape failed twice in this episode"
            }
        );
        assert_eq!(
            decide(
                &facts(
                    &spent,
                    Phase::Connect,
                    Some(shape),
                    Some(ShapeKey::FrameStructure),
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(1)
            },
            "a different shape starts a fresh allowance"
        );
        assert_eq!(
            decide(
                &facts(&episode, Phase::Connect, Some(shape), None, Duration::ZERO),
                &policy
            ),
            Decision::AdvanceFailover {
                reason: "unidentifiable response shape, so another send would repeat it"
            },
            "a shape whose key cannot be computed has no repeat to compare"
        );
    }

    /// A throttle is the one quota-shaped failure worth another send: this
    /// episode spends its allowance on the same endpoint, then the chain is
    /// charged.
    /// The subject is the verdict; the ledger it reads is the input.
    #[test]
    fn a_throttle_spends_its_allowance_then_advances() {
        let policy = policy(4, 1, 5, 60);
        let throttle = FailureClass::Throttled;
        let first = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(&first, Phase::Connect, Some(throttle), None, Duration::ZERO),
                &policy
            ),
            Decision::Retry {
                after: Duration::from_secs(1)
            },
            "the throttled send is repeated once"
        );

        let spent = first.after_attempt();
        assert_eq!(
            decide(
                &facts(&spent, Phase::Connect, Some(throttle), None, Duration::ZERO),
                &policy
            ),
            Decision::AdvanceFailover {
                reason: "upstream throttled the request with no reset moment"
            },
            "the allowance, not the episode budget, is what ends the retry"
        );
    }

    /// The allowance may not outvote the episode's own budget: the send path's
    /// head guard reads a verdict past it as a spent budget and fails the round,
    /// so a one-send budget advances instead of retrying.
    /// The subject is the verdict; the ledger it reads is the input.
    #[test]
    fn a_throttle_never_votes_past_the_episode_budget() {
        let policy = policy(0, 1, 5, 60);
        let first = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &first,
                    Phase::Connect,
                    Some(FailureClass::Throttled),
                    None,
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::AdvanceFailover {
                reason: "upstream throttled the request with no reset moment"
            }
        );
    }

    /// The deadline governs the throttle retry as it governs every other one.
    /// The subject is the verdict; the ledger it reads is the input.
    #[test]
    fn a_throttle_retry_still_obeys_the_episode_deadline() {
        let policy = policy(4, 1, 5, 0);
        let first = Episode::new(1_000).after_attempt();
        assert_eq!(
            decide(
                &facts(
                    &first,
                    Phase::Connect,
                    Some(FailureClass::Throttled),
                    None,
                    Duration::ZERO
                ),
                &policy
            ),
            Decision::Stop {
                kind: StopKind::ChainExhausted,
                reason: DEADLINE_EXHAUSTED
            }
        );
    }

    /// A fact the table carries outranks the ledger: a system-initiated attempt
    /// does not send, and with nowhere to go it stops on the fact's own wording.
    #[test]
    fn a_fact_the_table_carries_stops_a_system_attempt() {
        let verdict = pre_send_verdict(
            AttemptOrigin::System,
            reading(Some(blocked(BlockKind::Quota, None)), None),
            &policy(10, 1, 60, 300),
        );
        assert_eq!(
            verdict,
            Decision::Stop {
                kind: StopKind::ExternalOnly,
                reason: "upstream quota exhausted for this account",
            }
        );
    }

    /// The same hit with a candidate the shell resolved: the chain advances, and
    /// the reason still names the fact.
    #[test]
    fn a_fact_with_a_candidate_advances_the_chain() {
        let verdict = pre_send_verdict(
            AttemptOrigin::System,
            reading(
                Some(blocked(BlockKind::EndpointShape, None)),
                Some(Alternative),
            ),
            &policy(10, 1, 60, 300),
        );
        assert_eq!(
            verdict,
            Decision::AdvanceFailover {
                reason: "endpoint response shape rejected",
            }
        );
    }

    /// The wake-triggered execution's first attempt sends anyway: a user asking is
    /// served once, whatever the table says.
    #[test]
    fn the_wake_first_attempt_sends_through_a_fact() {
        let verdict = pre_send_verdict(
            AttemptOrigin::External,
            reading(Some(blocked(BlockKind::Quota, None)), None),
            &policy(10, 1, 60, 300),
        );
        assert_eq!(verdict, Decision::Proceed);
    }

    /// Every kind the table can carry has its own wording, because the reader
    /// acts differently on each.
    #[test]
    fn every_fact_kind_names_its_own_reason() {
        assert_eq!(
            fact_reason(BlockKind::Quota),
            "upstream quota exhausted for this account"
        );
        assert_eq!(
            fact_reason(BlockKind::Payment),
            "upstream payment required for this account"
        );
        assert_eq!(
            fact_reason(BlockKind::EndpointAuth),
            "endpoint credentials rejected"
        );
        assert_eq!(
            fact_reason(BlockKind::EndpointShape),
            "endpoint response shape rejected"
        );
    }

    /// A verdict input for [`decide`]: the ledger's projection with the injected
    /// attempt values. `jitter` is zero unless a case asks for it, so the
    /// expected waits are exact.
    fn facts(
        episode: &Episode,
        phase: Phase,
        failure: Option<FailureClass>,
        shape: Option<ShapeKey>,
        jitter: Duration,
    ) -> Facts {
        let attempt = match failure {
            Some(class) => Attempt::failed(phase, class, shape, 1_000, jitter),
            None => Attempt::pre_send(phase, AttemptOrigin::System, 1_000, TableReading::none()),
        };
        episode.facts(&attempt)
    }

    /// A fact the shared table carries for a profile.
    fn blocked(kind: BlockKind, until: Option<u64>) -> BlockedFact {
        BlockedFact {
            block: Block {
                kind,
                until,
                detail: "seeded evidence".to_owned(),
                status: None,
                recorded_at: 1_000,
            },
            profile: "p1".to_owned(),
        }
    }

    /// The pre-send reading: what the table says, and whether the shell found a
    /// candidate to move to.
    fn reading(blocked: Option<BlockedFact>, alternative: Option<Alternative>) -> TableReading {
        TableReading {
            blocked,
            alternative,
        }
    }

    /// The verdict for a pre-send reading at `now`.
    fn pre_send_verdict(
        origin: AttemptOrigin,
        reading: TableReading,
        policy: &RetryPolicy,
    ) -> Decision {
        let episode = Episode::new(1_000);
        let attempt = Attempt::pre_send(Phase::Connect, origin, 1_000, reading);
        decide(&episode.facts(&attempt), policy)
    }
}
