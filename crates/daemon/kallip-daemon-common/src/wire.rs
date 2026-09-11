//! UDS wire protocol for the kallip local daemon.
//!
//! One newline-delimited JSON exchange per short connection: the client
//! connects, writes one [`Request`] line, reads one [`Response`] line, closes.
//! No streaming, no multiplexing — the four management verbs do not need it,
//! and a plain JSON line stays debuggable with `nc -U`.
//!
//! The `v` field on both envelopes is the protocol version. The daemon
//! family (common, daemon, ctl, web) ships as one unit, so additive
//! evolution — new fields, new tokens — rides the atomic upgrade and does
//! NOT bump `v`; mixed old/new binaries within one install are outside the
//! contract and fail loudly (missing field). `v` bumps are reserved for
//! changes no consumer can tolerate (removing a field, respelling a
//! token); unknown versions are rejected by the reader.

use serde::{Deserialize, Serialize};

/// Protocol version of this crate's wire shape.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum bytes accepted for one request line. Legitimate requests are far
/// smaller; the cap turns a runaway client (or a non-protocol file pointed at
/// the socket) into a clean error instead of an unbounded read.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// A client request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    #[serde(flatten)]
    pub body: RequestBody,
}

/// The management verbs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    /// Launch a new instance. `workspace` is an absolute path; `env` carries
    /// the allowlisted KEY=VALUE pairs the daemon passes to the spawn helper
    /// (relay config, operator token override, log filter).
    Spawn {
        slug: String,
        workspace: String,
        env: Vec<String>,
        /// Dev-only: run this explicit tagma binary instead of the
        /// resolved one. Absent on production payloads.
        #[serde(default)]
        exe: Option<String>,
        /// Dedicated-user form: launch the instance as this pre-declared
        /// system user (the platform-hosting profile). The daemon resolves
        /// the name to a uid/gid/home and drops privilege before exec;
        /// authorization then requires the peer to be the target user
        /// itself or root. Absent, the
        /// instance runs as the requesting peer itself (the same-uid form,
        /// which is the whole single-user story).
        #[serde(default)]
        user: Option<String>,
    },
    /// Terminate an instance: SIGTERM, grace period, SIGKILL.
    Stop { slug: String },
    /// Relaunch a registered instance that is stopped or dead: the
    /// record's workspace and user env reload; the surviving
    /// credentials are picked up by the fresh process. `env` is a one-shot
    /// overlay for this launch only — validated like spawn's env and
    /// not written back to the record. `#[serde(default)]` keeps
    /// pre-field payloads (the instances backend) parsing.
    Start {
        slug: String,
        #[serde(default)]
        env: Vec<String>,
        /// Dev-only: run this explicit tagma binary instead of the
        /// resolved one. Absent on production payloads.
        #[serde(default)]
        exe: Option<String>,
    },
    /// Register an existing instance (running or stopped) under this
    /// daemon without touching any process: the adopt verb for
    /// instances the daemon did not spawn. `data_dir` is the
    /// instance's own data directory (runtime.json, credentials/);
    /// the daemon validates shape and overlap, observes the live
    /// state, and never launches or signals anything. `env` is the
    /// persistent snapshot replayed by later starts, exactly like
    /// spawn's env. New fields must stay `#[serde(default)]` (the
    /// additive-evolution rule behind Start's env and exe).
    Adopt {
        slug: String,
        workspace: String,
        data_dir: String,
        #[serde(default)]
        env: Vec<String>,
        #[serde(default)]
        user: Option<String>,
        /// The caller's assertion that the instance is meant to run
        /// local-only; the escape hatch for the adopt-time relay probe.
        #[serde(default)]
        accept_local_only: bool,
    },
    /// Deregister an instance: the record goes away, the process
    /// does not — a running instance keeps running unmanaged, so
    /// stop it first when the goal is a stopped instance.
    /// Idempotent: a slug with no record removes as a no-op.
    Remove { slug: String },
    /// Tail an instance's log files — a read-only diagnostic: no
    /// state change, works on stopped instances too. `lines` is the
    /// request intent (the daemon clamps and may return fewer to
    /// stay inside its payload budget); `file` picks one file from
    /// the log directory instead of the merged tail; `cursor`
    /// continues a previous pull (the `--follow` contract). New
    /// fields must stay `#[serde(default)]` (the additive-evolution
    /// rule).
    Log {
        slug: String,
        #[serde(default)]
        lines: Option<u32>,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        cursor: Option<LogCursor>,
    },
    /// List all managed instances (directory scan).
    List,
    /// One instance's health, or omit `slug` for the daemon itself.
    Health { slug: Option<String> },
}

