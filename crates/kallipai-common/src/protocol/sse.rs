//! SSE wire-format events for tagma-to-client streaming.

use serde::{Deserialize, Serialize};

use crate::approval::ApprovalStatus;

/// Distinguishable cause for within-set failover chain exhaustion.
///
/// Carried by [`SseEvent::FailoverChainExhausted`] (and, on the runtime side, by the matching
/// `AgentOutcome` / `FailoverOutcome` variants) so operators can tell apart the structurally
/// distinct exhaustion modes instead of seeing a generic error.
///
/// Defined here (not in the runtime) because it is part of the serialized event contract, so the
/// wire crate must own the taxonomy — the same shape as [`ApprovalStatus`], which is shared
/// between the runtime and the tagma API. Fieldless and `Copy` (common has no `anyhow`); the
/// trigger text rides in a separate `detail: String` on the carrying event. Typed here (rather
/// than a free-text `reason: String` like [`SseEvent::Failover`]) because the exhaustion *states*
/// are enumerable and clients branch on them, whereas a failover hop's cause is an opaque
/// per-backend error.
///
/// **Terminal-reason coalescing:** a chain that skips candidates for *mixed*
/// reasons surfaces the subtler mode: [`AllCandidatesInfeasible`](Self::AllCandidatesInfeasible)
/// when any candidate's window could not serve the budget, else
/// [`AllCandidatesUnbuildable`](Self::AllCandidatesUnbuildable) when any backend refused to
/// build, else [`AllBackupsExhausted`](Self::AllBackupsExhausted) when every candidate ahead
/// was already marked in the shared fact table. The per-candidate `warn!`s in
/// `advance_failover` carry each skip's precise cause. A single infeasible candidate wins
/// the reason even when the majority were unbuildable: the window-infeasibility mode is
/// the subtler, more actionable one to surface (the operator can retune the budget shape
/// without redeploying credentials). Intentional — do not "fix" it to last-reason or
/// counts without revisiting the operator-UX rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailoverChainExhaustion {
    /// The set has a single profile — failover was never configured.
    NoFailoverConfigured,
    /// A multi-profile set with no candidate left to move to: the chain was advanced
    /// through and its tail failed terminally, or every candidate ahead was already
    /// marked in the shared fact table (switching into one would only repeat the
    /// failure another agent already paid for).
    AllBackupsExhausted,
    /// Remaining candidate profiles existed but every one's backend refused to build
    /// (configuration / credential failure, distinct from the runtime trigger).
    AllCandidatesUnbuildable,
    /// Remaining candidate profiles existed and every one's declared `max_context_window` violated
    /// a budget invariant (e.g. `summary_max_tokens` exceeds the pinned budget at that window) —
    /// tune `summary_max_tokens` / `pinned_budget_ratio` or raise the window. Distinct from
    /// [`AllCandidatesUnbuildable`](Self::AllCandidatesUnbuildable): these candidates build fine,
    /// their window just can't serve the configured budget shape.
    AllCandidatesInfeasible,
}

impl std::fmt::Display for FailoverChainExhaustion {
    /// Operator-readable lowercase prose, shared by the CLI and `kallipai-run` so
    /// both surfaces render the same cause identically.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::NoFailoverConfigured => "no failover configured",
            Self::AllBackupsExhausted => "all backups exhausted",
            Self::AllCandidatesUnbuildable => "all failover candidates unbuildable",
            Self::AllCandidatesInfeasible => "all failover candidates had infeasible windows",
        };
        f.write_str(s)
    }
}

/// Chain-transient retry arming carried by
/// [`SseEvent::FailoverChainExhausted`] when the runtime armed a delayed retry
/// (the attempt budget is not yet spent) instead of parking the agent for good;
/// the status surface mirrors the same shape as its `retrying` field.
/// `retry_in_secs` is the armed backoff delay -- a relative duration, not an
/// absolute timestamp, so the payload stays valid whenever it is read. `None`
/// on the event keeps the pre-retry wire shape (see
/// `fce_transient_retry_roundtrip_and_legacy_shape`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TransientRetryInfo {
    pub attempt: u32,
    pub max_attempts: u32,
    pub retry_in_secs: f64,
}

