//! `lesche` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};

/// Deliver messages via the tagma's relay (the lesche data-plane).
/// Targets the calling agent (resolved from `KALLIP_ID`), like `activity`.
///
/// The primitive is "send a message", not "reply": a message may be a
/// response, a proactive heads-up, or (future) a file. Three addressing
/// forms: bare (the bilateral 1:1 with the user), `--room`, `--tagma`.
#[derive(Subcommand)]
pub(crate) enum LescheCommand {
    /// Send a text message: to the user (default), a room (`--room`), or a
    /// peer tagma's direct session (`--tagma`).
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
    /// The peer tagma id to send to (copied from the inbound
    /// `[From: ... (<tagma-id>) | direct <session>]` header or from
    /// `sessions`). Sends into that tagma's direct session; the first send
    /// creates it (idempotent to re-send). Note: attachments must live in a
    /// workspace the peer can read — a private-area file record fails the
    /// peer's fetch with 403.
    #[arg(long, allow_hyphen_values = true)]
    pub tagma: Option<String>,
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
    /// The peer tagma id to read (the session id is derived from the pair,
    /// so you never type a session id).
    #[arg(long, allow_hyphen_values = true)]
    pub tagma: Option<String>,
    /// Return only messages with `seq > after_seq` (exclusive). Default: from
    /// the start.
    #[arg(long)]
    pub after_seq: Option<i64>,
    /// Max messages to return (server-clamped).
    #[arg(long)]
    pub limit: Option<u64>,
}
