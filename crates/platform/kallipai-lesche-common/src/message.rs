//! Envelope + E2E payload model.
//!
//! The archeion sees only the [`Envelope`] (routing metadata + opaque ciphertext).
//! The [`TagmaRequest`] / [`TagmaReply`] inside is the E2E payload shared
//! between app and tagma; the archeion never decrypts it.

use crate::direct::FileAttachment;
use crate::event::AuthoredEvent;
use crate::rooms::MemberId;
use kallipai_archeion_common::bytes::Ciphertext;
use kallipai_archeion_common::ids::{ChannelId, TraceId};
use serde::{Deserialize, Serialize};
use serde_json;
use time::OffsetDateTime;

/// Re-exported from `kallipai_archeion_common::participant` so the envelope sender
/// type stays reachable from its long-standing `kallipai_lesche_common::message`
/// path. See that module for the trust model.
pub use kallipai_archeion_common::participant::Participant;

/// The unit the archeion forwards between endpoints. Carries routing metadata +
/// AEAD ciphertext; the archeion reads only the metadata.
///
/// `channel_id` is the routing target: which channel this envelope is
/// addressed to. One field, two value domains -- a multi-member room
/// ([`crate::rooms::RoomId`], a v4 UUID) or the bilateral 1:1 conversation
/// (`ConversationId`, a v5 UUID derived per tagma). The UUID version
/// nibble keeps the two spaces disjoint, so a room envelope can never be
/// mistaken for a 1:1 conversation and vice versa. A direct-session id
/// (a v5 hash normalized into the v4 form) shares the room's value domain;
/// the relay dispatches on its joined-rooms / direct-session cache
/// membership rather than a tag field. Serialized as `channel_id`;
/// `conversation_id` remains readable as a serde alias for
/// one rolling-deploy window (removal tracked in the debt ledger).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(rename = "channel_id", alias = "conversation_id")]
    pub channel_id: ChannelId,
    pub sender: Participant,
    /// Per-conversation, per-sender monotonic counter from 0. Doubles as the
    /// AEAD nonce counter (direction-tagged) and as the archeion's idempotency key.
    pub sequence_n: u64,
    pub trace_id: TraceId,
    #[serde(with = "time::serde::iso8601")]
    pub timestamp: OffsetDateTime,
    pub ciphertext: Ciphertext,
}

/// One decoded history entry: the sender paired with the content-only wire
/// reply. The history-pull response shape (online replay and offline
/// `/external/history`), so the frontend has one render path for replayed rows
/// matching the live `{sender, body}` shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub sender: Participant,
    pub reply: TagmaReply,
}

/// The room plaintext: the content a room participant broadcasts to the room's
/// other members. This is the room-path payload; it is NOT the bilateral
/// [`TagmaRequest`] (rooms and 1:1 conversations are disjoint address spaces).
/// A room message is its text plus the optional references below. The bilateral
/// `req_id`/ack machinery does not apply: lesche's synchronous 202 is the room
/// send ack, and the rooms route assigns the per-room seq.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomMessage {
    pub text: String,
    /// A file attached to the message, when the sender shared one. Optional
    /// with `serde(default)` + `skip_serializing_if` so messages without an
    /// attachment keep the historical wire shape byte-for-byte and older
    /// readers (which ignore unknown fields) are none the wiser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<FileAttachment>,
    /// The position, in THIS room's log, of the message this one quotes: the
    /// per-room `seq` the lesche assigned it. Same room only -- a seq names a
    /// row here and nowhere else. A positive seq renders as a quote; `<= 0`
    /// reads as no quote at all (there is no row 0). Only the position
    /// crosses the wire: the quoted text is never copied, so a reader who
    /// cannot see the target renders absence, not stale copy. Optional with
    /// `serde(default)` + `skip_serializing_if` (the `attachment` pattern):
    /// un-quoted sends keep the historical wire shape byte-for-byte.
    /// An absence verdict speaks for the present only: a position starts
    /// resolving once a row takes it, so a quote naming a seq the room has
    /// not reached yet begins pointing at that row later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<i64>,
    /// The room members this message names: their participant ids, in the order the
    /// sender picked them. Only the id crosses the wire -- the display handle is
    /// derived at read time from the registry, so a renamed account or a departed
    /// member still renders as the same participant instead of a frozen name.
    /// Optional with `serde(default)` + `skip_serializing_if` (the `attachment`
    /// pattern): an unmentioned send keeps the historical wire shape byte-for-byte.
    ///
    /// A mention is a fact, not a route: every member receives the message either
    /// way, and unread stays a row-based count. Decoding is lenient by design (see
    /// `mentions_from_the_wire`): a malformed entry costs that entry and never the
    /// frame, and a list past [`MENTIONS_MAX`] is truncated rather than refused --
    /// the reader is the side that must survive a payload it did not write. A writer
    /// may be strict; the cap itself is one number.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "mentions_from_the_wire"
    )]
    pub mentions: Vec<MemberId>,
}

/// Upper bound on a room message's mention list. A writer that carries more is a
/// caller error worth naming; the reader truncates at the same number and still
/// decodes. One constant so the two sides cannot drift apart.
pub const MENTIONS_MAX: usize = 16;

/// The canonical spelling of a participant id: a lowercase hyphenated UUID.
///
/// Participant ids are UUID strings the platform derives or mints (v5 for a user
/// or tagma, v4 for an external agent), so that is the shape every writer emits.
/// Accepting only it keeps this side and the TS codec on the same set, and every
/// accepted spelling settles on one value -- a member named in two cases is one
/// member, comparable byte for byte wherever the id is compared later.
fn canonical_participant_id(raw: &str) -> Option<MemberId> {
    let parsed = uuid::Uuid::parse_str(raw).ok()?;
    let canonical = parsed.hyphenated().to_string().to_ascii_lowercase();
    if canonical.eq_ignore_ascii_case(raw) {
        Some(MemberId::from(canonical))
    } else {
        None
    }
}

