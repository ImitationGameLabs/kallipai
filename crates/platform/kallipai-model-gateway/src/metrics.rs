//! The macro-observability face: Prometheus collectors written from
//! the pingora phase boundaries of the forwarding path.
//!
//! One `Metrics` value owns its collectors and the registry they are
//! registered in. Production builds on the process default registry
//! (so every `prometheus::gather()` caller sees the families); tests
//! build on a private registry, which keeps assertions isolated from
//! any other collector in the process. Recording is synchronous -- the
//! pingora phases call inline; there is no background task.
//!
//! Label vocabularies are static (endpoint 3, forward outcome 11,
//! identity outcome 4, connect error class 4, upstream status class 4,
//! API family 4 -- the registry's validation set). The one
//! credential-sourced label is `upstream`: the upstream URL authority
//! (host and port) verbatim. The failure domain is the service behind
//! that address, so two providers on one host share a series and extra
//! base paths never split one.
//! `api_family` labels the attribution family only: connection and
//! establishment measurements do not vary with the request dialect,
//! and the label there would multiply series without information.

use std::time::Duration;

use prometheus::{
    HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder,
};

use crate::forward::dialect::Endpoint;

/// Bucket edges for the end-to-end durations (forward duration and
/// time-to-first-header): sized for LLM traffic, where local models
/// answer in well under a second and reasoning streams run for minutes.
/// The library default would put essentially every sample in the first
/// bucket. Recalibrate against real quantiles once production samples
/// exist (operator review after the first deployment).
const FORWARD_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// Bucket edges for connection establishment: the whole operation lands
/// well under ten seconds, and the fine low end separates a LAN dial
/// from a TLS handshake.
const CONNECT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The terminal outcome of one forwarding request: the label values of
/// `kallipai_model_gateway_forward_total`. The values are mutually exclusive and
/// exhaustive -- every request that routes onto a forwarding endpoint
/// is counted under exactly one of them, once, at the `logging` phase
/// (the single terminal record point; the phases before it only stage
/// facts into the request context).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ForwardOutcome {
    /// The identity face rejected the bearer, or the identity
    /// store failed behind it (the 401 family).
    GatedAuth,
    /// Selection rejected the request: unknown profile, no selected
    /// collection, no default set, no serving member, family mismatch
    /// -- the request never reached an upstream (the 404/405/400
    /// family).
    GatedRoute,
    /// The target profile or default set is outside the caller's
    /// visibility domain (403).
    GatedVisibility,
    /// The target profile is parked (403): a platform state, not an
    /// error.
    GatedParking,
    /// The declared request body exceeded the forwarding bound (413).
    GatedPayload,
    /// The upstream answered 2xx.
    Upstream2xx,
    /// The upstream answered 4xx.
    Upstream4xx,
    /// The upstream answered 5xx.
    Upstream5xx,
    /// The upstream answered something else (3xx and unusual codes).
    UpstreamOther,
    /// The upstream could not be reached: connection establishment
    /// failed.
    ConnectError,
    /// The forward failed on the gateway side after the gates: an
    /// internal selection failure (store error, missing credential,
    /// unparseable prefix) or a mid-stream proxy error.
    ProxyError,
}

impl ForwardOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::GatedAuth => "gated_auth",
            Self::GatedRoute => "gated_route",
            Self::GatedVisibility => "gated_visibility",
            Self::GatedParking => "gated_parking",
            Self::GatedPayload => "gated_payload",
            Self::Upstream2xx => "upstream_2xx",
            Self::Upstream4xx => "upstream_4xx",
            Self::Upstream5xx => "upstream_5xx",
            Self::UpstreamOther => "upstream_other",
            Self::ConnectError => "connect_error",
            Self::ProxyError => "proxy_error",
        }
    }

    /// The upstream status-line class. Everything outside 2xx/4xx/5xx
    /// (3xx redirects, unusual codes) is `UpstreamOther`.
    pub(crate) fn from_upstream_status(status: u16) -> Self {
        match status {
            200..=299 => Self::Upstream2xx,
            400..=499 => Self::Upstream4xx,
            500..=599 => Self::Upstream5xx,
            _ => Self::UpstreamOther,
        }
    }
}

