//! The adk's sink: every wake, model and tool record as an instrument.
//!
//! The adk records at its boundaries, deep inside the acquisition loop. A
//! histogram cannot be rebuilt from a total, so this sink takes each record at
//! the call site instead of reading atomics afterwards. The bucket edges are
//! the adk's own, so the shape an operator queries does not change with the
//! reporting path.

use std::sync::Arc;
use std::time::Duration;

use kallipai_adk::metrics::{
    AdkSink, FIRST_TOKEN_BUCKETS, InFlight, MODEL_BUCKETS, ModelOutcome, TOOL_BUCKETS, TokenKind,
    ToolOutcome, WAKE_BUCKETS, WakeOutcome,
};
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

/// One instrument per adk family, built once from the process meter.
pub(crate) struct OtelAdkSink {
    wakes: Counter<u64>,
    wake_duration: Histogram<f64>,
    model_requests: Counter<u64>,
    model_duration: Histogram<f64>,
    first_token: Histogram<f64>,
    tokens: Counter<u64>,
    in_flight: UpDownCounter<i64>,
    tool_calls: Counter<u64>,
    tool_duration: Histogram<f64>,
}

impl OtelAdkSink {
    pub(crate) fn new(meter: &Meter) -> Arc<Self> {
        let wakes = meter
            .u64_counter("kallipai_tagma.agent_wakes")
            .with_description("Agent wakes by terminal outcome; one count per finished wake.")
            .build();
        let wake_duration = meter
            .f64_histogram("kallipai_tagma.agent_wake_duration")
            .with_unit("s")
            .with_description("Agent wake duration, by terminal outcome.")
            .with_boundaries(WAKE_BUCKETS.to_vec())
            .build();
        let model_requests = meter
            .u64_counter("kallipai_tagma.model_requests")
            .with_description("Model requests by outcome; one count per send attempt.")
            .build();
        let model_duration = meter
            .f64_histogram("kallipai_tagma.model_request_duration")
            .with_unit("s")
            .with_description(
                "Model request duration, from the send that started the attempt to its end, every outcome together.",
            )
            .with_boundaries(MODEL_BUCKETS.to_vec())
            .build();
        let first_token = meter
            .f64_histogram("kallipai_tagma.model_first_token")
            .with_unit("s")
            .with_description(
                "Wait from sending a request to the first streamed delta, for the stream that was adopted.",
            )
            .with_boundaries(FIRST_TOKEN_BUCKETS.to_vec())
            .build();
        let tokens = meter
            .u64_counter("kallipai_tagma.model_tokens")
            .with_description("Tokens by kind, as the provider reported them.")
            .build();
        let in_flight = meter
            .i64_up_down_counter("kallipai_tagma.model_in_flight")
            .with_description("Model requests in flight right now.")
            .build();
        let tool_calls = meter
            .u64_counter("kallipai_tagma.tool_calls")
            .with_description("Tool calls by outcome; one count per call.")
            .build();
        let tool_duration = meter
            .f64_histogram("kallipai_tagma.tool_call_duration")
            .with_unit("s")
            .with_description("Tool call duration by outcome.")
            .with_boundaries(TOOL_BUCKETS.to_vec())
            .build();
        Arc::new(Self {
            wakes,
            wake_duration,
            model_requests,
            model_duration,
            first_token,
            tokens,
            in_flight,
            tool_calls,
            tool_duration,
        })
    }
}

impl AdkSink for OtelAdkSink {
    fn record_wake(&self, outcome: WakeOutcome, duration: Duration) {
        let labels = [KeyValue::new("outcome", outcome.as_str())];
        self.wakes.add(1, &labels);
        self.wake_duration.record(duration.as_secs_f64(), &labels);
    }

    fn model_in_flight(&self) -> InFlight {
        self.in_flight.add(1, &[]);
        let gauge = self.in_flight.clone();
        InFlight::on_drop(move || gauge.add(-1, &[]))
    }

    fn record_model_call(&self, outcome: ModelOutcome, duration: Duration) {
        let labels = [KeyValue::new("outcome", outcome.as_str())];
        self.model_requests.add(1, &labels);
        self.model_duration.record(duration.as_secs_f64(), &[]);
    }

    fn record_first_token(&self, duration: Duration) {
        self.first_token.record(duration.as_secs_f64(), &[]);
    }

