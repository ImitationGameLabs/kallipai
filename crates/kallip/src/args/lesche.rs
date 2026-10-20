//! `lesche` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};

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
    /// peer's fetch with 403.
    #[arg(long, allow_hyphen_values = true)]
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

/// Args for `kallip lesche read` (pull a conversation's history; exactly
/// one of `--room` / `--tagma`).
#[derive(Args)]
pub(crate) struct ReadArgs {
    /// The room id to read from (one of the ids listed by `kallip lesche
    /// rooms`).
    #[arg(
        long,
        allow_hyphen_values = true,
        required_unless_present = "tagma",
        conflicts_with = "tagma"
    )]
    pub room: Option<String>,
    /// The peer tagma id to read -- the id `kallip lesche sessions` lists for
    /// the peer, not the `UUID` in a message header's parens (that value is
    /// the sender's participant id). The session id is derived from the pair,
    /// so you never type a session id.
    #[arg(long, allow_hyphen_values = true)]
    pub tagma: Option<String>,
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
    /// conversation's own log, so this rides a room read or a direct-session
    /// read alike. The answer is that single block; a position this read
    /// cannot show is an error, never an empty output.
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
