//! `lesche` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};
use kallipai_lesche_common::direct::DirectSessionId;
use kallipai_lesche_common::ids::check_segment;
use kallipai_lesche_common::rooms::RoomId;

/// Deliver messages via the tagma's relay (the lesche data-plane).
/// Targets the calling agent (resolved from `KALLIPAI_ID`), like `activity`.
///
/// The primitive is "send a message", not "reply": a message may be a
/// response, a proactive heads-up, or (future) a file. Three addressing
/// forms: bare (the bilateral 1:1 with the user), `--room`, `--tagma`.
#[derive(Subcommand)]
pub(crate) enum LescheCommand {
    /// Send a text message: to the user (default), a room (`--room`), or a
    /// peer tagma's direct session (`--tagma`).
    /// The user's client renders messages as Markdown.
    Send(SendArgs),
    /// List the rooms this tagma has joined (so you can address them with
    /// `send --room <room>`). See also `sessions` for every addressable
    /// surface in one list.
    Rooms,
    /// Read a conversation's history (`--room` or `--tagma` required).
    Read(ReadArgs),
    /// List every addressable surface in one list: the user's bilateral 1:1,
    /// the joined rooms, and this tagma's direct sessions (each with kind,
    /// id, and peer metadata).
    Sessions,
}

/// Args for `kallip lesche send`. The text is read from the full stdin
/// (multiline — pipe, heredoc, or `< file`); prefer a quoted heredoc
/// `<<'EOF'` so shell expansion cannot corrupt it.
#[derive(Args)]
pub(crate) struct SendArgs {
    /// The room id to send into. Omit for the bilateral 1:1
    /// conversation; pass the room id (copied verbatim from the inbound
    /// `[From: ... | room <id>]` header) to reply in a multi-member room.
    #[arg(long, allow_hyphen_values = true, conflicts_with = "tagma")]
    pub room: Option<String>,
    /// The peer tagma id to send to: `kallip lesche sessions` pairs it with
    /// the session id in the inbound `[From: ... | direct <session>]` header
    /// (the header's parens carry the sender's participant id, not the tagma
    /// id). Sends into that tagma's direct session; the first send
    /// creates it (idempotent to re-send). Note: attachments must live in a
    /// workspace the peer can read — a private-area file record fails the
    /// peer's fetch with 403. The id rides a request path, so an empty
    /// value or a dot segment is refused here.
    #[arg(long, allow_hyphen_values = true, value_parser = tagma_id)]
    pub tagma: Option<String>,
    /// The position, in that conversation's own log, that this message answers
    /// -- a room's `seq`, or that session's own `seq`. A room or `--tagma`
    /// send: the bilateral 1:1 has no such log, so a send there carrying one
    /// is rejected.
    #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
    pub quote: Option<i64>,
    /// The members this send names, one flag per participant id (repeatable).
    /// A room send only: an id names a member of that room, so a send at the
    /// bilateral 1:1 or at a peer tagma's session is rejected. The ids reach
    /// the tagma as written -- whether each one names a member is answered
    /// where the room is, not here.
    #[arg(long = "mention", allow_hyphen_values = true)]
    pub mention: Vec<String>,
}

