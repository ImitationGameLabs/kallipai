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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_log_names_itself_in_the_command() {
        assert_eq!(
            QuoteLog::Room("ops").read_command(7),
            "kallip lesche read --room ops --quote 7"
        );
        assert_eq!(
            QuoteLog::Direct("3f2a").read_command(7),
            "kallip lesche read --session 3f2a --quote 7"
        );
        assert_eq!(
            QuoteLog::Bilateral.read_command(42),
            "kallip lesche read --quote 42"
        );
    }
}