/// Deduplicate (first occurrence wins, sender order kept) and cap a mention list
/// at [`MENTIONS_MAX`], dropping entries that are not participant ids.
///
/// The one normalization both ends share: a reader applies it to what arrived, a
/// writer to what it is about to send, so "the same members" means the same
/// list on both sides.
pub fn normalize_mentions<I>(ids: I) -> Vec<MemberId>
where
    I: IntoIterator<Item = MemberId>,
{
    let mut out: Vec<MemberId> = Vec::new();
    for id in ids {
        if out.len() == MENTIONS_MAX {
            break;
        }
        let Some(canonical) = canonical_participant_id(id.as_ref()) else {
            continue;
        };
        if out.contains(&canonical) {
            continue;
        }
        out.push(canonical);
    }
    out
}

/// Wording of the two write-side refusals, one hole each: the text a caller
/// named, and the cap.
///
/// The TypeScript codec reads these two constants out of this file (its source
/// pin, the same way it reads [`MENTIONS_MAX`]), so "the same message on both
/// ends" is one fact instead of two literals that can drift apart. Renaming a
/// constant or changing the hole shape makes that pin fail loudly. They stay
/// private: the templates are this crate's wording, and the pin reads source
/// text rather than linking against a name this crate must keep public.
const MENTION_MALFORMED: &str = "not a participant id: {}";
/// See [`MENTION_MALFORMED`]; its hole is [`MENTIONS_MAX`].
const MENTION_TOO_MANY: &str = "a send names more than {} members";

/// Why a writer's mention list was refused.
///
/// Only a writer produces one: a reader drops what it cannot place (see
/// [`normalize_mentions`]) and never refuses the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionError {
    /// An entry is not a participant id in its canonical spelling (see
    /// `canonical_participant_id`); carries the text as the caller wrote it.
    Malformed(String),
    /// More distinct members than the cap allows, counted after repeats fold.
    /// `named` is the member count the fold stopped at, so it reads `max + 1`
    /// however far past the cap a caller went; the text reports `max`.
    TooMany { named: usize, max: usize },
}

impl std::fmt::Display for MentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(raw) => f.write_str(&MENTION_MALFORMED.replacen("{}", raw, 1)),
            Self::TooMany { max, .. } => {
                f.write_str(&MENTION_TOO_MANY.replacen("{}", &max.to_string(), 1))
            }
        }
    }
}

impl std::error::Error for MentionError {}

/// The write-side counterpart of [`normalize_mentions`]: the same accepted
/// spellings and the same folding (first occurrence wins, sender order kept,
/// the cap counts members rather than entries), except that a member the caller
/// named and this side cannot place is an error rather than a silent drop.
///
/// The judging lives in two shared places: the tagma route and the codec's
/// `checkedMentions`. The CLI hands the ids over unchanged, so a refusal
/// there names the same reason. A send that quietly lost a picked member
/// would look like a mention naming nobody, which is the failure this
/// refuses.
pub fn checked_mentions<I, S>(raw: I) -> Result<Vec<MemberId>, MentionError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<MemberId> = Vec::new();
    for entry in raw {
        let entry = entry.as_ref();
        let Some(canonical) = canonical_participant_id(entry) else {
            return Err(MentionError::Malformed(entry.to_owned()));
        };
        if out.contains(&canonical) {
            continue;
        }
        out.push(canonical);
        // The cap is judged inside the fold, the way the read side does it:
        // a list past it costs one pass and stops, instead of folding every
        // entry first (each one a linear `contains`) to learn the same thing
        // at the end. Repeats and re-spellings never reach this branch.
        if out.len() > MENTIONS_MAX {
            return Err(MentionError::TooMany {
                named: out.len(),
                max: MENTIONS_MAX,
            });
        }
    }
    Ok(out)
}

/// Decode [`RoomMessage::mentions`] leniently: anything that is not a list of
/// participant ids reads as the entries that are, and a malformed value reads as
/// no mentions at all. The message itself always decodes -- the same rule the TS
/// codec applies to a malformed quote, and the reason a crafted payload cannot
/// cost a room its text.
///
/// It reads through `serde_json::Value`, so this field needs a self-describing
/// format: `RoomMessage` decodes from JSON alone. `attachment` and `quote` carry
/// no such constraint, which is why the wire is stated as JSON rather than as
/// any serde format.
fn mentions_from_the_wire<'de, D>(deserializer: D) -> Result<Vec<MemberId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(deserializer)?;
    let Some(items) = raw.as_array() else {
        return Ok(Vec::new());
    };
    Ok(normalize_mentions(
        items
            .iter()
            .filter_map(|item| item.as_str())
            .map(|id| MemberId::from(id.to_owned())),
    ))
}

/// App -> tagma: one semantic operation against the tagma, encrypted inside an
/// envelope. The tagma owns the agent(s) that realize the op; the app never
/// names an agent. `req_id` correlates the op with its [`TagmaReply`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TagmaRequest {
    /// Send a user message to the tagma's root agent.
    SendMessage {
        req_id: u64,
        text: String,
        /// A file attached to the message, when the sender shared one. Optional
        /// with `serde(default)` + `skip_serializing_if` (the `RoomMessage`
        /// pattern): sends without an attachment keep the historical wire shape
        /// byte-for-byte, and older readers ignore the unknown field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachment: Option<FileAttachment>,
        /// The id of the message this one quotes (`chat_history.id` in this
        /// bilateral partition), when the sender replied to an earlier line.
        /// Only the id crosses the wire: the quoted text is never copied, so
        /// a target reclaimed by the history GC renders as unavailable at
        /// both ends. Optional with `serde(default)` + `skip_serializing_if`
        /// (the `attachment` pattern), so un-quoted sends keep the old shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quote: Option<i64>,
    },
    /// Interrupt the tagma's in-flight turn.
    Interrupt { req_id: u64 },
}