/// A daemon response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseBody {
    Ok {
        #[serde(flatten)]
        payload: OkPayload,
    },
    Err {
        code: ErrorCode,
        message: String,
    },
}

/// Successful payloads, keyed by verb.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OkPayload {
    Spawn {
        slug: String,
        pid: u32,
        port: u16,
    },
    /// The adopt outcome: the registration fact plus the observed
    /// state at adoption time. Deliberately no pid/port — those are
    /// spawn/start products; list and health carry them for an
    /// adopted instance like any other.
    Adopt {
        slug: String,
        state: InstanceState,
    },
    /// The log tail as one text payload plus the cursor a follow
    /// client resumes from. `next_cursor` is `None` when the log
    /// directory is missing or empty — nothing to follow yet.
    Log {
        text: String,
        #[serde(default)]
        next_cursor: Option<LogCursor>,
    },
    Stop {
        slug: String,
    },
    Remove {
        slug: String,
    },
    List {
        instances: Vec<InstanceInfo>,
    },
    Health {
        report: HealthReport,
    },
}

/// A position in an instance's log stream: the file name within the
/// instance's log directory plus the byte offset reached in it.
/// Clients echo it back to pull only new lines; the daemon clamps
/// or resets it when files rotated underneath (the follow contract
/// of the log verb).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogCursor {
    pub file: String,
    pub byte: u64,
}

/// The daemon's liveness word. Clients match on this, never
/// on the prose in `detail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    Running,
    Stopped,
    Dead,
    /// A state token this build does not know (an older or newer peer
    /// on the same wire). `serde(other)` lands here instead of failing
    /// the whole message decode: one mixed-version field degrades one
    /// instance, not the entire panel.
    #[serde(other)]
    Unknown,
}

impl InstanceState {
    /// The lowercase wire token, ready for CLI rendering.
    pub fn as_str(&self) -> &'static str {
        match self {
            InstanceState::Running => "running",
            InstanceState::Stopped => "stopped",
            InstanceState::Dead => "dead",
            InstanceState::Unknown => "unknown",
        }
    }
}

/// One managed instance as seen by a directory scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub slug: String,
    pub instance_id: String,
    pub workspace: String,
    pub running: bool,
    /// True while the instance is live (`state` Running), kept for
    /// one-field boolean checks (the web render keys on it). Mirrors
    /// the liveness half of `state`, not the state itself.
    pub state: InstanceState,
    /// Owning uid recorded at spawn (SO_PEERCRED of the requesting peer).
    pub owner: Option<u32>,
    /// The enrolled tagma identity, read by the scan from the instance's own
    /// `credentials/<entry>/tagma.id`. None when the instance never enrolled
    /// (local-only) or the read failed — the panel join then degrades to the
    /// slug convention. `#[serde(default)]` keeps pre-field wire JSON parsing.
    /// Discipline: `tagma.id` is the ONLY credentials file ever read;
    /// `tagma.token` (0o600 secret) is never opened.
    #[serde(default)]
    pub tagma_id: Option<String>,
    /// The instance's live listen port; None unless the instance is
    /// currently live, Running (a surviving runtime.json from a stopped
    /// or dead instance never surfaces its stale port). Surfaced so
    /// clients no longer rely on session-only spawn memory.
    #[serde(default)]
    pub port: Option<u16>,
}

/// Liveness detail for one instance (or the daemon).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthReport {
    pub slug: Option<String>,
    pub running: bool,
    /// True while the instance is live (`state` Running), kept for
    /// one-field boolean checks (the web render keys on it).
    /// Mirrors the liveness half of `state`, not the state itself.
    pub state: InstanceState,
    /// Human-readable supplement when `running` is false; consumers must
    /// not parse this — match `state` instead.
    pub detail: Option<String>,
}