    fn record_tokens(&self, kind: TokenKind, tokens: u64) {
        self.tokens
            .add(tokens, &[KeyValue::new("kind", kind.as_str())]);
    }

    fn record_tool_call(&self, outcome: ToolOutcome, duration: Duration) {
        let labels = [KeyValue::new("outcome", outcome.as_str())];
        self.tool_calls.add(1, &labels);
        self.tool_duration.record(duration.as_secs_f64(), &labels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::test_meter::{
        self, Recorded, counter, gauge, histogram_bounds, histogram_count,
    };

    /// One wake reaches the counter under its outcome and the duration under
    /// the adk's own edges.
    #[test]
    fn a_wake_lands_in_its_counter_and_its_histogram() {
        let (metrics, capture, provider) = test_meter::local();
        let adk = metrics.adk();
        adk.record_wake(WakeOutcome::Completed, Duration::from_millis(1500));

        provider.force_flush().expect("the reader flushes");
        let samples = capture.snapshot();
        assert_eq!(
            counter(
                &samples,
                "kallipai_tagma.agent_wakes",
                &[("outcome", "completed")]
            ),
            1,
            "one finished wake is one count"
        );
        assert_eq!(
            histogram_count(&samples, "kallipai_tagma.agent_wake_duration"),
            1
        );
        assert_eq!(
            histogram_bounds(&samples, "kallipai_tagma.agent_wake_duration"),
            Some(WAKE_BUCKETS.to_vec()),
            "the edges are the adk's, not the instrument's default"
        );
    }

    /// The in-flight gauge is given back whichever way the request ends.
    #[test]
    fn the_in_flight_counter_returns_to_zero() {
        let (metrics, capture, provider) = test_meter::local();
        let adk = metrics.adk();
        {
            let _first = adk.model_in_flight();
            let _second = adk.model_in_flight();
            provider.force_flush().expect("the reader flushes");
            assert_eq!(
                gauge(&capture.snapshot(), "kallipai_tagma.model_in_flight", &[]),
                2.0
            );
        }
        provider.force_flush().expect("the reader flushes");
        assert_eq!(
            gauge(&capture.snapshot(), "kallipai_tagma.model_in_flight", &[]),
            0.0
        );
    }

    /// Token kinds and tool outcomes keep their own label values.
    #[test]
    fn labels_follow_the_record_they_came_from() {
        let (metrics, capture, provider) = test_meter::local();
        let adk = metrics.adk();
        adk.record_tokens(TokenKind::CacheRead, 40);
        adk.record_tool_call(ToolOutcome::Timeout, Duration::from_millis(5));

        provider.force_flush().expect("the reader flushes");
        let samples = capture.snapshot();
        assert_eq!(
            counter(
                &samples,
                "kallipai_tagma.model_tokens",
                &[("kind", "cache_read")]
            ),
            40
        );
        assert_eq!(
            counter(
                &samples,
                "kallipai_tagma.tool_calls",
                &[("outcome", "timeout")]
            ),
            1
        );
    }

    /// One attempt's duration is one series whatever outcome it ends in: a
    /// quantile over the histogram reads every outcome together, which is
    /// what the instrument's description promises.
    #[test]
    fn the_model_duration_is_one_series_for_every_outcome() {
        let (metrics, capture, provider) = test_meter::local();
        let adk = metrics.adk();
        adk.record_model_call(ModelOutcome::Ok, Duration::from_millis(700));
        adk.record_model_call(ModelOutcome::Error, Duration::from_millis(900));

        provider.force_flush().expect("the reader flushes");
        let samples = capture.snapshot();
        let durations: Vec<_> = samples
            .iter()
            .filter(|sample| {
                matches!(
                    sample,
                    Recorded::Histogram { name, .. }
                        if name == "kallipai_tagma.model_request_duration"
                )
            })
            .collect();
        assert_eq!(durations.len(), 1, "both outcomes land in one series");
        assert!(
            !durations.iter().any(|sample| matches!(
                sample,
                Recorded::Histogram { attributes, .. }
                    if attributes.iter().any(|(key, _)| key == "outcome")
            )),
            "no outcome label, so a quantile sums the outcomes"
        );
        assert_eq!(
            histogram_count(&samples, "kallipai_tagma.model_request_duration"),
            2,
            "both attempts count in that one series"
        );
    }
}