/// App -> tagma: a control operation that does NOT drive the agent (today: the
/// cursor-based history pull). Carried in the same encrypted envelope channel as
/// [`TagmaRequest`]; the relay dispatches by the `op` discriminant (which is
/// disjoint from `TagmaRequest`'s). Kept separate so `TagmaRequest` stays
/// "actions against the agent" and is not polluted by sync plumbing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TagmaControl {
    /// Pull a batch of chat history. The `(after, before)` pair selects the
    /// window:
    /// - `after=None,  before=None`  -> most recent `limit` rows (a first-time
    ///   device with an empty local cache).
    /// - `after=Some,  before=None`  -> rows with `id > after` (incremental
    ///   catch-up on reconnect; the app sends its rendered high-water mark).
    /// - `after=None,  before=Some`  -> rows with `id < before` (scroll-up lazy
    ///   load of older history).
    ///
    /// The relay responds with the matching rows (re-encrypted) followed by a
    /// [`TagmaReply::HistoryBatchEnd`] carrying the same `req_id`. `limit` is
    /// clamped server-side to `HISTORY_BATCH_MAX`.
    History {
        req_id: u64,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    },
    /// A management operation (budget, agents, profiles, schedules). The relay
    /// dispatches it in-process against the tagma route handlers with
    /// `Identity::Operator`, the same trust model as `SendMessage`: the
    /// bilateral E2E conversation is the authorization boundary, so any
    /// principal that can open that conversation is already trusted as the
    /// operator. `method` is the HTTP verb (GET/POST/PUT/DELETE), `path` is the
    /// tagma route path (/budget, /agents/{id}/interrupt, etc.), and `body` is
    /// the JSON request body (null for GET/DELETE). The relay replies with a
    /// `TagmaReply::ManageResult` carrying the same `req_id`.
    Manage {
        req_id: u64,
        method: String,
        path: String,
        body: serde_json::Value,
    },
}

/// Tagma -> app: either the result of a correlated op, or an unsolicited
/// event from the tagma's event pump. The archeion never decrypts this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TagmaReply {
    /// `SendMessage` was accepted by the tagma. `history_id` is the
    /// `chat_history.id` of the **inbound** row the tagma appended for this
    /// user message (before running the op), so the app can stamp its
    /// optimistic local user line with a stable id and dedup it against a later
    /// history replay. `0` means no row was recorded and must not be used for
    /// dedup. The ack itself is live-only: it is never stored or replayed.
    MessageAccepted {
        req_id: u64,
        queue_depth: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
        #[serde(default)]
        history_id: i64,
        /// When this inbound row was appended (RFC 3339). Absent on acks with no
        /// durable row (`history_id == 0`) and on payloads serialized before the
        /// field existed. The authoritative send time for the user's line.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
        /// The request's attachment, echoed so the app can stamp its optimistic
        /// user line with the authoritative reference. Absent on un-attached
        /// sends and on acks serialized before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachment: Option<FileAttachment>,
        /// The request's quoted-message id, echoed for the same reason as the
        /// attachment above. Absent on un-quoted sends and on older acks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quote: Option<i64>,
    },
    /// `Interrupt` was delivered.
    Interrupted { req_id: u64 },
    /// An op failed. `status` mirrors the tagma/archeion HTTP status where one
    /// applies (502 for an internal tagma panic).
    Error {
        req_id: u64,
        status: u16,
        message: String,
        /// Machine-readable rejection code, when the source ApiError carries
        /// one (absent on frames that do not carry it).
        #[serde(default)]
        code: Option<String>,
    },
    /// An unsolicited authored tagma event (an assistant message). Has no
    /// `req_id`: it is produced by the tagma's event pump, not in reply to any
    /// single op. `history_id` is the `chat_history.id` of the outbound row the
    /// tagma appended for this event before emitting it; the app uses it as a
    /// stable id to order/dedup frames across batch replay and live delivery.
    /// `0` means the row was not recorded (e.g. a relay running without chat
    /// history) and must not be used for dedup. Runtime signals (busy/idle,
    /// terminals, errors) do NOT ride this variant — they cross as plaintext
    /// `LescheEvent::TagmaSystem`, since they are operator metadata, not
    /// conversation content.
    Event {
        event: AuthoredEvent,
        #[serde(default)]
        history_id: i64,
        /// When the outbound row was appended (RFC 3339). Absent on frames with
        /// no durable row and on payloads serialized before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    /// Replay-only echo of a user-authored message (an inbound `chat_history`
    /// row), produced by `handle_history` so the app can reconstruct the user
    /// side of a past conversation. Never emitted on the live path: live user
    /// messages come from the app's own optimistic render + `MessageAccepted`.
    /// `history_id` is the inbound row id; the app dedups/orders by it.
    UserMessage {
        history_id: i64,
        text: String,
        /// When the inbound row was originally appended (RFC 3339). Absent on
        /// payloads serialized before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
        /// The row's attachment, when the inbound message carried one. Absent
        /// on rows persisted before the field existed (replayed as no file).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachment: Option<FileAttachment>,
        /// The row's quoted-message id, when the inbound message quoted one.
        /// Absent on rows persisted before the field existed (replayed as
        /// un-quoted), mirroring the attachment contract.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quote: Option<i64>,
    },
    /// The sole completion signal for a `TagmaControl::History` batch. The
    /// relay emits the batch's rows (each as `Event` or `UserMessage`) and then
    /// this marker carrying the same `req_id`. `count` is the rows sent;
    /// `more` being true means the window was truncated by `limit` and more
    /// rows remain pullable. If this marker never arrives, the app treats the
    /// sync as failed and retries on the next reconnect.
    HistoryBatchEnd {
        req_id: u64,
        count: u32,
        more: bool,
        /// Unread rows in this peer's partition: the agent's own replies
        /// (`direction = outbound`) above the stored read cursor. The tagma
        /// computes it, so the app never derives a badge from the window it
        /// happened to pull; a batch is stamped on every pull (cold start,
        /// reconnect, scroll page). Absent on payloads serialized before the
        /// field existed (`serde(default)`), where it reads as zero.
        #[serde(default)]
        unread: u32,
        /// The read cursor the count was computed against. Paired with
        /// `unread` it orders two snapshots -- a stale one carries a smaller
        /// cursor -- which the app needs because the carriers (this marker,
        /// the live `Unread` event, the keryx's cold-start flush) travel
        /// different paths. Zero on payloads from a peer predating it.
        #[serde(default)]
        cursor: i64,
        /// Rows the window held that the decoder could not read: the tagma
        /// skips them, so a non-zero count means the batch is a window with
        /// a hole in it -- enough to say "not empty", never enough to prove
        /// absence. Zero on payloads from a peer predating it.
        #[serde(default)]
        undecodable: u32,
    },
    /// The result of a `TagmaControl::Manage` op. `status` mirrors the HTTP
    /// status the tagma handler returned (200, 201, 204, 400, 404, 409, etc.);
    /// `body` is the handler JSON response body (null for empty/204 responses).
    /// Live-only: never stored or replayed.
    ManageResult {
        req_id: u64,
        status: u16,
        body: serde_json::Value,
    },
}

