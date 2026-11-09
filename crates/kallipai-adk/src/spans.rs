//! The spans a wake is made of.
//!
//! Every span opens on [`SPAN_TARGET`], so an exporter records it whatever the
//! log filter says. Every name is low cardinality: the agent's role may appear,
//! while ids, indices and outcomes travel as attributes.
//!
//! Attributes borrow the OpenTelemetry GenAI names where the conventions have
//! one (`gen_ai.*`) and use `kallipai.*` for what this product adds. No
//! attribute carries conversation text: not a prompt, not a tool's arguments or
//! result, not a model's answer, not a credential. The trace shows what ran,
//! how long it took, and how it ended.

use just_llm_client::types::generation::Usage;
use kallipai_common::AgentId;
use kallipai_common::telemetry::{SPAN_TARGET, record_attribute};
use tracing::Span;

/// The agent's id as text. A context that has none yet reports `unbound`, the
/// same word the lifecycle line uses for it.
pub(crate) fn agent_label(agent_id: Option<&AgentId>) -> String {
    agent_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "unbound".to_string())
}

/// The span of one wake: everything a single [`run_and_report`] does.
///
/// The role is the low-cardinality half of an agent's identity, so it rides in
/// the name; the id it belongs to is an attribute. The origin is `external` or
/// `system`, the two ways a wake starts.
///
/// [`run_and_report`]: crate::agent_task::run_and_report
pub(crate) fn wake_span(role: &str, agent_id: &str, origin: &'static str) -> Span {
    let name = format!("invoke_agent {role}");
    tracing::info_span!(
        target: SPAN_TARGET,
        "wake",
        otel.name = %name,
        gen_ai.operation.name = "invoke_agent",
        gen_ai.agent.id = agent_id,
        gen_ai.agent.name = role,
        gen_ai.conversation.id = agent_id,
        kallipai.wake.origin = origin,
    )
}

/// The span of one round inside a wake. The wire calls a round a turn: the span
/// is named `turn` and its attributes are `kallipai.turn.*`.
///
/// A round is the unit the budget, the tools and the history record are
/// organised around. The index is an attribute rather than part of the name, so
/// that every round of every agent aggregates into one series.
pub(crate) fn turn_span(role: &str, agent_id: &str, round: usize) -> Span {
    tracing::info_span!(
        target: SPAN_TARGET,
        "turn",
        otel.name = "turn",
        gen_ai.agent.id = agent_id,
        gen_ai.agent.name = role,
        gen_ai.conversation.id = agent_id,
        kallipai.turn.index = round as u64,
    )
}

/// The span of one request to a model endpoint.
///
/// The model rides in the name, so a trace of an agent that switched profiles
/// reads which model answered. The endpoint is the `gen_ai.provider.name`
/// attribute and the profile's own id is `kallipai.profile.id`.
/// One span covers one request. A send the retry shell repeats stays in it; a
/// request made again from the top of the acquisition loop opens its own, and
/// that is every request after a mid-stream failure or a failover.
pub(crate) fn chat_span(model: &str, provider: &str, profile_id: &str, agent_id: &str) -> Span {
    let name = format!("chat {model}");
    tracing::info_span!(
        target: SPAN_TARGET,
        "chat",
        otel.name = %name,
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = provider,
        gen_ai.request.model = model,
        gen_ai.agent.id = agent_id,
        gen_ai.conversation.id = agent_id,
        kallipai.profile.id = profile_id,
    )
}

/// The span of one tool call.
///
/// The tool name is low cardinality and stays a name. The call id pairs this
/// span with the result the agent received, and the outcome word says how the
/// call ended; neither carries the arguments or the result themselves.
pub(crate) fn tool_span(tool: &str, call_id: &str, agent_id: &str) -> Span {
    let name = format!("execute_tool {tool}");
    tracing::info_span!(
        target: SPAN_TARGET,
        "execute_tool",
        otel.name = %name,
        gen_ai.operation.name = "execute_tool",
        gen_ai.tool.name = tool,
        gen_ai.tool.call.id = call_id,
        gen_ai.agent.id = agent_id,
        gen_ai.conversation.id = agent_id,
    )
}