/// Args for `kallip lesche read` (pull a conversation's history).
///
/// A read names one log, then asks it for a page or for one row. The targets
/// name the logs a delivered reference can point into: `--room <room>` a room's
/// transcript, `--session <session>` a direct session's (the `direct <session>`
/// value an inbound header spells), `--tagma <peer>` the session derived from
/// that peer pair, and no target at all the bilateral 1:1 with the user.
/// `--quote` reads the single row a reference names; `--after`, `--before` and
/// `--limit` read a page.
#[derive(Args)]
pub(crate) struct ReadArgs {
    /// The room id to read from (one of the ids listed by `kallip lesche
    /// rooms`). The id rides a request path, so it has to be the id the lesche
    /// minted -- a canonical UUID -- and is refused before any request is made
    /// if it is not.
    #[arg(
        long,
        allow_hyphen_values = true,
        value_parser = room_id,
        required_unless_present_any = ["tagma", "session", "quote"],
        conflicts_with_all = ["tagma", "session"]
    )]
    pub room: Option<String>,
    /// The peer tagma id to read -- the id `kallip lesche sessions` lists for
    /// the peer, not the `UUID` in a message header's parens (that value is
    /// the sender's participant id). The session id is derived from the pair,
    /// so you never type a session id. The id rides a request path, so an
    /// empty value or a dot segment is refused here.
    #[arg(
        long,
        allow_hyphen_values = true,
        value_parser = tagma_id,
        conflicts_with_all = ["room", "session"]
    )]
    pub tagma: Option<String>,
    /// The direct-session id to read -- the `direct <session>` value in an
    /// inbound message header. Names the log itself rather than deriving it
    /// from a peer pair, which is what a delivered message's read-back command
    /// spells. The log is read forward only, so `--before` is refused here:
    /// `--quote` reads one row of it and `--after`/`--limit` read a page. The
    /// id is a path segment, so it has to be the canonical spelling the tagma
    /// answers to; anything else is refused before the request is made.
    #[arg(
        long,
        allow_hyphen_values = true,
        value_parser = session_id,
        conflicts_with_all = ["room", "tagma"]
    )]
    pub session: Option<String>,
    /// Return only messages with `seq > <N>` (exclusive). Default: from the
    /// start.
    #[arg(long = "after")]
    pub after_seq: Option<i64>,
    /// Return the page of OLDER messages ending below this position
    /// (`seq < before_seq`, exclusive), oldest first. A room read only: the
    /// page below a position is a room's own cursor, and a direct session's
    /// log is read forward only, so the flag is refused at a direct-session
    /// target rather than dropped -- dropping it would replay the session
    /// from its start. Omit to page forward: an omitted cursor is simply
    /// absent, because `0` would read as "no row" rather than as "from the
    /// start".
    #[arg(
        long = "before",
        value_parser = clap::value_parser!(i64).range(1..),
        conflicts_with_all = ["after_seq", "quote"]
    )]
    pub before_seq: Option<i64>,
    /// Read only the one message this position names -- the row a
    /// `| quote=<seq>` segment points at. A seq names a row in that
    /// conversation's own log, so this rides a room read, a direct-session read,
    /// or -- with no target at all -- the bilateral 1:1, whose rows are ids. The
    /// answer is that single block; a position this read cannot show is an
    /// error, never an empty output.
    #[arg(
        long,
        value_parser = clap::value_parser!(i64).range(1..),
        conflicts_with_all = ["after_seq", "before_seq", "limit"]
    )]
    pub quote: Option<i64>,
    /// Max messages to return (server-clamped). Not accepted beside `--quote`,
    /// which always reads one row.
    #[arg(long)]
    pub limit: Option<u64>,
}

/// A room id as the lesche mints it: a canonical UUID.
///
/// The value rides one request path segment, so a value of another shape is not
/// a room this CLI can address -- it would ask a route for a resource that
/// cannot exist. The judgement is the room type's own ([`RoomId::parse`]), the
/// same one the tagma applies where it can see the room.
fn room_id(raw: &str) -> Result<String, String> {
    RoomId::parse(raw)
        .map(String::from)
        .ok_or_else(|| format!("{raw:?} is not a room id: expected a canonical UUID"))
}

/// A direct-session id as the tagma derives it: a canonical UUID.
///
/// Same rule and same judgement as [`DirectSessionId::parse`], so the CLI and
/// the route it calls refuse exactly the same spellings.
fn session_id(raw: &str) -> Result<String, String> {
    DirectSessionId::parse(raw)
        .map(|id| id.as_ref().to_owned())
        .ok_or_else(|| format!("{raw:?} is not a session id: expected a canonical UUID"))
}

/// A peer tagma id, as far as it can be judged here: not empty, and not a dot
/// segment.
///
/// The value rides one request path segment, and a dot segment survives
/// percent-encoding -- a URL parser resolves it after decoding -- so the CLI
/// refuses it here rather than letting the request reach a route the caller
/// did not name. The judgement is [`check_segment`], the one the encoder
/// applies.
fn tagma_id(raw: &str) -> Result<String, String> {
    check_segment(raw)
        .map(|()| raw.to_owned())
        .map_err(|err| format!("{raw:?} is not a tagma id: {err}"))
}

#[cfg(test)]
mod tests {
    use super::{room_id, session_id, tagma_id};

    /// A caller-supplied id has to be the canonical spelling the lesche mints:
    /// the value rides a request path, so any other spelling is refused here
    /// rather than asked for over the wire.
    #[test]
    fn only_a_canonical_id_is_accepted() {
        let id = "00000000-0000-4000-8000-000000000ee5";
        assert_eq!(room_id(id).as_deref(), Ok(id));
        assert_eq!(session_id(id).as_deref(), Ok(id));
        for raw in ["../../sessions/abc", "../rooms/abc", "room-1", ""] {
            assert!(room_id(raw).is_err(), "room id accepted: {raw:?}");
            assert!(session_id(raw).is_err(), "session id accepted: {raw:?}");
        }
        // The same id spelled another way would key a second row, so the
        // canonical form is the only one that counts.
        assert!(room_id("00000000-0000-4000-8000-000000000EE5").is_err());
        assert!(session_id("00000000000040008000000000000ee5").is_err());
    }

    /// A peer tagma id rides a request path too, so the values encoding
    /// cannot hold inside a segment are refused at the flag.
    #[test]
    fn a_tagma_id_cannot_be_a_dot_segment() {
        assert_eq!(tagma_id("tagma-abc").as_deref(), Ok("tagma-abc"));
        for raw in ["", ".", ".."] {
            assert!(tagma_id(raw).is_err(), "tagma id accepted: {raw:?}");
        }
    }
}