/// The resolution outcome of one forwarding-identity authentication:
/// the label values of `kallipai_model_gateway_identity_resolutions_total`. Derived
/// from the rejection status so a future 403 stays counted without a
/// vocabulary change; the current chain produces 401 and 5xx only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityOutcome {
    Ok,
    Unauthorized,
    Forbidden,
    Error,
}

impl IdentityOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::Error => "error",
        }
    }

    pub(crate) fn from_status(status: u16) -> Self {
        match status {
            401 => Self::Unauthorized,
            403 => Self::Forbidden,
            _ => Self::Error,
        }
    }
}

/// Why an upstream connection attempt failed: the label values of
/// `kallipai_model_gateway_upstream_connect_failures_total`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectErrorClass {
    Refused,
    Timeout,
    Tls,
    Other,
}

impl ConnectErrorClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Timeout => "timeout",
            Self::Tls => "tls",
            Self::Other => "other",
        }
    }
}

/// The upstream status-line class: the label values of
/// `kallipai_model_gateway_upstream_responses_total`. A closed
/// four-value vocabulary; codes outside the three classes (3xx and
/// unusual) share `other` so per-upstream sums close.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpstreamStatusClass {
    S2xx,
    S4xx,
    S5xx,
    Other,
}

impl UpstreamStatusClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::S2xx => "2xx",
            Self::S4xx => "4xx",
            Self::S5xx => "5xx",
            Self::Other => "other",
        }
    }

    /// The status-line class; everything outside 2xx/4xx/5xx is
    /// `Other`, mirroring the forward table's residual bucket.
    pub(crate) fn from_status(status: u16) -> Self {
        match status {
            200..=299 => Self::S2xx,
            400..=499 => Self::S4xx,
            500..=599 => Self::S5xx,
            _ => Self::Other,
        }
    }
}

/// The endpoint label: the wire family a visitor would name, not the
/// crate's path spelling (`/v1/messages` is the anthropic wire).
pub(crate) fn endpoint_label(endpoint: Endpoint) -> &'static str {
    match endpoint {
        Endpoint::ChatCompletions => "chat",
        Endpoint::Responses => "responses",
        Endpoint::Messages => "anthropic",
    }
}

/// The collector family of the forwarding face. Cheap to clone (every
/// handle is a shared counter behind an arc); one instance per process
/// is the intended shape, reached through [`crate::state::AppState`].
#[derive(Clone)]
pub(crate) struct Metrics {
    registry: Registry,
    forward_total: IntCounterVec,
    forward_duration: HistogramVec,
    time_to_first_header: HistogramVec,
    connect_duration: HistogramVec,
    connect_failures: IntCounterVec,
    in_flight: IntGauge,
    identity_resolutions: IntCounterVec,
    upstream_responses: IntCounterVec,
}

impl Metrics {
    /// Production form: everything registers into the process default
    /// registry, which the `/metrics` endpoint gathers.
    pub(crate) fn default_registry() -> Self {
        Self::on(prometheus::default_registry().clone())
    }

    /// Test form: a private registry, so assertions read only what the
    /// test itself recorded.
    #[cfg(test)]
    pub(crate) fn private() -> Self {
        Self::on(Registry::new())
    }