/// Stable error codes. Clients match on these, not on messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The slug already exists in the record area.
    SlugTaken,
    /// The requester may not launch an instance as the target user.
    /// Fired before any state change, so it leaks nothing about slug
    /// existence.
    Denied,
    /// The workspace overlaps an existing instance's workspace or the
    /// instance tree itself.
    WorkspaceOverlap,
    /// A spawn configuration input is invalid (bad slug grammar, missing
    /// required env pair, non-directory workspace).
    InvalidSpawnInput,
    /// The spawned process did not publish pid/port within the timeout.
    SpawnTimeout,
    /// No managed instance carries this slug.
    NotFound,
    /// The instance is not running (stop/health on a dead slug).
    NotRunning,
    /// The request could not be parsed or violates the protocol version.
    BadRequest,
    /// An internal error (I/O, signal failure); the message carries context.
    Internal,
    /// A code token this build does not know (an older or newer peer
    /// on the same wire). `serde(other)` lands here instead of failing
    /// the whole message decode.
    #[serde(other)]
    Unknown,
}

/// An instance slug must be a DNS-label-like lowercase slug
/// (`[a-z0-9][a-z0-9-]*`): it is a directory name in the instance tree and a
/// log key, exactly like the relay-entry names inside `relays.toml`
/// (kallip-tagma `valid_entry_name`). Duplicated grammar on purpose — the
/// daemon stays independent of kallip-tagma; unifying the two into one shared
/// helper is a tracked follow-up, not something this crate reaches for now.
pub fn valid_slug(slug: &str) -> bool {
    // The 64-char cap is part of the grammar: a slug is a directory
    // name and a path component end to end, so length is validated
    // with shape, not left to whichever client remembers to pre-check.
    slug.len() <= 64 && {
        let mut chars = slug.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }
}

/// Serialize a [`Request`] to its wire line (no trailing newline).
pub fn encode_request(request: &Request) -> Result<String, serde_json::Error> {
    serde_json::to_string(request)
}

/// Parse one request line. Errors carry the parse failure verbatim; callers
/// translate that into a `bad_request` response.
pub fn decode_request(line: &str) -> Result<Request, serde_json::Error> {
    let request: Request = serde_json::from_str(line)?;
    Ok(request)
}

/// Serialize a [`Response`] to its wire line (no trailing newline).
pub fn encode_response(response: &Response) -> Result<String, serde_json::Error> {
    serde_json::to_string(response)
}

/// Parse one response line.
pub fn decode_response(line: &str) -> Result<Response, serde_json::Error> {
    let response: Response = serde_json::from_str(line)?;
    Ok(response)
}

/// Construct a v1 request envelope.
pub fn request(body: RequestBody) -> Request {
    Request {
        v: PROTOCOL_VERSION,
        body,
    }
}

/// Construct a v1 ok response envelope.
pub fn ok(payload: OkPayload) -> Response {
    Response {
        v: PROTOCOL_VERSION,
        body: ResponseBody::Ok { payload },
    }
}

