//! The external-event projector: maps the tagma's internal `SseEvent` stream
//! onto the transport-neutral external vocabulary a frontend consumes.
//!
//! Splits each internal event by destination channel so the
//! encrypted/plaintext boundary is type-enforced:
//!
//! - the **authored** half ([`AuthoredEvent`]) is conversation content — it
//!   rides the encrypted E2EE envelope and is persisted in `chat_history`;
//! - the **system** half ([`SignalEvent`]) is operator metadata — it rides the
//!   plaintext signal channel, is logged for observability, and is never
//!   persisted in `chat_history`.
//!
//! Variants outside the external capability set (streaming deltas, tool
//! events, retry/failover telemetry, approval updates, and the dead `Status`)
//! map to neither and are dropped with a `debug!` so a new tagma event never
//! vanishes silently. Shared by both serving paths: the relay pump (events
//! cross the E2EE relay) and the direct external SSE (events served locally,
//! no relay). The two halves are mutually exclusive today: an event is either
//! authored content or a runtime signal, never both.

use kallip_common::protocol::{AuthoredEvent, SignalEvent, SseEvent};
use tracing::debug;

/// Project a tagma-internal `SseEvent` onto the external vocabulary. See the
/// module docs for the channel split.
pub(crate) fn project_external(sse: &SseEvent) -> (Option<AuthoredEvent>, Option<SignalEvent>) {
    let authored = match sse {
        SseEvent::AssistantContent { content } => Some(AuthoredEvent::AssistantContent {
            content: content.clone(),
        }),
        _ => None,
    };
    let system = match sse {
        SseEvent::Idle => Some(SignalEvent::Idle),
        SseEvent::Busy => Some(SignalEvent::Busy),
        SseEvent::Error { message } => Some(SignalEvent::Error {
            message: message.clone(),
        }),
        SseEvent::Interrupted => Some(SignalEvent::Interrupted),
        SseEvent::Cancelled => Some(SignalEvent::Cancelled),
        SseEvent::TokenBudgetExceeded { consumed, budget } => {
            Some(SignalEvent::TokenBudgetExceeded {
                consumed: *consumed,
                budget: *budget,
            })
        }
        SseEvent::MaxRoundsExceeded => Some(SignalEvent::MaxRoundsExceeded),
        SseEvent::FailoverChainExhausted { reason, detail } => {
            Some(SignalEvent::FailoverChainExhausted {
                reason: *reason,
                detail: detail.clone(),
            })
        }
        // AssistantContent is authored content, not a signal — explicitly None
        // so it does not fall through to the `other` arm's "dropped" log.
        SseEvent::AssistantContent { .. } => None,
        other => {
            debug!(
                target: "tagma.projector.drop",
                event = ?other,
                "dropping out-of-capability tagma event"
            );
            None
        }
    };
    if let Some(ref signal) = system {
        debug!(target: "tagma.projector.signal", event = ?signal, "projected system signal");
    }
    (authored, system)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_common::protocol::FailoverChainExhaustion;

    #[test]
    fn project_external_splits_authored_and_signal() {
        // Authored content -> authored half only.
        assert!(matches!(
            project_external(&SseEvent::AssistantContent {
                content: "c".into()
            }),
            (Some(AuthoredEvent::AssistantContent { .. }), None)
        ));
        // System signals -> signal half only, no authored, never persisted.
        assert!(matches!(
            project_external(&SseEvent::Busy),
            (None, Some(SignalEvent::Busy))
        ));
        assert!(matches!(
            project_external(&SseEvent::Interrupted),
            (None, Some(SignalEvent::Interrupted))
        ));
        assert!(matches!(
            project_external(&SseEvent::Cancelled),
            (None, Some(SignalEvent::Cancelled))
        ));
        assert!(matches!(
            project_external(&SseEvent::MaxRoundsExceeded),
            (None, Some(SignalEvent::MaxRoundsExceeded))
        ));
        assert!(matches!(
            project_external(&SseEvent::TokenBudgetExceeded {
                consumed: 1,
                budget: 2
            }),
            (
                None,
                Some(SignalEvent::TokenBudgetExceeded {
                    consumed: 1,
                    budget: 2
                })
            )
        ));
        assert!(matches!(
            project_external(&SseEvent::Error {
                message: "m".into()
            }),
            (None, Some(SignalEvent::Error { .. }))
        ));
        {
            let (a, s) = project_external(&SseEvent::Error {
                message: "m".into(),
            });
            assert!(a.is_none());
            match s {
                Some(SignalEvent::Error { message }) => assert_eq!(message, "m"),
                other => panic!("expected Error, got {other:?}"),
            }
        }
        {
            let (a, s) = project_external(&SseEvent::FailoverChainExhausted {
                reason: FailoverChainExhaustion::NoFailoverConfigured,
                detail: "d".into(),
            });
            assert!(a.is_none());
            match s {
                Some(SignalEvent::FailoverChainExhausted { reason, detail }) => {
                    assert!(matches!(
                        reason,
                        FailoverChainExhaustion::NoFailoverConfigured
                    ));
                    assert_eq!(detail, "d");
                }
                other => panic!("expected FailoverChainExhausted, got {other:?}"),
            }
        }
        // Dropped (out-of-capability) variants map to neither half.
        assert!(matches!(
            project_external(&SseEvent::ToolCall {
                name: "x".into(),
                args: "{}".into()
            }),
            (None, None)
        ));
        assert!(matches!(
            project_external(&SseEvent::AssistantContentDelta { delta: "d".into() }),
            (None, None)
        ));
        assert!(matches!(
            project_external(&SseEvent::ApprovalUpdated {
                id: "a".into(),
                status: kallip_common::approval::ApprovalStatus::Pending
            }),
            (None, None)
        ));
    }
}