    fn on(registry: Registry) -> Self {
        let forward_total = IntCounterVec::new(
            Opts::new(
                "kallipai_model_gateway_forward_total",
                "Forwarding requests by endpoint and terminal outcome; one count per request at the logging phase.",
            ),
            &["endpoint", "outcome"],
        )
        .expect("a statically named counter");
        let forward_duration = HistogramVec::new(
            HistogramOpts::new(
                "kallipai_model_gateway_forward_duration_seconds",
                "End-to-end forwarding duration (request accepted to the terminal logging phase) by endpoint.",
            )
            .buckets(FORWARD_BUCKETS.to_vec()),
            &["endpoint"],
        )
        .expect("a statically named histogram");
        let time_to_first_header = HistogramVec::new(
            HistogramOpts::new(
                "kallipai_model_gateway_upstream_time_to_first_header_seconds",
                "Time from request acceptance to the first upstream response header, by upstream address.",
            )
            .buckets(FORWARD_BUCKETS.to_vec()),
            &["upstream"],
        )
        .expect("a statically named histogram");
        let connect_duration = HistogramVec::new(
            HistogramOpts::new(
                "kallipai_model_gateway_upstream_connect_duration_seconds",
                "Upstream connection establishment duration (L4 plus TLS, read from the pingora timing digest), by upstream address.",
            )
            .buckets(CONNECT_BUCKETS.to_vec()),
            &["upstream"],
        )
        .expect("a statically named histogram");
        let connect_failures = IntCounterVec::new(
            Opts::new(
                "kallipai_model_gateway_upstream_connect_failures_total",
                "Upstream connection establishment failures by upstream address and error class.",
            ),
            &["upstream", "error_class"],
        )
        .expect("a statically named counter");
        let upstream_responses = IntCounterVec::new(
            Opts::new(
                "kallipai_model_gateway_upstream_responses_total",
                "Upstream response status classes by upstream address and API family, one count per response at the first upstream header; mid-stream breaks after a healthy header land in the forward table's proxy_error, so the two tables do not sum equal.",
            ),
            &["upstream", "api_family", "status_class"],
        )
        .expect("a statically named counter");
        let in_flight = IntGauge::new(
            "kallipai_model_gateway_forward_in_flight",
            "Requests currently selected for forwarding (between selection and the terminal logging phase).",
        )
        .expect("a statically named gauge");
        let identity_resolutions = IntCounterVec::new(
            Opts::new(
                "kallipai_model_gateway_identity_resolutions_total",
                "Tagma-identity resolutions on the forwarding face by outcome.",
            ),
            &["outcome"],
        )
        .expect("a statically named counter");
        let build_info = IntGaugeVec::new(
            Opts::new(
                "kallipai_model_gateway_build_info",
                "Build metadata; the value is always 1, the version label carries the crate version.",
            ),
            &["version"],
        )
        .expect("a statically named gauge");

        registry
            .register(Box::new(forward_total.clone()))
            .expect("registering a statically named collector");
        registry
            .register(Box::new(forward_duration.clone()))
            .expect("registering a statically named collector");
        registry
            .register(Box::new(time_to_first_header.clone()))
            .expect("registering a statically named collector");
        registry
            .register(Box::new(connect_duration.clone()))
            .expect("registering a statically named collector");
        registry
            .register(Box::new(connect_failures.clone()))
            .expect("registering a statically named collector");
        registry
            .register(Box::new(upstream_responses.clone()))
            .expect("registering a statically named collector");
        registry
            .register(Box::new(in_flight.clone()))
            .expect("registering a statically named collector");
        registry
            .register(Box::new(identity_resolutions.clone()))
            .expect("registering a statically named collector");
        registry
            .register(Box::new(build_info.clone()))
            .expect("registering a statically named collector");
        // The process face: cpu, memory, fds, threads, start time. The
        // only source of process-level resource data on hosts without
        // a node exporter. The crate's default registry already ships
        // one (Linux plus the process feature), so that flavor's
        // re-registration is the same face twice, not a conflict.
        match registry.register(Box::new(
            prometheus::process_collector::ProcessCollector::for_self(),
        )) {
            Ok(()) => {}
            Err(prometheus::Error::AlreadyReg) => {}
            Err(e) => panic!("registering the process collector: {e}"),
        }
        build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);

