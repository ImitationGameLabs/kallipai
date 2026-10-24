//! The read-back command a delivered message's quote reference spells.
//!
//! A `| quote=<position>` segment names a row in one conversation's log, and
//! the reference lines under the prompt header tell the reader how to print that
//! row in full. The command text lives here, shared by the tagma that renders it
//! and the CLI that has to accept it, so a rendered command cannot drift from
//! the parser: `kallip`'s own tests feed these very strings back through its
//! clap parser rather than comparing them to literals.

/// The log a quoted position is a row of.
///
/// A position means nothing outside its own log, so the read-back command has to
/// name that log: a room's transcript, a direct session's, or the bilateral 1:1
/// with the user (which needs no flag -- a bare read is the 1:1 one). The three
/// surfaces a delivered message arrives on name exactly these three logs, so
/// this enum is the only table of that pairing: the tagma's `Surface::quote_log`
/// maps onto it rather than enumerating the same three logs again.
///
/// The id a log names is the UUID the surface issued (a room id, a direct
/// session id): the command prints it unquoted, and a reader splits the
/// command on whitespace, so an id spelling a space or a shell metacharacter
/// would not survive a paste. Ids are the lesche's own UUIDs, never values a
/// message supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteLog<'a> {
    /// A room's own `seq`.
    Room(&'a str),
    /// A direct session's own `seq`, addressed by the session id -- the value an
    /// inbound header already spells (`direct <session>`).
    Direct(&'a str),
    /// A `chat_history` row id in the bilateral 1:1.
    Bilateral,
}

impl QuoteLog<'_> {
    /// The `kallip lesche read` invocation that prints the single row at
    /// `position` in this log.
    pub fn read_command(&self, position: i64) -> String {
        match self {
            QuoteLog::Room(id) => format!("kallip lesche read --room {id} --quote {position}"),
            QuoteLog::Direct(id) => format!("kallip lesche read --session {id} --quote {position}"),
            QuoteLog::Bilateral => format!("kallip lesche read --quote {position}"),
        }
    }
}

/// The opening of a rendered row header: `[seq=<position> `, with the space
/// that separates the position from the rest of the header.
///
/// The renderer writes it and a `--quote` read checks its answer against it,
/// so both sides take the opening from here: a header shape that moved on one
/// side alone would otherwise leave the check looking for a line nothing
/// writes any more.
pub fn block_prefix(position: i64) -> String {
    format!("[seq={position} ")
}

/// Whether `text` opens with this conversation's render of the row at
/// `position`.
///
/// A page that landed beside the position names its own `seq`, so a mismatch
/// is exactly the answer that this read holds no such row.
pub fn names_position(text: &str, position: i64) -> bool {
    text.starts_with(&block_prefix(position))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_log_names_itself_in_the_command() {
        assert_eq!(
            QuoteLog::Room("9f3a1c2e-8b7d-4f6a-9c1e-2d3b4a5c6d7e").read_command(7),
            "kallip lesche read --room 9f3a1c2e-8b7d-4f6a-9c1e-2d3b4a5c6d7e --quote 7"
        );
        assert_eq!(
            QuoteLog::Direct("c4b2a8f0-1d3e-4a5b-8c9d-0e1f2a3b4c5d").read_command(7),
            "kallip lesche read --session c4b2a8f0-1d3e-4a5b-8c9d-0e1f2a3b4c5d --quote 7"
        );
        assert_eq!(
            QuoteLog::Bilateral.read_command(42),
            "kallip lesche read --quote 42"
        );
    }
}