impl TagmaReply {
    /// Stamp a `chat_history` row id onto this reply. `Event` carries the id of
    /// its outbound row; `MessageAccepted` carries the id of the inbound row
    /// the send appended. The other variants are live-only (never stored or
    /// replayed) and are left untouched.
    pub fn set_history_id(&mut self, id: i64) {
        match self {
            TagmaReply::Event { history_id, .. }
            | TagmaReply::MessageAccepted { history_id, .. } => *history_id = id,
            _ => {}
        }
    }

    /// Stamp the row's `created_at` (Unix seconds) onto this reply as an RFC
    /// 3339 string. Covers the three durable variants: `Event` (outbound row),
    /// `MessageAccepted` (live inbound ack), `UserMessage` (replayed inbound).
    /// Unlike [`set_history_id`](Self::set_history_id) -- which skips
    /// `UserMessage` because its `history_id` is a required field set at
    /// construction -- this covers `UserMessage` too, since its `created_at`
    /// defaults to `None` and is stamped from the row by the replay path.
    pub fn set_created_at(&mut self, secs: i64) {
        let formatted = format_created_at(secs);
        match self {
            TagmaReply::Event { created_at, .. }
            | TagmaReply::MessageAccepted { created_at, .. }
            | TagmaReply::UserMessage { created_at, .. } => *created_at = Some(formatted),
            _ => {}
        }
    }
}