        Self {
            registry,
            forward_total,
            forward_duration,
            time_to_first_header,
            connect_duration,
            connect_failures,
            in_flight,
            identity_resolutions,
            upstream_responses,
        }
    }

    /// The Prometheus text exposition of every family in the registry.
    pub(crate) fn render(&self) -> String {
        TextEncoder::new()
            .encode_to_string(&self.registry.gather())
            .expect(
                "the text encoder only fails on invalid UTF-8, which none of these labels carry",
            )
    }

    pub(crate) fn record_forward(&self, endpoint: Endpoint, outcome: ForwardOutcome) {
        self.forward_total
            .with_label_values(&[endpoint_label(endpoint), outcome.as_str()])
            .inc();
    }

    /// Observed only for requests that were actually forwarded (a
    /// selection exists): instant gate rejections would drown the
    /// long-tail story the histogram exists to tell.
    pub(crate) fn observe_forward_duration(&self, endpoint: Endpoint, duration: Duration) {
        self.forward_duration
            .with_label_values(&[endpoint_label(endpoint)])
            .observe(duration.as_secs_f64());
    }

    pub(crate) fn observe_time_to_first_header(&self, upstream: &str, duration: Duration) {
        self.time_to_first_header
            .with_label_values(&[upstream])
            .observe(duration.as_secs_f64());
    }
    /// The attribution table: one count per upstream response, taken at
    /// the first upstream header (the status line is in). Mid-stream
    /// breaks after a healthy header still count here under the status
    /// class the header carried; the forward table's terminal outcome
    /// records the break as `proxy_error`, so the two tables do not sum
    /// equal (their difference is the mid-stream death count).
    pub(crate) fn record_upstream_response(
        &self,
        upstream: &str,
        api_family: &str,
        class: UpstreamStatusClass,
    ) {
        self.upstream_responses
            .with_label_values(&[upstream, api_family, class.as_str()])
            .inc();
    }

    pub(crate) fn observe_connect_duration(&self, upstream: &str, duration: Duration) {
        self.connect_duration
            .with_label_values(&[upstream])
            .observe(duration.as_secs_f64());
    }

    /// One failed connection attempt, not one failed request: the same
    /// request dials twice under a retry and counts twice here, while
    /// `record_forward` still counts the request once.
    pub(crate) fn record_connect_failure(&self, upstream: &str, class: ConnectErrorClass) {
        self.connect_failures
            .with_label_values(&[upstream, class.as_str()])
            .inc();
    }

    pub(crate) fn inc_in_flight(&self) {
        self.in_flight.inc();
    }

    pub(crate) fn dec_in_flight(&self) {
        self.in_flight.dec();
    }

    pub(crate) fn record_identity(&self, outcome: IdentityOutcome) {
        self.identity_resolutions
            .with_label_values(&[outcome.as_str()])
            .inc();
    }

    /// Test-only reader: the recorded count of one forward-outcome
    /// series.
    #[cfg(test)]
    pub(crate) fn forward_count(&self, endpoint: Endpoint, outcome: ForwardOutcome) -> u64 {
        self.forward_total
            .with_label_values(&[endpoint_label(endpoint), outcome.as_str()])
            .get()
    }

    /// Test-only reader: the recorded count of one identity-outcome
    /// series.
    #[cfg(test)]
    pub(crate) fn identity_count(&self, outcome: IdentityOutcome) -> u64 {
        self.identity_resolutions
            .with_label_values(&[outcome.as_str()])
            .get()
    }

    /// Test-only reader: the in-flight level.
    #[cfg(test)]
    pub(crate) fn in_flight_value(&self) -> i64 {
        self.in_flight.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The families the face owns -- names and label names -- locked
    /// from the collectors' descriptors: a rename, a relabel, a drop,
    /// or an accidental extra family fails here, not in a dashboard.
    /// (A label-vec collector only gathers once it has children, so
    /// the lock reads the descriptors, not the exposition.)
    #[test]
    fn families_lock() {
        use prometheus::core::Collector;
        let metrics = Metrics::private();
        let shape = |c: &dyn Collector| {
            let desc = c.desc();
            assert_eq!(desc.len(), 1, "one descriptor per collector");
            (desc[0].fq_name.clone(), desc[0].variable_labels.clone())
        };
        assert_eq!(
            shape(&metrics.forward_total),
            (
                "kallipai_model_gateway_forward_total".to_owned(),
                vec!["endpoint".to_owned(), "outcome".to_owned()],
            )
        );
        assert_eq!(
            shape(&metrics.forward_duration),
            (
                "kallipai_model_gateway_forward_duration_seconds".to_owned(),
                vec!["endpoint".to_owned()]
            )
        );
        assert_eq!(
            shape(&metrics.time_to_first_header),
            (
                "kallipai_model_gateway_upstream_time_to_first_header_seconds".to_owned(),
                vec!["upstream".to_owned()]
            )
        );
        assert_eq!(
            shape(&metrics.connect_duration),
            (
                "kallipai_model_gateway_upstream_connect_duration_seconds".to_owned(),
                vec!["upstream".to_owned()]
            )
        );
        assert_eq!(
            shape(&metrics.connect_failures),
            (
                "kallipai_model_gateway_upstream_connect_failures_total".to_owned(),
                vec!["upstream".to_owned(), "error_class".to_owned()],
            )
        );
        assert_eq!(
            shape(&metrics.upstream_responses),
            (
                "kallipai_model_gateway_upstream_responses_total".to_owned(),
                vec![
                    "upstream".to_owned(),
                    "api_family".to_owned(),
                    "status_class".to_owned(),
                ]
            )
        );
        assert_eq!(
            shape(&metrics.in_flight),
            (
                "kallipai_model_gateway_forward_in_flight".to_owned(),
                vec![]
            )
        );
        assert_eq!(
            shape(&metrics.identity_resolutions),
            (
                "kallipai_model_gateway_identity_resolutions_total".to_owned(),
                vec!["outcome".to_owned()]
            )
        );
    }

    #[test]
    fn outcome_vocabulary_lock() {
        let all = [
            ForwardOutcome::GatedAuth,
            ForwardOutcome::GatedRoute,
            ForwardOutcome::GatedVisibility,
            ForwardOutcome::GatedParking,
            ForwardOutcome::GatedPayload,
            ForwardOutcome::Upstream2xx,
            ForwardOutcome::Upstream4xx,
            ForwardOutcome::Upstream5xx,
            ForwardOutcome::UpstreamOther,
            ForwardOutcome::ConnectError,
            ForwardOutcome::ProxyError,
        ];
        let rendered: Vec<&str> = all.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            rendered,
            vec![
                "gated_auth",
                "gated_route",
                "gated_visibility",
                "gated_parking",
                "gated_payload",
                "upstream_2xx",
                "upstream_4xx",
                "upstream_5xx",
                "upstream_other",
                "connect_error",
                "proxy_error",
            ]
        );
    }

    #[test]
    fn status_class_vocabulary_lock() {
        let all = [
            UpstreamStatusClass::S2xx,
            UpstreamStatusClass::S4xx,
            UpstreamStatusClass::S5xx,
            UpstreamStatusClass::Other,
        ];
        let rendered: Vec<&str> = all.iter().map(|c| c.as_str()).collect();
        assert_eq!(rendered, vec!["2xx", "4xx", "5xx", "other"]);
        assert_eq!(
            UpstreamStatusClass::from_status(304),
            UpstreamStatusClass::Other
        );
    }

    #[test]
    fn upstream_status_classes() {
        assert_eq!(
            ForwardOutcome::from_upstream_status(200),
            ForwardOutcome::Upstream2xx
        );
        assert_eq!(
            ForwardOutcome::from_upstream_status(299),
            ForwardOutcome::Upstream2xx
        );
        assert_eq!(
            ForwardOutcome::from_upstream_status(404),
            ForwardOutcome::Upstream4xx
        );
        assert_eq!(
            ForwardOutcome::from_upstream_status(500),
            ForwardOutcome::Upstream5xx
        );
        // 3xx and unusual codes are not a redirect story for a proxy's
        // upstream: they land in the residual bucket.
        assert_eq!(
            ForwardOutcome::from_upstream_status(304),
            ForwardOutcome::UpstreamOther
        );
    }

    #[test]
    fn identity_status_derivation() {
        assert_eq!(
            IdentityOutcome::from_status(401),
            IdentityOutcome::Unauthorized
        );
        assert_eq!(
            IdentityOutcome::from_status(403),
            IdentityOutcome::Forbidden
        );
        assert_eq!(IdentityOutcome::from_status(500), IdentityOutcome::Error);
    }

    #[test]
    fn endpoint_labels() {
        assert_eq!(endpoint_label(Endpoint::ChatCompletions), "chat");
        assert_eq!(endpoint_label(Endpoint::Responses), "responses");
        assert_eq!(endpoint_label(Endpoint::Messages), "anthropic");
    }

    #[test]
    fn forward_counters_and_isolation() {
        let a = Metrics::private();
        let b = Metrics::private();
        a.record_forward(Endpoint::ChatCompletions, ForwardOutcome::Upstream2xx);
        a.record_forward(Endpoint::ChatCompletions, ForwardOutcome::Upstream2xx);
        a.record_forward(Endpoint::Messages, ForwardOutcome::GatedVisibility);
        b.record_forward(Endpoint::ChatCompletions, ForwardOutcome::GatedAuth);
        assert_eq!(
            a.forward_total
                .with_label_values(&["chat", "upstream_2xx"])
                .get(),
            2
        );
        assert_eq!(
            a.forward_total
                .with_label_values(&["anthropic", "gated_visibility"])
                .get(),
            1
        );
        // A private registry sees only its own recordings.
        assert_eq!(
            b.forward_total
                .with_label_values(&["chat", "upstream_2xx"])
                .get(),
            0
        );
        assert_eq!(
            b.forward_total
                .with_label_values(&["chat", "gated_auth"])
                .get(),
            1
        );
    }

    #[test]
    fn histograms_take_samples() {
        let metrics = Metrics::private();
        metrics.observe_forward_duration(Endpoint::ChatCompletions, Duration::from_millis(1200));
        metrics.observe_time_to_first_header("owner/provider", Duration::from_millis(250));
        metrics.observe_connect_duration("owner/provider", Duration::from_millis(12));
        assert_eq!(
            metrics
                .forward_duration
                .with_label_values(&["chat"])
                .get_sample_count(),
            1
        );
        assert_eq!(
            metrics
                .time_to_first_header
                .with_label_values(&["owner/provider"])
                .get_sample_count(),
            1
        );
        assert_eq!(
            metrics
                .connect_duration
                .with_label_values(&["owner/provider"])
                .get_sample_count(),
            1
        );
    }

    #[test]
    fn connect_failures_by_class() {
        let metrics = Metrics::private();
        metrics.record_connect_failure("owner/p1", ConnectErrorClass::Refused);
        metrics.record_connect_failure("owner/p1", ConnectErrorClass::Refused);
        metrics.record_connect_failure("owner/p1", ConnectErrorClass::Tls);
        assert_eq!(
            metrics
                .connect_failures
                .with_label_values(&["owner/p1", "refused"])
                .get(),
            2
        );
        assert_eq!(
            metrics
                .connect_failures
                .with_label_values(&["owner/p1", "tls"])
                .get(),
            1
        );
    }

    #[test]
    fn in_flight_gauge_moves() {
        let metrics = Metrics::private();
        assert_eq!(metrics.in_flight.get(), 0);
        metrics.inc_in_flight();
        metrics.inc_in_flight();
        metrics.dec_in_flight();
        assert_eq!(metrics.in_flight.get(), 1);
    }

    #[test]
    fn render_exposes_the_families() {
        let metrics = Metrics::private();
        metrics.record_forward(Endpoint::Responses, ForwardOutcome::GatedParking);
        let text = metrics.render();
        assert!(text.contains("# HELP kallipai_model_gateway_forward_total"));
        assert!(
            text.contains(
                "kallipai_model_gateway_forward_total{endpoint=\"responses\",outcome=\"gated_parking\"} 1"
            )
        );
        // The build stamp carries the crate version and the constant 1.
        assert!(text.contains(&format!(
            "kallipai_model_gateway_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
        // The process face is present without any recording call.
        assert!(text.contains("process_"));
    }
}