/// Construct a v1 err response envelope.
pub fn err(code: ErrorCode, message: impl Into<String>) -> Response {
    Response {
        v: PROTOCOL_VERSION,
        body: ResponseBody::Err {
            code,
            message: message.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_request_round_trips_with_all_fields() {
        let body = RequestBody::Log {
            slug: "team-a".into(),
            lines: Some(50),
            file: Some("instance.2026-09-10.log".into()),
            cursor: Some(LogCursor {
                file: "instance.2026-09-09.log".into(),
                byte: 2048,
            }),
        };
        let line = encode_request(&request(body.clone())).expect("encode");
        let back = decode_request(&line).expect("decode");
        assert_eq!(back.body, body);
    }

    #[test]
    fn log_request_defaults_every_optional_field() {
        // Same additive rule as Start's env: a slug-only payload
        // decodes — defaults mean the merged tail, daemon-chosen
        // line count, no follow cursor.
        let line = r#"{"v":1,"type":"log","slug":"team-a"}"#;
        let back = decode_request(line).expect("decode");
        match back.body {
            RequestBody::Log {
                slug,
                lines,
                file,
                cursor,
            } => {
                assert_eq!(slug, "team-a");
                assert_eq!(lines, None);
                assert_eq!(file, None);
                assert_eq!(cursor, None);
            }
            other => panic!("expected log, got {other:?}"),
        }
    }

    #[test]
    fn log_payload_round_trips_with_and_without_cursor() {
        let with = OkPayload::Log {
            text: "first\nsecond\n".into(),
            next_cursor: Some(LogCursor {
                file: "instance.2026-09-10.log".into(),
                byte: 14,
            }),
        };
        let line = encode_response(&ok(with.clone())).expect("encode");
        let back = decode_response(&line).expect("decode");
        assert_eq!(back.body, ResponseBody::Ok { payload: with });
        let without = OkPayload::Log {
            text: String::new(),
            next_cursor: None,
        };
        let line = encode_response(&ok(without.clone())).expect("encode");
        let back = decode_response(&line).expect("decode");
        assert_eq!(back.body, ResponseBody::Ok { payload: without });
    }

    #[test]
    fn request_round_trips_through_json() {
        let body = RequestBody::Spawn {
            slug: "team-a".into(),
            workspace: "/home/u/work/a".into(),
            env: vec!["KALLIP_TAGMA_ADDR=127.0.0.1:0".into()],
            exe: None,
            user: None,
        };
        let line = encode_request(&request(body.clone())).expect("encode");
        assert!(!line.contains('\n'), "one line, caller appends the newline");
        let back = decode_request(&line).expect("decode");
        assert_eq!(back.v, PROTOCOL_VERSION);
        assert_eq!(back.body, body);
    }

    #[test]
    fn start_request_round_trips_with_env() {
        let body = RequestBody::Start {
            slug: "team-a".into(),
            env: vec!["KALLIP_TAGMA_LOG_TO_STDERR=1".into()],
            exe: None,
        };
        let line = encode_request(&request(body.clone())).expect("encode");
        let back = decode_request(&line).expect("decode");
        assert_eq!(back.body, body);
    }

    #[test]
    fn start_request_without_env_defaults_empty() {
        // The instances backend predates the field; its field-less
        // payload must keep parsing (additive evolution, no `v` bump).
        let line = r#"{"v":1,"type":"start","slug":"team-a"}"#;
        let back = decode_request(line).expect("decode");
        match back.body {
            RequestBody::Start {
                slug,
                env,
                exe: None,
            } => {
                assert_eq!(slug, "team-a");
                assert!(env.is_empty());
            }
            other => panic!("expected start, got {other:?}"),
        }
    }

    #[test]
    fn spawn_request_without_exe_parses() {
        // Same additive rule as Start's env: a dev-loop-free payload
        // (no exe field) decodes with exe = None.
        let line = r#"{"v":1,"type":"spawn","slug":"team-a","workspace":"/w","env":[]}"#;
        let back = decode_request(line).expect("decode");
        match back.body {
            RequestBody::Spawn { slug, exe, .. } => {
                assert_eq!(slug, "team-a");
                assert!(exe.is_none());
            }
            other => panic!("expected spawn, got {other:?}"),
        }
    }
    #[test]
    fn spawn_request_without_user_parses() {
        // Same additive rule as exe/env: a same-uid-form payload (no
        // user field) decodes with user = None — old clients keep
        // meaning the single-user path, pinned here.
        let line = r#"{"v":1,"type":"spawn","slug":"team-a","workspace":"/w","env":[]}"#;
        let back = decode_request(line).expect("decode");
        match back.body {
            RequestBody::Spawn { user, .. } => {
                assert!(user.is_none());
            }
            other => panic!("expected spawn, got {other:?}"),
        }
    }
    #[test]
    fn response_err_round_trips_with_stable_code() {
        let line = encode_response(&err(ErrorCode::SlugTaken, "slug exists")).expect("encode");
        let back = decode_response(&line).expect("decode");
        match back.body {
            ResponseBody::Err { code, message } => {
                assert_eq!(code, ErrorCode::SlugTaken);
                assert_eq!(message, "slug exists");
            }
            other => panic!("expected err, got {other:?}"),
        }
    }

    #[test]
    fn instance_state_tolerates_unknown_tokens() {
        // An older daemon still emits a token this build never wrote; one
        // unknown token must degrade to one Unknown instance, not fail the
        // whole list decode.
        let parsed: InstanceState = serde_json::from_str("\"adopted\"").expect("legacy token");
        assert_eq!(parsed, InstanceState::Unknown);
        let parsed: InstanceState = serde_json::from_str("\"running\"").expect("known token");
        assert_eq!(parsed, InstanceState::Running);
    }

    #[test]
    fn error_code_tolerates_unknown_tokens() {
        let parsed: ErrorCode = serde_json::from_str("\"retired_code\"").expect("unknown token");
        assert_eq!(parsed, ErrorCode::Unknown);
        let parsed: ErrorCode = serde_json::from_str("\"slug_taken\"").expect("known token");
        assert_eq!(parsed, ErrorCode::SlugTaken);
    }

    #[test]
    fn spawn_payload_round_trips() {
        let payload = OkPayload::Spawn {
            slug: "team-a".into(),
            pid: 4242,
            port: 39999,
        };
        let line = encode_response(&ok(payload.clone())).expect("encode");
        let back = decode_response(&line).expect("decode");
        assert_eq!(back.body, ResponseBody::Ok { payload });
    }

    #[test]
    fn adopt_request_round_trips_with_all_fields() {
        let body = RequestBody::Adopt {
            slug: "team-a".into(),
            workspace: "/home/u/work/a".into(),
            data_dir: "/home/u/.local/share/kallipai/tagmata/team-a".into(),
            env: vec!["KALLIP_TAGMA_ADDR=127.0.0.1:4711".into()],
            user: Some("alice".into()),
            accept_local_only: true,
        };
        let line = encode_request(&request(body.clone())).expect("encode");
        let back = decode_request(&line).expect("decode");
        assert_eq!(back.body, body);
    }

    #[test]
    fn adopt_request_without_optional_fields_parses() {
        // Same additive rule as Start's env and Spawn's exe: a
        // minimal adopt payload decodes with the defaults — empty
        // env, same-uid form, probe active.
        let line = r#"{"v":1,"type":"adopt","slug":"team-a","workspace":"/w","data_dir":"/d"}"#;
        let back = decode_request(line).expect("decode");
        match back.body {
            RequestBody::Adopt {
                slug,
                env,
                user,
                accept_local_only,
                ..
            } => {
                assert_eq!(slug, "team-a");
                assert!(env.is_empty());
                assert!(user.is_none());
                assert!(!accept_local_only);
            }
            other => panic!("expected adopt, got {other:?}"),
        }
    }

    #[test]
    fn adopt_payload_round_trips() {
        let payload = OkPayload::Adopt {
            slug: "team-a".into(),
            state: InstanceState::Stopped,
        };
        let line = encode_response(&ok(payload.clone())).expect("encode");
        let back = decode_response(&line).expect("decode");
        assert_eq!(back.body, ResponseBody::Ok { payload });
    }

    #[test]
    fn unknown_protocol_version_parses_structurally_but_differs() {
        // A future-version line still parses structurally, but carries a
        // different `v`; the reader must compare against PROTOCOL_VERSION.
        let line = r#"{"v":99,"type":"list"}"#;
        let parsed = decode_request(line).expect("structural parse");
        assert_ne!(parsed.v, PROTOCOL_VERSION);
    }

    #[test]
    fn slug_grammar_matches_the_relay_entry_rule() {
        for good in ["a", "team-a", "t2", "9lives", "a-b-c", "a--b", "trail-"] {
            assert!(valid_slug(good), "{good} should be valid");
            // The cap boundary itself: 64 chars pass, 65 fail.
            assert!(valid_slug(&"a".repeat(64)));
            assert!(!valid_slug(&"a".repeat(65)));
        }
        for bad in [
            "",
            "-lead",
            "Upper",
            "under_score",
            "sp ace",
            "dot.dot",
            "ümlaut",
            "a-very-long-slug-that-keeps-going-well-past-the-sixty-four-char-cap",
        ] {
            assert!(!valid_slug(bad), "{bad} should be invalid");
        }
    }
}