/// Write how a tool call ended: `success`, `failed`, `timeout`, `deferred`,
/// `cancelled` or `skipped`. A later word replaces an earlier one on the same
/// span.
pub(crate) fn record_tool_outcome(span: &Span, outcome: &str) {
    record_attribute(span, "kallipai.tool.outcome", outcome);
}

/// Charge one send to the chat span around this call.
///
/// Written on every send, so the span ends holding the total: setting a key
/// twice keeps the last value. The count belongs to this request alone — a
/// failover to another profile opens a new chat span, and its sends never land
/// in this one.
pub(crate) fn count_chat_send(span: &Span, sends: u32) {
    record_attribute(span, "kallipai.chat.attempts", sends.to_string());
}

/// Write a response's token counts onto its chat span.
///
/// A counter the provider did not report stays absent rather than reading as
/// zero: "no cache was used" and "the provider said nothing about cache" are
/// different facts about the request.
pub(crate) fn record_usage(span: &Span, usage: &Usage) {
    record_attribute(
        span,
        "gen_ai.usage.input_tokens",
        usage.prompt_tokens.to_string(),
    );
    record_attribute(
        span,
        "gen_ai.usage.output_tokens",
        usage.completion_tokens.to_string(),
    );
    if let Some(tokens) = usage.cache_read_tokens {
        record_attribute(
            span,
            "gen_ai.usage.cache_read.input_tokens",
            tokens.to_string(),
        );
    }
    if let Some(tokens) = usage.cache_write_tokens {
        record_attribute(
            span,
            "gen_ai.usage.cache_write.input_tokens",
            tokens.to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use just_llm_client::LlmBackend;
    use kallipai_common::AgentId;
    use kallipai_common::telemetry::test_observability::ObservedSpan;

    use crate::agent_task::{AgentContext, WakeDisposition, run_and_report};
    use crate::event::AgentEvent;
    use crate::history::HistoryWriter;
    use crate::retry::AttemptOrigin;
    use crate::test_support::{
        GatedTool, MapSource, RecordingBackend, ctx_from_source_with_tools, install_recorder,
        profile, usage_with_every_counter, user_msg,
    };

    fn named<'a>(spans: &'a [ObservedSpan], name: &str) -> &'a ObservedSpan {
        spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("the {name} span: {spans:#?}"))
    }

    /// The context a tree test runs its wake in: one profile `a` served through
    /// provider `p1`, the given tools, a role unique to the test, and a history
    /// so the turns are recorded.
    async fn wake_context(
        backend: Arc<RecordingBackend>,
        tools: Vec<Box<dyn just_llm_client::LlmTool>>,
        role: &str,
    ) -> (AgentContext, AgentId, tempfile::TempDir) {
        let source = Arc::new(MapSource(std::collections::HashMap::from([(
            "p1".to_string(),
            backend as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source_with_tools(
            vec![profile("a", "p1", 100_000)],
            source,
            crate::retry::RetryPolicy::default(),
            tools,
        )
        .await;
        let agent_id = AgentId::random();
        ctx.config.agent_id = Some(agent_id.clone());
        ctx.config.role = role.to_string();
        ctx.config.max_tool_rounds = 3;
        let dir = tempfile::tempdir().unwrap();
        ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));
        ctx.record_turn(vec![user_msg("q1")]).await;
        (ctx, agent_id, dir)
    }

    /// The spans of one wake's trace.
    fn wake_spans<'a>(spans: &'a [ObservedSpan], wake: &ObservedSpan) -> Vec<&'a ObservedSpan> {
        spans
            .iter()
            .filter(|span| span.trace_id == wake.trace_id)
            .collect()
    }

    /// The round span at `index` inside one wake's trace.
    fn round<'a>(mine: &[&'a ObservedSpan], index: &str) -> &'a ObservedSpan {
        mine.iter()
            .find(|span| {
                span.name == "turn" && span.attribute("kallipai.turn.index") == Some(index)
            })
            .unwrap_or_else(|| panic!("round {index}: {mine:#?}"))
    }

    /// The span exported as `name` hanging directly under `parent`. A span
    /// carries its `otel.name`, so a chat or a tool reads with its model or
    /// tool in the name.
    fn under<'a>(mine: &[&'a ObservedSpan], name: &str, parent: &ObservedSpan) -> &'a ObservedSpan {
        mine.iter()
            .find(|span| {
                span.name == name && span.parent_span_id.as_deref() == Some(parent.span_id.as_str())
            })
            .unwrap_or_else(|| panic!("the {name} span under a round: {mine:#?}"))
    }

    /// One wake, two rounds: the wake is the root of the trace, and each round
    /// hangs under it, in the order the loop ran them.
    #[tokio::test]
    async fn a_wake_is_the_root_and_its_rounds_hang_under_it() {
        // The role is unique to this test, so a neighbour's wake cannot be
        // mistaken for it.
        let recorder = install_recorder();

        let backend = RecordingBackend::new();
        let gate = GatedTool::new("hold");
        // The tool does not hold here: the permit is stored before the round
        // reaches it, so the round runs straight through.
        gate.release.notify_one();
        let role = format!("dev-{}", AgentId::random());
        let (mut ctx, agent_id, _dir) =
            wake_context(backend.clone(), vec![Box::new(gate)], &role).await;

        // Round 0 runs the tool, round 1 breaks the run.
        backend.queue_tool_call("hold", "{}", usage_with_every_counter(11, 5, 0, 0, 0), None);
        backend.queue_tool_call(
            "break",
            "{\"timeout_secs\":30}",
            usage_with_every_counter(21, 9, 0, 0, 0),
            None,
        );
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);

        let disposition =
            run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;
        assert_ne!(
            disposition,
            WakeDisposition::Terminated,
            "the wake served its rounds"
        );

        let spans = recorder.observed();
        let id = agent_id.to_string();
        let wake = named(&spans, &format!("invoke_agent {role}"));
        assert_eq!(
            wake.parent_span_id, None,
            "a wake with no context around it is the root: {spans:#?}"
        );
        assert_eq!(
            wake.attribute("gen_ai.operation.name"),
            Some("invoke_agent")
        );
        assert_eq!(wake.attribute("gen_ai.agent.id"), Some(id.as_str()));
        assert_eq!(wake.attribute("gen_ai.conversation.id"), Some(id.as_str()));
        assert_eq!(wake.attribute("kallipai.wake.origin"), Some("system"));
        assert_eq!(wake.attribute("kallipai.wake.outcome"), Some("awake"));
        assert_eq!(wake.attribute("kallipai.outcome"), Some("ok"));
        assert_eq!(wake.status, "unset");

        let rounds: Vec<&ObservedSpan> = spans
            .iter()
            .filter(|span| span.name == "turn" && span.trace_id == wake.trace_id)
            .collect();
        assert_eq!(rounds.len(), 2, "one round span per round: {spans:#?}");
        for (index, round) in rounds.iter().enumerate() {
            assert_eq!(
                round.attribute("kallipai.turn.index"),
                Some(index.to_string().as_str())
            );
            assert_eq!(
                round.parent_span_id.as_deref(),
                Some(wake.span_id.as_str()),
                "round {index} hangs under its wake"
            );
            assert_eq!(round.trace_id, wake.trace_id);
        }
        assert_eq!(
            rounds[0].attribute("kallipai.turn.outcome"),
            Some("continue")
        );
        assert_eq!(rounds[1].attribute("kallipai.turn.outcome"), Some("break"));
        for round in &rounds {
            assert_eq!(round.attribute("kallipai.outcome"), Some("ok"));
            assert_eq!(round.status, "unset");
        }
    }

    /// A round's model call and its tool call hang under that round, and the
    /// chat carries what a reader needs: which model, through which endpoint,
    /// how many sends it took, and what it cost.
    #[tokio::test]
    async fn chat_and_tool_spans_hang_under_their_round() {
        let recorder = install_recorder();

        let backend = RecordingBackend::new();
        let gate = GatedTool::new("hold");
        gate.release.notify_one();
        let role = format!("tools-{}", AgentId::random());
        let (mut ctx, agent_id, _dir) =
            wake_context(backend.clone(), vec![Box::new(gate)], &role).await;

        // Round 0 asks for the tool and reports every counter a provider can
        // give; round 1 breaks the run.
        backend.queue_tool_call("hold", "{}", usage_with_every_counter(11, 5, 3, 2, 0), None);
        backend.queue_tool_call(
            "break",
            "{\"timeout_secs\":30}",
            usage_with_every_counter(21, 9, 0, 0, 0),
            None,
        );
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let disposition =
            run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;
        assert_ne!(
            disposition,
            WakeDisposition::Terminated,
            "the wake served its rounds"
        );

        let spans = recorder.observed();
        let wake = named(&spans, &format!("invoke_agent {role}"));
        let mine = wake_spans(&spans, wake);
        let first = round(&mine, "0");
        let chat = under(&mine, "chat a-model", first);
        let tool = under(&mine, "execute_tool hold", first);

        assert_eq!(chat.attribute("gen_ai.operation.name"), Some("chat"));
        assert_eq!(chat.attribute("gen_ai.request.model"), Some("a-model"));
        assert_eq!(chat.attribute("gen_ai.provider.name"), Some("p1"));
        assert_eq!(chat.attribute("kallipai.profile.id"), Some("a"));
        assert_eq!(
            chat.attribute("gen_ai.agent.id"),
            Some(agent_id.to_string().as_str())
        );
        assert_eq!(chat.attribute("kallipai.chat.attempts"), Some("1"));
        assert_eq!(chat.attribute("gen_ai.usage.input_tokens"), Some("11"));
        assert_eq!(chat.attribute("gen_ai.usage.output_tokens"), Some("5"));
        assert_eq!(
            chat.attribute("gen_ai.usage.cache_read.input_tokens"),
            Some("3")
        );
        assert_eq!(
            chat.attribute("gen_ai.usage.cache_write.input_tokens"),
            Some("2")
        );
        assert_eq!(chat.attribute("kallipai.outcome"), Some("ok"));
        assert_eq!(chat.status, "unset");

        assert_eq!(
            tool.attribute("gen_ai.operation.name"),
            Some("execute_tool")
        );
        assert_eq!(tool.attribute("gen_ai.tool.name"), Some("hold"));
        assert_eq!(tool.attribute("gen_ai.tool.call.id"), Some("call-1"));
        assert_eq!(tool.attribute("kallipai.tool.outcome"), Some("success"));
        assert_eq!(tool.attribute("kallipai.outcome"), Some("ok"));
        assert_eq!(tool.status, "unset");

        // The next round opens its own chat under its own round, not under the
        // one before it.
        let second = round(&mine, "1");
        let next_chat = under(&mine, "chat a-model", second);
        assert_eq!(next_chat.attribute("kallipai.chat.attempts"), Some("1"));
        assert_eq!(next_chat.attribute("gen_ai.usage.input_tokens"), Some("21"));
    }

    /// A model call that cannot be served ends its chat span red with a class
    /// code, and its round ends red with its own: the trace answers which round
    /// failed and how, without a log line.
    #[tokio::test]
    async fn a_failed_chat_carries_a_class_code_and_a_red_round() {
        /// The closed set `FailureClass::code()` names, locked by its own test.
        const CLASS_CODES: [&str; 8] = [
            "transient",
            "quota_blocked",
            "throttled",
            "endpoint_permanent",
            "request_permanent",
            "local_config",
            "response_shape",
            "unclassified",
        ];

        let recorder = install_recorder();

        let backend = RecordingBackend::new();
        let role = format!("fail-{}", AgentId::random());
        let (mut ctx, _agent_id, _dir) = wake_context(backend.clone(), Vec::new(), &role).await;

        // The one round's model call cannot be served at all.
        backend.queue_error(just_llm_client::BackendError::invalid_request(
            "the request was rejected",
        ));
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        // The failing round's disposition is the failure's own; the trace
        // is what this test reads.
        let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

        let spans = recorder.observed();
        let wake = named(&spans, &format!("invoke_agent {role}"));
        let mine = wake_spans(&spans, wake);
        let failed_round = round(&mine, "0");
        let chat = under(&mine, "chat a-model", failed_round);

        assert_eq!(
            failed_round.attribute("kallipai.turn.outcome"),
            Some("error")
        );
        assert_eq!(
            failed_round.attribute("error.type"),
            Some("turn_failed"),
            "a failed turn names its class"
        );
        assert!(
            failed_round.status.starts_with("error: "),
            "the round is red: {}",
            failed_round.status
        );
        assert_eq!(chat.attribute("kallipai.outcome"), Some("failed"));
        assert!(
            chat.status.starts_with("error: "),
            "the chat is red: {}",
            chat.status
        );
        let code = chat
            .attribute("error.type")
            .expect("a failed chat names its class");
        assert_eq!(code, "request_permanent", "a rejected request is permanent");
        assert!(
            CLASS_CODES.contains(&code),
            "a code off the closed set: {code}"
        );
    }

    /// No span carries conversation text: a sentinel planted in the prompt and in
    /// a tool's arguments never reaches an attribute, because a trace outlives
    /// the turn it describes.
    #[tokio::test]
    async fn no_span_carries_conversation_text() {
        const SENTINEL: &str = "SENTINEL-c0ffee-do-not-export";

        let recorder = install_recorder();

        let backend = RecordingBackend::new();
        let gate = GatedTool::new("hold");
        gate.release.notify_one();
        let role = format!("leak-{}", AgentId::random());
        let (mut ctx, _agent_id, _dir) =
            wake_context(backend.clone(), vec![Box::new(gate)], &role).await;
        ctx.record_turn(vec![user_msg(SENTINEL)]).await;

        backend.queue_tool_call(
            "hold",
            &format!("{{\"note\":\"{SENTINEL}\"}}"),
            usage_with_every_counter(7, 3, 0, 0, 0),
            None,
        );
        backend.queue_tool_call(
            "break",
            "{\"timeout_secs\":30}",
            usage_with_every_counter(9, 4, 0, 0, 0),
            None,
        );
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        // The failing round's disposition is the failure's own; the trace
        // is what this test reads.
        let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

        let spans = recorder.observed();
        let wake = named(&spans, &format!("invoke_agent {role}"));
        let mine = wake_spans(&spans, wake);
        assert!(
            mine.iter().any(|span| span.name == "execute_tool hold"),
            "the tool ran: {mine:#?}"
        );
        for span in &mine {
            for (key, value) in &span.attributes {
                assert!(
                    !value.contains(SENTINEL),
                    "{} carries conversation text in {key}: {value}",
                    span.name
                );
            }
        }
    }

    /// A model request cancelled while it is in flight is neither a failure nor
    /// a round that finished. `abandoned` is the fallback for a scope that never
    /// said how it ended, so a cancel has to say it: otherwise the commonest
    /// shape of an interrupt is filed as a forgotten verdict.
    #[tokio::test]
    async fn a_request_cancelled_in_flight_reads_as_cancelled() {
        let recorder = install_recorder();

        let backend = RecordingBackend::new();
        // The request is in flight: nothing below the acquisition loop can see
        // the cancel arrive, so the loop's own cancel arm is what answers.
        let release = backend.queue_hanging_send();
        let role = format!("cancel-{}", AgentId::random());
        let (mut ctx, _agent_id, _dir) = wake_context(backend.clone(), Vec::new(), &role).await;

        let lifecycle = ctx.cancel.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            lifecycle.cancel();
        });
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;
        canceller.await.unwrap();
        release.notify_one();

        let spans = recorder.observed();
        let wake = named(&spans, &format!("invoke_agent {role}"));
        let mine = wake_spans(&spans, wake);
        let cancelled_round = round(&mine, "0");
        assert_eq!(
            cancelled_round.attribute("kallipai.turn.outcome"),
            Some("cancelled"),
            "a cancelled round says so, rather than reading as a park"
        );
        assert_eq!(
            cancelled_round.attribute("kallipai.outcome"),
            Some("cancelled")
        );
        assert_eq!(cancelled_round.status, "unset");

        let chat = under(&mine, "chat a-model", cancelled_round);
        assert_eq!(chat.attribute("kallipai.outcome"), Some("cancelled"));
        assert_eq!(chat.status, "unset", "a cancel is not an error: {chat:#?}");
        assert_eq!(chat.attribute("error.type"), None);
    }

    /// A tool that reports its own failure and a tool the run loop had to cut
    /// off read as two different outcomes: the flag `run_tool_bounded` returns
    /// is what keeps a hung call from reading as a refusal.
    #[tokio::test]
    async fn a_failed_tool_and_a_timed_out_tool_read_differently() {
        struct Refuse;
        #[async_trait::async_trait]
        impl just_llm_client::LlmTool for Refuse {
            fn name(&self) -> &str {
                "refuse"
            }
            fn description(&self) -> &str {
                "Report a failure of its own."
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            async fn call(&self, _args_json: &str) -> anyhow::Result<String> {
                anyhow::bail!("the tool refused")
            }
        }

        struct Slow;
        #[async_trait::async_trait]
        impl just_llm_client::LlmTool for Slow {
            fn name(&self) -> &str {
                "slow"
            }
            fn description(&self) -> &str {
                "Never answer in time."
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            async fn call(&self, _args_json: &str) -> anyhow::Result<String> {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok("late".to_owned())
            }
        }

        let recorder = install_recorder();

        let backend = RecordingBackend::new();
        let role = format!("tools-{}", AgentId::random());
        let (mut ctx, _agent_id, _dir) = wake_context(
            backend.clone(),
            vec![Box::new(Refuse), Box::new(Slow)],
            &role,
        )
        .await;
        // One second is enough for the slow tool to blow through.
        ctx.config.tool_timeout_secs = 1;

        backend.queue_tool_call(
            "refuse",
            "{}",
            usage_with_every_counter(3, 1, 0, 0, 0),
            None,
        );
        backend.queue_tool_call("slow", "{}", usage_with_every_counter(4, 1, 0, 0, 0), None);
        backend.queue_tool_call(
            "break",
            "{\"timeout_secs\":30}",
            usage_with_every_counter(5, 2, 0, 0, 0),
            None,
        );
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let _ = run_and_report(&mut ctx, &tx, &mut prompt_rx, AttemptOrigin::System).await;

        let spans = recorder.observed();
        let wake = named(&spans, &format!("invoke_agent {role}"));
        let mine = wake_spans(&spans, wake);

        let tool_at = |name: &str| {
            mine.iter()
                .find(|span| span.name == name)
                .copied()
                .unwrap_or_else(|| panic!("the {name} span: {mine:#?}"))
        };
        let failed = tool_at("execute_tool refuse");
        assert_eq!(failed.attribute("kallipai.tool.outcome"), Some("failed"));
        assert_eq!(failed.attribute("kallipai.outcome"), Some("failed"));
        assert_eq!(failed.attribute("error.type"), Some("tool_failed"));
        assert!(failed.status.starts_with("error: "), "{}", failed.status);

        let timed_out = tool_at("execute_tool slow");
        assert_eq!(
            timed_out.attribute("kallipai.tool.outcome"),
            Some("timeout")
        );
        assert_eq!(timed_out.attribute("kallipai.outcome"), Some("failed"));
        assert_eq!(timed_out.attribute("error.type"), Some("tool_timeout"));
    }
}
