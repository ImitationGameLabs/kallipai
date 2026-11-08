pub mod agentid;
pub mod approval;
pub mod authtoken;
pub mod collection;
pub mod context;
pub mod credential;
pub mod declaration;
pub mod facts;
pub mod idtype;
pub mod message;
pub mod policy;
pub mod polis;
pub mod protocol;
pub mod readback;
pub mod retry;
pub mod secret_file;
pub mod timefmt;
pub mod tokens;
pub mod toolresult;

#[cfg(feature = "axum")]
pub mod auth_header;
#[cfg(feature = "axum")]
pub mod cors;
#[cfg(feature = "logging")]
pub mod logging;
#[cfg(feature = "axum")]
pub mod sse;
#[cfg(feature = "propagation")]
pub mod telemetry;

/// The target every span this workspace publishes carries: the name the export
/// layer's filter selects on, so a span is recorded however `RUST_LOG` says.
///
/// Spans and log events are separated by target, not by level. The layer
/// records this target at `info` and no event at any level, so a configured
/// exporter keeps its spans and the trace carries no log text. A span opened
/// without this target is not recorded at all: the filter's default directive
/// is `off`, which is why a span built by `#[instrument]` must name the target.
///
/// It sits outside the `telemetry` feature because opening a span does not need
/// the exporter. One literal serves both sides: a second copy would keep
/// compiling after a rename and silently export nothing.
pub const SPAN_TARGET: &str = "kallipai.spans";

pub use agentid::AgentId;