/// The structured reason a quota or payment block parks an agent: which profile
/// hit the wall, the moment the upstream said it reopens, and the evidence.
/// Carried by [`SseEvent::QuotaBlocked`] and, on the runtime side, by the
/// matching `AgentEvent`, so the payload has one shape and one owner: the event
/// that reports it.
///
/// `reset` is the *display* face of the upstream's hint ([`crate::facts::display_until`]):
/// a moment further out than the trust bound reads as `None`, which is what
/// "the upstream did not say, or said something nobody should believe" means to
/// every consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaBlockedInfo {
    /// The profile that hit the wall.
    pub profile: String,
    /// When the upstream said service resumes, as a value a reader may show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<u64>,
    /// Bounded excerpt of the upstream's own words.
    pub detail: String,
}

/// Wire-format event for SSE transport (tagma to client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SseEvent {
    Reasoning {
        content: String,
    },
    AssistantContent {
        content: String,
    },
    AssistantContentDelta {
        delta: String,
    },
    ReasoningDelta {
        delta: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult {
        result: String,
    },
    /// The agent yielded control (called the `break` tool, or was force-idled by
    /// the no-progress guardrail). Content-less: a message to the user is now a
    /// deliberate `kallip lesche send` CLI call observed via
    /// [`ToolCall`](Self::ToolCall)/ [`ToolResult`](Self::ToolResult), not the
    /// final assistant message. This event
    /// is a pure status transition — the task parks, awaiting external input.
    Idle,
    /// The agent deliberately parked itself waiting (via `break(wait)`) with an
    /// armed timer. Terminal for the turn like [`Idle`](Self::Idle), but the agent
    /// has unfinished business: it wakes on the timer (an injected `[system]`
    /// turn -- the agent then decides what to do) or on any external event, and
    /// `break(idle)` finishes. Contrast the runtime-driven
    /// [`Retrying`](Self::Retrying) backoff, which is non-terminal.
    Waiting {
        timeout_secs: u64,
    },
    MaxRoundsExceeded,
    Error {
        message: String,
    },
    Status {
        message: String,
    },
    Busy,
    ApprovalUpdated {
        id: String,
        status: ApprovalStatus,
    },
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: String,
        delay_secs: f64,
    },
    /// The LLM stream dropped mid-way (transport error after content started flowing) and the
    /// runner is retrying the same request: a delta continuation when an anchor arrived, the
    /// full context when the drop came first.
    /// Downstream must abandon the partial assistant/reasoning content accumulated since the last
    /// boundary (fold or discard it) before the retried stream renders. Non-terminal; the agent
    /// stays busy. Fields mirror [`Retrying`](Self::Retrying) plus the carried `error`.
    StreamReset {
        error: String,
        attempt: u32,
        max_attempts: u32,
        delay_secs: f64,
    },
    /// Within-set failover: the active profile failed terminally and the runner advanced to the
    /// next profile in the set's chain. Non-terminal — the agent stays busy and continues the
    /// turn on the new profile. `from`/`to` are profile ids.
    Failover {
        from: String,
        to: String,
        reason: String,
    },
    Cancelled,
    /// The current round was interrupted; the agent stays alive and idle, ready for the
    /// next prompt. Distinct from `Cancelled`, which is terminal (remove/shutdown).
    Interrupted,
    TokenBudgetExceeded {
        consumed: u64,
        budget: u64,
    },
    /// Within-set failover chain exhausted: the active profile failed terminally and no
    /// buildable backup remained. Terminal for the turn (the agent goes idle) but **not**
    /// lifecycle-terminal — the agent stays alive and can be re-prompted (e.g. after the
    /// operator reconfigures failover). `reason` distinguishes the cause; `detail` is the
    /// original trigger's `{:#}` display. Distinct from a generic [`Failover`](Self::Failover)
    /// (a non-terminal hop) and from [`Error`](Self::Error) (an undifferentiated turn error).
    FailoverChainExhausted {
        reason: FailoverChainExhaustion,
        detail: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        transient_retry: Option<TransientRetryInfo>,
    },

    /// The endpoint's account is out of quota or credit and no candidate could
    /// serve: the agent parks with the structured reason instead of a free-text
    /// error, so the surfaces can show which endpoint and account, when the
    /// upstream said service resumes, and the evidence. Terminal for the turn.
    /// No transient re-run follows: the park ends when the profile's window
    /// does, on its own timer, and an external trigger wakes it sooner.
    QuotaBlocked(QuotaBlockedInfo),
}
impl SseEvent {
    /// Whether this event terminates the current turn/stream.
    ///
    /// Turn-level, not lifecycle-level: after a terminal event the agent goes
    /// idle, but stays alive and can be re-prompted — only [`Cancelled`](Self::Cancelled)
    /// removes the agent. A new turn is opened by [`Busy`](Self::Busy). The consumers that
    /// branch on this (the one-shot `kallipai-run` stream loop, the tagma bridge's idle mark
    /// via the runtime-side mirror `AgentEvent::is_terminal`) must stay in sync with this
    /// classification.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Idle
                | Self::Waiting { .. }
                | Self::MaxRoundsExceeded
                | Self::Error { .. }
                | Self::Cancelled
                | Self::Interrupted
                | Self::TokenBudgetExceeded { .. }
                | Self::FailoverChainExhausted { .. }
                | Self::QuotaBlocked(..)
        )
    }

    /// Whether this event is a point where the tagma can interject a queued
    /// prompt into the running agent: [`ToolCall`](Self::ToolCall) (the assistant
    /// committed this batch of tool calls, ending the streamed message) or any
    /// terminal event (see [`is_terminal`](Self::is_terminal)). The runtime drains
    /// queued prompts at the top of the next round iteration, so consumers flush
    /// their queues here to land in time. Within-stream retries ([`Retrying`](Self::Retrying),
    /// [`StreamReset`](Self::StreamReset), [`Failover`](Self::Failover)) are not boundaries.
    pub fn is_boundary(&self) -> bool {
        self.is_terminal() || matches!(self, Self::ToolCall { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive classification snapshot for [`SseEvent::is_terminal`] and
    /// [`SseEvent::is_boundary`]. The `let _: () = match` arm set must cover
    /// every variant, so adding a variant without classifying it here (and in
    /// the predicates) is a compile error, not a silent drift — the whole
    /// point of centralizing the classification.
    #[test]
    fn classification_is_exhaustive_and_matches_predicates() {
        for v in ALL_VARIANTS {
            let (terminal, boundary) = (v.is_terminal(), v.is_boundary());
            let _: () = match v {
                // Streaming content — neither terminal nor a boundary.
                SseEvent::Reasoning { .. }
                | SseEvent::AssistantContent { .. }
                | SseEvent::AssistantContentDelta { .. }
                | SseEvent::ReasoningDelta { .. } => {
                    assert!(!terminal && !boundary, "{v:?}");
                }
                // Tool completion — not a boundary (the ToolCall before it was).
                SseEvent::ToolResult { .. } => {
                    assert!(!terminal && !boundary, "{v:?}");
                }
                // Interjection point: queued-prompt flush lands here.
                SseEvent::ToolCall { .. } => {
                    assert!(!terminal && boundary, "{v:?}");
                }
                // Within-stream retries — the agent stays busy through them.
                SseEvent::Retrying { .. }
                | SseEvent::StreamReset { .. }
                | SseEvent::Failover { .. } => {
                    assert!(!terminal && !boundary, "{v:?}");
                }
                // Status transitions — informational only.
                SseEvent::Busy | SseEvent::Status { .. } | SseEvent::ApprovalUpdated { .. } => {
                    assert!(!terminal && !boundary, "{v:?}");
                }
                // Turn-terminal: boundary too (is_boundary = terminal ∪ ToolCall).
                SseEvent::Idle
                | SseEvent::Waiting { .. }
                | SseEvent::MaxRoundsExceeded
                | SseEvent::Error { .. }
                | SseEvent::Cancelled
                | SseEvent::Interrupted
                | SseEvent::TokenBudgetExceeded { .. }
                | SseEvent::FailoverChainExhausted { .. }
                | SseEvent::QuotaBlocked(..) => {
                    assert!(terminal && boundary, "{v:?}");
                }
            };
        }
    }

    /// One instance of every variant, for the exhaustive snapshot above.
    const ALL_VARIANTS: &[SseEvent] = &[
        SseEvent::Reasoning {
            content: String::new(),
        },
        SseEvent::AssistantContent {
            content: String::new(),
        },
        SseEvent::AssistantContentDelta {
            delta: String::new(),
        },
        SseEvent::ReasoningDelta {
            delta: String::new(),
        },
        SseEvent::ToolCall {
            name: String::new(),
            args: String::new(),
        },
        SseEvent::ToolResult {
            result: String::new(),
        },
        SseEvent::Idle,
        SseEvent::Waiting { timeout_secs: 0 },
        SseEvent::MaxRoundsExceeded,
        SseEvent::Error {
            message: String::new(),
        },
        SseEvent::Status {
            message: String::new(),
        },
        SseEvent::Busy,
        SseEvent::ApprovalUpdated {
            id: String::new(),
            status: ApprovalStatus::Committed,
        },
        SseEvent::Retrying {
            attempt: 0,
            max_attempts: 0,
            error: String::new(),
            delay_secs: 0.0,
        },
        SseEvent::StreamReset {
            error: String::new(),
            attempt: 0,
            max_attempts: 0,
            delay_secs: 0.0,
        },
        SseEvent::Failover {
            from: String::new(),
            to: String::new(),
            reason: String::new(),
        },
        SseEvent::Cancelled,
        SseEvent::Interrupted,
        SseEvent::TokenBudgetExceeded {
            consumed: 0,
            budget: 0,
        },
        SseEvent::FailoverChainExhausted {
            reason: FailoverChainExhaustion::NoFailoverConfigured,
            detail: String::new(),
            transient_retry: None,
        },
        SseEvent::QuotaBlocked(QuotaBlockedInfo {
            profile: String::new(),
            reset: Some(0),
            detail: String::new(),
        }),
    ];

    /// Wire-shape snapshot: the serde tag and the exact field-name set of
    /// every variant. This is the cross-language contract surface — external
    /// clients (and a future TS typing of the event stream) branch on these
    /// names, so adding/renaming/removing a variant or field must red here
    /// before it ships. Values are not pinned (semantics live in the
    /// predicate tests above); fixtures reuse [`ALL_VARIANTS`]' empty values.
    #[test]
    fn wire_schema_snapshot() {
        let expected: &[(&str, &[&str])] = &[
            ("reasoning", &["content"]),
            ("assistantContent", &["content"]),
            ("assistantContentDelta", &["delta"]),
            ("reasoningDelta", &["delta"]),
            ("toolCall", &["args", "name"]),
            ("toolResult", &["result"]),
            ("idle", &[]),
            ("waiting", &["timeout_secs"]),
            ("maxRoundsExceeded", &[]),
            ("error", &["message"]),
            ("status", &["message"]),
            ("busy", &[]),
            ("approvalUpdated", &["id", "status"]),
            (
                "retrying",
                &["attempt", "delay_secs", "error", "max_attempts"],
            ),
            (
                "streamReset",
                &["attempt", "delay_secs", "error", "max_attempts"],
            ),
            ("failover", &["from", "reason", "to"]),
            ("cancelled", &[]),
            ("interrupted", &[]),
            ("tokenBudgetExceeded", &["budget", "consumed"]),
            ("failoverChainExhausted", &["detail", "reason"]),
            ("quotaBlocked", &["detail", "profile", "reset"]),
        ];
        assert_eq!(
            ALL_VARIANTS.len(),
            expected.len(),
            "variant count drifted — extend both tables"
        );
        for (v, (tag, fields)) in ALL_VARIANTS.iter().zip(expected) {
            let obj = serde_json::to_value(v).expect("SseEvent serializes");
            let map = obj.as_object().expect("internally tagged -> object");
            let actual_tag = map
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            assert_eq!(actual_tag, *tag, "{v:?}: serde tag drifted");
            let mut actual: Vec<&str> = map
                .keys()
                .map(String::as_str)
                .filter(|k| *k != "type")
                .collect();
            actual.sort_unstable();
            let mut want: Vec<&str> = fields.to_vec();
            want.sort_unstable();
            assert_eq!(actual, want, "{v:?}: field-name set drifted");
        }
    }

    /// `transient_retry` rides the FCE payload only when a retry is armed:
    /// `None` omits the field entirely (the pre-retry wire shape -- payloads
    /// written before the field existed deserialize unchanged), `Some` adds it.
    /// Pinning both directions keeps old clients and old fixtures valid while
    /// making the armed shape a stable contract.
    #[test]
    fn fce_transient_retry_roundtrip_and_legacy_shape() {
        let armed = SseEvent::FailoverChainExhausted {
            reason: FailoverChainExhaustion::AllBackupsExhausted,
            detail: "provider 5xx".into(),
            transient_retry: Some(TransientRetryInfo {
                attempt: 1,
                max_attempts: 3,
                retry_in_secs: 2.5,
            }),
        };
        let json = serde_json::to_value(&armed).expect("serializes");
        assert_eq!(
            json["transient_retry"],
            serde_json::json!({"attempt": 1, "max_attempts": 3, "retry_in_secs": 2.5})
        );
        let back: SseEvent = serde_json::from_value(json).expect("roundtrips");
        match back {
            SseEvent::FailoverChainExhausted {
                transient_retry: Some(info),
                ..
            } => assert_eq!(
                (info.attempt, info.max_attempts, info.retry_in_secs),
                (1, 3, 2.5)
            ),
            other => panic!("wrong variant: {other:?}"),
        }

        let legacy = serde_json::json!({
            "type": "failoverChainExhausted",
            "reason": "allBackupsExhausted",
            "detail": "provider 5xx",
        });
        match serde_json::from_value::<SseEvent>(legacy).expect("legacy shape deserializes") {
            SseEvent::FailoverChainExhausted {
                transient_retry: None,
                ..
            } => {}
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// The enums embedded in the wire shape carry their own contract: their
    /// serialized strings (`status`, `reason`) are what clients branch on.
    /// Each state's string is pinned, and the explicit match arms double as
    /// a compile-time exhaustiveness pin: adding a state without extending
    /// this match is a compile error, not a silent escape to the wire (the
    /// runtime-side consumers match single variants, so no other compile
    /// lock exists for these enums).
    #[test]
    fn embedded_enum_value_domains_snapshot() {
        for v in [
            ApprovalStatus::Pending,
            ApprovalStatus::Committed,
            ApprovalStatus::Approved,
            ApprovalStatus::Denied,
            ApprovalStatus::Redeemed,
            ApprovalStatus::Cancelled,
        ] {
            let _: () = match v {
                ApprovalStatus::Pending => {
                    assert_eq!(serde_json::to_string(&v).unwrap(), "\"pending\"");
                }
                ApprovalStatus::Committed => {
                    assert_eq!(serde_json::to_string(&v).unwrap(), "\"committed\"");
                }
                ApprovalStatus::Approved => {
                    assert_eq!(serde_json::to_string(&v).unwrap(), "\"approved\"");
                }
                ApprovalStatus::Denied => {
                    assert_eq!(serde_json::to_string(&v).unwrap(), "\"denied\"");
                }
                ApprovalStatus::Redeemed => {
                    assert_eq!(serde_json::to_string(&v).unwrap(), "\"redeemed\"");
                }
                ApprovalStatus::Cancelled => {
                    assert_eq!(serde_json::to_string(&v).unwrap(), "\"cancelled\"");
                }
            };
        }
        for v in [
            FailoverChainExhaustion::NoFailoverConfigured,
            FailoverChainExhaustion::AllBackupsExhausted,
            FailoverChainExhaustion::AllCandidatesUnbuildable,
            FailoverChainExhaustion::AllCandidatesInfeasible,
        ] {
            let _: () = match v {
                FailoverChainExhaustion::NoFailoverConfigured => {
                    assert_eq!(
                        serde_json::to_string(&v).unwrap(),
                        "\"noFailoverConfigured\""
                    );
                }
                FailoverChainExhaustion::AllBackupsExhausted => {
                    assert_eq!(
                        serde_json::to_string(&v).unwrap(),
                        "\"allBackupsExhausted\""
                    );
                }
                FailoverChainExhaustion::AllCandidatesUnbuildable => {
                    assert_eq!(
                        serde_json::to_string(&v).unwrap(),
                        "\"allCandidatesUnbuildable\""
                    );
                }
                FailoverChainExhaustion::AllCandidatesInfeasible => {
                    assert_eq!(
                        serde_json::to_string(&v).unwrap(),
                        "\"allCandidatesInfeasible\""
                    );
                }
            };
        }
    }

    /// The quota-blocked payload rides the event flat, under the camelCase field
    /// names the wire contract uses: pinning the exact bytes is what makes a
    /// rename or a nesting mistake red before a client sees it.
    #[test]
    fn quota_blocked_wire_bytes_are_pinned() {
        let info = QuotaBlockedInfo {
            profile: "p1".to_owned(),
            reset: Some(1_700_000_600),
            detail: "HTTP 429 slow down".to_owned(),
        };
        let json = serde_json::to_string(&SseEvent::QuotaBlocked(info)).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"quotaBlocked\",\"profile\":\"p1\",\"reset\":1700000600,\"detail\":\"HTTP 429 slow down\"}"
        );
    }

    /// A missing (or untrustworthy) reset moment omits the field entirely, so an
    /// old client reading the payload sees the same shape it always did — the
    /// same rule the armed transient-retry payload follows.
    #[test]
    fn a_quota_block_without_a_usable_reset_omits_the_field() {
        let info = QuotaBlockedInfo {
            profile: "p1".to_owned(),
            reset: None,
            detail: "HTTP 429".to_owned(),
        };
        let json = serde_json::to_string(&SseEvent::QuotaBlocked(info)).unwrap();
        assert!(!json.contains("reset"), "{json}");
        let back: SseEvent = serde_json::from_str(&json).unwrap();
        let SseEvent::QuotaBlocked(back) = back else {
            panic!("round-trips as the same variant");
        };
        assert_eq!(back.reset, None);
        assert_eq!(back.profile, "p1");
    }
}