/// Format a Unix-seconds `created_at` as an RFC 3339 string. `from_unix_timestamp`
/// yields UTC, which `time`'s `Rfc3339` renders with a `Z` suffix
/// (`2026-07-26T12:34:56Z`). Falls back to the epoch for inputs outside the
/// representable range (never expected for a real `created_at`).
fn format_created_at(secs: i64) -> String {
    use time::format_description::well_known::Rfc3339;
    OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_archeion_common::ids::{ParticipantId, ParticipantKind, TagmaId, UserId};
    use time::OffsetDateTime;

    #[test]
    fn participant_serializes_to_struct_shape() {
        let p = Participant {
            id: ParticipantId::for_user(&UserId::from("u1".to_string())),
            kind: ParticipantKind::Human,
            handle: "Alice".into(),
            tagma_id: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Wire labels are the struct field names + snake_case kind.
        assert_eq!(parsed["kind"], "human");
        assert_eq!(parsed["handle"], "Alice");
        assert_eq!(parsed["id"].as_str().unwrap(), p.id.as_ref());
        // tagma_id is omitted when None (skip_serializing_if).
        assert!(parsed.get("tagma_id").is_none());

        let a = Participant {
            id: ParticipantId::for_tagma(&TagmaId::from("t1".to_string())),
            kind: ParticipantKind::Agent,
            handle: "Tagma".into(),
            tagma_id: Some(TagmaId::from("t1".to_string())),
        };
        let json = serde_json::to_string(&a).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["kind"], "agent");
        // tagma_id is present (not omitted) when Some.
        assert_eq!(parsed["tagma_id"].as_str().unwrap(), "t1");
    }

    /// The attachment field is optional both ways: a message without one
    /// serializes to the historical `{text}` shape (old readers unaffected)
    /// and an old `{text}` payload deserializes with `attachment: None`.
    #[test]
    fn room_message_attachment_is_optional_both_ways() {
        // Old wire shape: no attachment key at all.
        let legacy = serde_json::json!({ "text": "hi" });
        let msg: RoomMessage = serde_json::from_value(legacy).unwrap();
        assert_eq!(msg.text, "hi");
        assert!(msg.attachment.is_none());
        // And it re-serializes to the same shape (no empty object).
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"text":"hi"}"#);

        // With an attachment: the full shape round-trips.
        let msg = RoomMessage {
            text: "report".into(),
            attachment: Some(FileAttachment {
                record_id: uuid::Uuid::nil(),
                name: "report.pdf".into(),
                size: 1234,
                modality: None,
            }),
            quote: None,
            mentions: Vec::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"record_id\""));
        assert!(json.contains("report.pdf"));
        let back: RoomMessage = serde_json::from_str(&json).unwrap();
        let att = back.attachment.expect("attachment survives the round trip");
        assert_eq!(att.name, "report.pdf");
        assert_eq!(att.size, 1234);
    }

    /// The bilateral send attachment mirrors the room contract's optional-both-
    /// ways rule: a send without one serializes to the historical shape (no
    /// `attachment` key) and an old payload deserializes with `attachment: None`.
    #[test]
    fn send_message_attachment_is_optional_both_ways() {
        // Old wire shape: no attachment key at all.
        let legacy = r#"{"op":"send_message","req_id":1,"text":"hi"}"#;
        let req: TagmaRequest = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            req,
            TagmaRequest::SendMessage {
                attachment: None,
                ..
            }
        ));
        // And it re-serializes to the same shape.
        let json = serde_json::to_string(&TagmaRequest::SendMessage {
            req_id: 1,
            text: "hi".into(),
            attachment: None,
            quote: None,
        })
        .unwrap();
        assert_eq!(json, legacy);

        // With an attachment: the full shape round-trips.
        let json = serde_json::to_string(&TagmaRequest::SendMessage {
            req_id: 2,
            text: "hi".into(),
            attachment: Some(FileAttachment {
                record_id: uuid::Uuid::nil(),
                name: "report.pdf".into(),
                size: 1234,
                modality: None,
            }),
            quote: None,
        })
        .unwrap();
        let back: TagmaRequest = serde_json::from_str(&json).unwrap();
        match back {
            TagmaRequest::SendMessage {
                req_id,
                attachment: Some(att),
                ..
            } => {
                assert_eq!(req_id, 2);
                assert_eq!(att.name, "report.pdf");
                assert_eq!(att.size, 1234);
            }
            _ => panic!("expected SendMessage carrying the attachment"),
        }
    }

    #[test]
    fn envelope_round_trips() {
        let env = Envelope {
            channel_id: ChannelId::from("c1".to_string()),
            sender: Participant {
                id: ParticipantId::for_user(&UserId::from("u1".to_string())),
                kind: ParticipantKind::Human,
                handle: "Alice".into(),
                tagma_id: None,
            },
            sequence_n: 3,
            trace_id: TraceId::from("t1".to_string()),
            timestamp: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            ciphertext: Ciphertext(vec![1, 2, 3]),
        };
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sequence_n, 3);
        assert_eq!(back.ciphertext.0, vec![1, 2, 3]);
    }
    #[test]
    fn envelope_channel_id_alias_reads_old_wire_name() {
        // A pre-rename peer (or an in-flight envelope during a rolling deploy)
        // writes the routing target as `conversation_id`; the serde alias
        // keeps those payloads readable while every local write emits
        // `channel_id` only.
        let env = Envelope {
            channel_id: ChannelId::from("c1".to_string()),
            sender: Participant {
                id: ParticipantId::for_user(&UserId::from("u1".to_string())),
                kind: ParticipantKind::Human,
                handle: "Alice".into(),
                tagma_id: None,
            },
            sequence_n: 3,
            trace_id: TraceId::from("t1".to_string()),
            timestamp: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            ciphertext: Ciphertext(vec![1, 2, 3]),
        };
        // Serialize, then hand-rewrite the key to the OLD wire name to model
        // a peer that has not shipped the rename.
        let mut old = serde_json::to_value(&env).unwrap();
        let v = old.as_object_mut().unwrap().remove("channel_id").unwrap();
        old.as_object_mut()
            .unwrap()
            .insert("conversation_id".into(), v);
        // The alias reads the old name; the field carries the same target.
        let back: Envelope = serde_json::from_value(old).unwrap();
        assert_eq!(back.channel_id, env.channel_id);
        // Local writes emit the new name only -- the old name never appears.
        let out = serde_json::to_value(&env).unwrap();
        assert!(out.get("channel_id").is_some());
        assert!(out.get("conversation_id").is_none());
        // And the new name round-trips identically.
        let again: Envelope = serde_json::from_value(out).unwrap();
        assert_eq!(again.channel_id, env.channel_id);
    }

    #[test]
    fn room_message_round_trips() {
        let msg = RoomMessage {
            text: "hi from a room".into(),
            attachment: None,
            quote: None,
            mentions: Vec::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"text":"hi from a room"}"#);
        let back: RoomMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.text, "hi from a room");
    }

    #[test]
    fn room_message_quote_is_optional_on_the_wire() {
        let plain = RoomMessage {
            text: "look".into(),
            attachment: None,
            quote: None,
            mentions: Vec::new(),
        };
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("quote"));
        // A payload without the field (the historical shape) still decodes.
        let old: RoomMessage = serde_json::from_str(&json).unwrap();
        assert!(old.quote.is_none());

        let quoted = RoomMessage {
            text: "look".into(),
            attachment: None,
            quote: Some(42),
            mentions: Vec::new(),
        };
        let json = serde_json::to_string(&quoted).unwrap();
        assert!(json.contains("quote"));
        let back: RoomMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.quote, Some(42));
    }

    /// A mention list is part of the room plaintext: the ids a sender named
    /// survive a decode and a re-encode. Asserted on the wire (a `serde_json::Value`),
    /// not on the struct, so the contract under test is the JSON shape itself.
    #[test]
    fn a_mention_list_survives_a_decode_and_a_re_encode() {
        let named = "6f9619ff-8b86-d011-b42d-00cf4fc964ff";
        let wire = serde_json::json!({ "text": "look", "mentions": [named] });
        let parsed: RoomMessage = serde_json::from_value(wire.clone()).unwrap();
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back.get("mentions"), wire.get("mentions"));
    }

    /// A mention list is absent unless the sender named someone, so an
    /// unmentioned message keeps the historical shape byte-for-byte.
    #[test]
    fn room_message_mentions_are_absent_unless_named() {
        let msg = RoomMessage {
            text: "hi".into(),
            attachment: None,
            quote: None,
            mentions: Vec::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"text":"hi"}"#);
        let back: RoomMessage = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert!(back.mentions.is_empty());
    }

    /// The ids travel in the sender's order, as they were picked.
    #[test]
    fn room_message_mentions_round_trip_in_order() {
        let ada = uuid::Uuid::new_v4().to_string();
        let bo = uuid::Uuid::new_v4().to_string();
        let msg = RoomMessage {
            text: "look".into(),
            attachment: None,
            quote: None,
            mentions: vec![MemberId::from(bo.clone()), MemberId::from(ada.clone())],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: RoomMessage = serde_json::from_str(&json).unwrap();
        let ids: Vec<&str> = back.mentions.iter().map(MemberId::as_ref).collect();
        assert_eq!(ids, vec![bo.as_str(), ada.as_str()]);
    }

    /// An entry that is not a participant id costs that entry, never the frame:
    /// the text still arrives.
    #[test]
    fn a_malformed_mention_entry_costs_only_that_entry() {
        let named = uuid::Uuid::new_v4().to_string();
        let wire = serde_json::json!({
            "text": "look",
            "mentions": ["not-an-id", named, 42, null, { "id": named }],
        });
        let back: RoomMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(back.text, "look");
        assert_eq!(back.mentions.len(), 1);
        assert_eq!(back.mentions[0].as_ref(), named);
    }

    /// A value that is not a list at all reads as no mentions, again without
    /// costing the message.
    #[test]
    fn a_non_list_mention_value_reads_as_no_mentions() {
        let wire = serde_json::json!({ "text": "look", "mentions": "ada" });
        let back: RoomMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(back.text, "look");
        assert!(back.mentions.is_empty());
    }

    /// Naming the same member twice is one mention, at its first position.
    #[test]
    fn mentions_are_deduplicated_in_the_senders_order() {
        let ada = uuid::Uuid::new_v4().to_string();
        let bo = uuid::Uuid::new_v4().to_string();
        let wire = serde_json::json!({ "text": "look", "mentions": [ada, bo, ada] });
        let back: RoomMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(back.mentions.len(), 2);
        assert_eq!(back.mentions[0].as_ref(), ada);
        assert_eq!(back.mentions[1].as_ref(), bo);
    }

    /// The bound is a read-side backstop: a longer list is truncated, and the
    /// ones kept are the first ones.
    #[test]
    fn mentions_beyond_the_bound_are_truncated() {
        let ids: Vec<String> = (0..MENTIONS_MAX + 3)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let wire = serde_json::json!({ "text": "look", "mentions": ids });
        let back: RoomMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(back.mentions.len(), MENTIONS_MAX);
        assert_eq!(
            back.mentions[MENTIONS_MAX - 1].as_ref(),
            ids[MENTIONS_MAX - 1]
        );
    }

    /// The write side refuses what the read side would drop, and it hands the
    /// entry back as the caller wrote it: a caller has to be able to find the
    /// string it sent among its own arguments.
    #[test]
    fn checked_mentions_refuses_an_entry_that_is_not_a_participant_id() {
        let err = checked_mentions(["not-an-id"]).unwrap_err();
        assert_eq!(err, MentionError::Malformed("not-an-id".to_string()));
        assert_eq!(err.to_string(), "not a participant id: not-an-id");
    }

    /// The accepted spellings are the read side's: case folds, the hyphenated
    /// shape is what comes back.
    #[test]
    fn checked_mentions_accepts_the_same_spellings_as_the_read_side() {
        let canonical = uuid::Uuid::new_v4().to_string();
        let checked = checked_mentions([canonical.to_uppercase()]).unwrap();
        assert_eq!(checked.len(), 1);
        assert_eq!(checked[0].as_ref(), canonical);
    }

    /// Repeats fold before the cap counts: naming one member twice is one
    /// member, kept where it first appeared.
    #[test]
    fn checked_mentions_folds_repeats_before_counting() {
        let ada = uuid::Uuid::new_v4().to_string();
        let bo = uuid::Uuid::new_v4().to_string();
        let checked = checked_mentions([ada.clone(), bo.clone(), ada.clone()]).unwrap();
        assert_eq!(checked.len(), 2);
        assert_eq!(checked[0].as_ref(), ada);
        assert_eq!(checked[1].as_ref(), bo);
    }

    /// The cap counts members, not entries: its two neighbours sit on opposite
    /// sides, one distinct member over is refused while a repeat is not.
    #[test]
    fn checked_mentions_caps_members_not_entries() {
        let ids: Vec<String> = (0..MENTIONS_MAX)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        assert_eq!(checked_mentions(&ids).unwrap().len(), MENTIONS_MAX);

        let mut one_member_over = ids.clone();
        one_member_over.push(uuid::Uuid::new_v4().to_string());
        let err = checked_mentions(&one_member_over).unwrap_err();
        assert_eq!(
            err,
            MentionError::TooMany {
                named: MENTIONS_MAX + 1,
                max: MENTIONS_MAX,
            }
        );
        assert_eq!(err.to_string(), "a send names more than 16 members");

        let mut one_entry_over = ids.clone();
        one_entry_over.push(ids[0].clone());
        assert_eq!(
            checked_mentions(&one_entry_over).unwrap().len(),
            MENTIONS_MAX
        );
    }

    /// The accepted set is one shape in two cases (hyphenated, either case);
    /// four other spellings of the same id name nobody. The TS codec holds the
    /// same list, so a change on either side has to be answered on the other.
    #[test]
    fn only_a_hyphenated_id_names_a_participant() {
        let canonical = uuid::Uuid::new_v4().to_string();
        let wire = serde_json::json!({
            "text": "look",
            "mentions": [
                canonical,
                canonical.to_uppercase(),
                format!("{{{canonical}}}"),
                canonical.replace('-', ""),
                format!("urn:uuid:{canonical}"),
                "not-an-id",
            ],
        });
        let back: RoomMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(back.mentions.len(), 1);
        assert_eq!(back.mentions[0].as_ref(), canonical);
    }

    /// An upper-case spelling is accepted and stored in the one canonical case,
    /// so a member keeps a single id whatever case the writer picked.
    #[test]
    fn a_mention_is_stored_in_its_canonical_case() {
        let canonical = uuid::Uuid::new_v4().to_string();
        let upper = canonical.to_uppercase();
        let wire = serde_json::json!({ "text": "look", "mentions": [upper] });
        let back: RoomMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(back.mentions.len(), 1);
        assert_eq!(back.mentions[0].as_ref(), canonical);
    }

    /// Two cases of one member are one mention, which is what makes the id
    /// comparison downstream a byte-for-byte one.
    #[test]
    fn one_member_named_in_two_cases_counts_once() {
        let canonical = uuid::Uuid::new_v4().to_string();
        let upper = canonical.to_uppercase();
        let wire = serde_json::json!({ "text": "look", "mentions": [upper, canonical] });
        let back: RoomMessage = serde_json::from_value(wire).unwrap();
        assert_eq!(back.mentions.len(), 1);
        assert_eq!(back.mentions[0].as_ref(), canonical);
    }

    #[test]
    fn tagma_request_round_trips() {
        let req = TagmaRequest::SendMessage {
            req_id: 7,
            text: "hi".into(),
            attachment: None,
            quote: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"op\":\"send_message\""));
        let back: TagmaRequest = serde_json::from_str(&json).unwrap();
        match back {
            TagmaRequest::SendMessage {
                req_id,
                text,
                attachment: _,
                quote: _,
            } => {
                assert_eq!(req_id, 7);
                assert_eq!(text, "hi");
            }
            _ => panic!("expected SendMessage"),
        }
    }

    #[test]
    fn tagma_reply_variants_round_trip() {
        let cases = vec![
            serde_json::to_string(&TagmaReply::MessageAccepted {
                req_id: 1,
                queue_depth: 0,
                warning: None,
                history_id: 0,
                created_at: None,
                attachment: None,
                quote: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::MessageAccepted {
                req_id: 1,
                queue_depth: 2,
                warning: Some("queue growing".into()),
                history_id: 7,
                created_at: None,
                attachment: None,
                quote: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::Interrupted { req_id: 9 }).unwrap(),
            serde_json::to_string(&TagmaReply::Error {
                req_id: 5,
                status: 502,
                message: "boom".into(),
                code: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::Event {
                event: AuthoredEvent::AssistantContent {
                    content: "hi".into(),
                },
                history_id: 0,
                created_at: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::Event {
                event: AuthoredEvent::AssistantContent {
                    content: "hi".into(),
                },
                history_id: 42,
                created_at: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::UserMessage {
                history_id: 11,
                text: "hi".into(),
                created_at: None,
                attachment: None,
                quote: None,
            })
            .unwrap(),
            serde_json::to_string(&TagmaReply::HistoryBatchEnd {
                req_id: 3,
                count: 10,
                more: true,
                unread: 0,
                cursor: 0,
                undecodable: 0,
            })
            .unwrap(),
        ];
        for json in cases {
            let _: TagmaReply = serde_json::from_str(&json).unwrap();
        }
        // `warning` and `created_at` are omitted when None.
        let none_json = serde_json::to_string(&TagmaReply::MessageAccepted {
            req_id: 1,
            queue_depth: 0,
            warning: None,
            history_id: 0,
            created_at: None,
            attachment: None,
            quote: None,
        })
        .unwrap();
        assert!(!none_json.contains("warning"));
        assert!(!none_json.contains("created_at"));
        // `history_id` / `created_at` default when absent, so an Event serialized
        // without them (a stored payload from before the stamp, or an older peer)
        // still parses.
        let unstamped = r#"{"kind":"event","event":{"type":"assistant_content","content":"hi"}}"#;
        let parsed: TagmaReply = serde_json::from_str(unstamped).unwrap();
        assert!(matches!(
            parsed,
            TagmaReply::Event {
                history_id: 0,
                created_at: None,
                ..
            }
        ));
        // `history_id` round-trips a non-zero value and the field name is pinned.
        let stamped = serde_json::to_string(&TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "hi".into(),
            },
            history_id: 42,
            created_at: None,
        })
        .unwrap();
        assert!(
            stamped.contains("\"history_id\":42"),
            "history_id field name pinned: {stamped}"
        );
        // UserMessage / HistoryBatchEnd field names are pinned (created_at is
        // omitted when None, so the UserMessage form is unchanged).
        let um = serde_json::to_string(&TagmaReply::UserMessage {
            history_id: 11,
            text: "hi".into(),
            created_at: None,
            attachment: None,
            quote: None,
        })
        .unwrap();
        assert_eq!(um, r#"{"kind":"user_message","history_id":11,"text":"hi"}"#);
        let hbe = serde_json::to_string(&TagmaReply::HistoryBatchEnd {
            req_id: 3,
            count: 10,
            more: true,
            unread: 4,
            cursor: 9,
            undecodable: 0,
        })
        .unwrap();
        assert_eq!(
            hbe,
            r#"{"kind":"history_batch_end","req_id":3,"count":10,"more":true,"unread":4,"cursor":9,"undecodable":0}"#
        );
        // A batch marker from a tagma that predates `unread` still parses.
        let old: TagmaReply = serde_json::from_str(
            r#"{"kind":"history_batch_end","req_id":3,"count":10,"more":true}"#,
        )
        .unwrap();
        assert!(matches!(
            old,
            TagmaReply::HistoryBatchEnd {
                unread: 0,
                undecodable: 0,
                ..
            }
        ));
        // set_history_id stamps Event and MessageAccepted; leaves others alone.
        let mut ev = TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "hi".into(),
            },
            history_id: 0,
            created_at: None,
        };
        ev.set_history_id(99);
        assert!(matches!(ev, TagmaReply::Event { history_id: 99, .. }));
        let mut ack = TagmaReply::MessageAccepted {
            req_id: 1,
            queue_depth: 0,
            warning: None,
            history_id: 0,
            created_at: None,
            attachment: None,
            quote: None,
        };
        ack.set_history_id(99);
        assert!(matches!(
            ack,
            TagmaReply::MessageAccepted { history_id: 99, .. }
        ));
        let mut interrupted = TagmaReply::Interrupted { req_id: 1 };
        interrupted.set_history_id(99);
        assert!(matches!(interrupted, TagmaReply::Interrupted { req_id: 1 }));
    }

    #[test]
    fn set_created_at_formats_and_covers_durable_variants() {
        // `time`'s Rfc3339 special-cases UTC to a `Z` suffix (not `+00:00`).
        assert_eq!(format_created_at(1_785_069_296), "2026-07-26T12:34:56Z");

        let mut ev = TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "hi".into(),
            },
            history_id: 0,
            created_at: None,
        };
        ev.set_created_at(1_785_069_296);
        assert!(
            matches!(ev, TagmaReply::Event { ref created_at, .. } if created_at.as_deref() == Some("2026-07-26T12:34:56Z"))
        );

        let mut ack = TagmaReply::MessageAccepted {
            req_id: 1,
            queue_depth: 0,
            warning: None,
            history_id: 0,
            created_at: None,
            attachment: None,
            quote: None,
        };
        ack.set_created_at(1_785_069_296);
        assert!(matches!(
            ack,
            TagmaReply::MessageAccepted { ref created_at, .. }
                if created_at.as_deref() == Some("2026-07-26T12:34:56Z")
        ));

        let mut um = TagmaReply::UserMessage {
            history_id: 11,
            text: "hi".into(),
            created_at: None,
            attachment: None,
            quote: None,
        };
        um.set_created_at(1_785_069_296);
        assert!(matches!(
            um,
            TagmaReply::UserMessage { ref created_at, .. }
                if created_at.as_deref() == Some("2026-07-26T12:34:56Z")
        ));

        // Non-durable variants are left untouched (no field to stamp).
        let mut interrupted = TagmaReply::Interrupted { req_id: 1 };
        interrupted.set_created_at(1_785_069_296);
        assert!(matches!(interrupted, TagmaReply::Interrupted { req_id: 1 }));
    }

    #[test]
    fn tagma_control_history_round_trips() {
        // latest mode
        let latest = serde_json::to_string(&TagmaControl::History {
            req_id: 1,
            after: None,
            before: None,
            limit: 50,
        })
        .unwrap();
        assert_eq!(
            latest,
            r#"{"op":"history","req_id":1,"after":null,"before":null,"limit":50}"#
        );
        let _: TagmaControl = serde_json::from_str(&latest).unwrap();
        // after mode
        let after = serde_json::to_string(&TagmaControl::History {
            req_id: 2,
            after: Some(10),
            before: None,
            limit: 50,
        })
        .unwrap();
        let parsed: TagmaControl = serde_json::from_str(&after).unwrap();
        assert!(matches!(
            parsed,
            TagmaControl::History {
                after: Some(10),
                before: None,
                limit: 50,
                ..
            }
        ));
        // before mode
        let before = serde_json::to_string(&TagmaControl::History {
            req_id: 3,
            after: None,
            before: Some(20),
            limit: 5,
        })
        .unwrap();
        let parsed: TagmaControl = serde_json::from_str(&before).unwrap();
        assert!(matches!(
            parsed,
            TagmaControl::History {
                after: None,
                before: Some(20),
                limit: 5,
                ..
            }
        ));
    }

    #[test]
    fn request_and_control_op_tags_are_disjoint() {
        // The relay dispatches by the `op` discriminant; the two enums must not
        // share a tag, and `history` must not parse as a TagmaRequest (and vice
        // versa for send_message/interrupt).
        let history = serde_json::to_string(&TagmaControl::History {
            req_id: 1,
            after: None,
            before: None,
            limit: 1,
        })
        .unwrap();
        assert!(
            serde_json::from_str::<TagmaRequest>(&history).is_err(),
            "history op must not parse as a TagmaRequest"
        );
        let send = serde_json::to_string(&TagmaRequest::SendMessage {
            req_id: 1,
            text: "x".into(),
            attachment: None,
            quote: None,
        })
        .unwrap();
        assert!(
            serde_json::from_str::<TagmaControl>(&send).is_err(),
            "send_message op must not parse as a TagmaControl"
        );
    }

    #[test]
    fn tagma_request_op_discriminants_are_disjoint() {
        let req = serde_json::to_string(&TagmaRequest::SendMessage {
            req_id: 1,
            text: "hi".into(),
            attachment: None,
            quote: None,
        })
        .unwrap();
        assert!(req.contains("\"op\":\"send_message\""));
        let req = serde_json::to_string(&TagmaRequest::Interrupt { req_id: 2 }).unwrap();
        assert!(req.contains("\"op\":\"interrupt\""));
    }
}
