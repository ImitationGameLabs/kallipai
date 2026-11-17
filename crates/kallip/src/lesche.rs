//! The `kallip lesche` family: the relay's send and read faces.
//!
//! Every command is self-only: the agent id comes from `KALLIPAI_ID`, so a
//! send, a room list and a read all speak for the agent whose shell runs them.
//!
//! The text a read prints is the tagma's own render, one bracketed block per
//! message, and this module passes it through untouched. The one verdict it
//! adds is the `--quote` check: a read that asked for one row says whether
//! that row came back (`expect_named_block`), rather than printing a
//! neighbouring row as if it were the row named.

use anyhow::Result;
use kallipai_client::types::LescheSessionEntry;
use kallipai_client::{DirectReadQuery, MessageDelivery, RoomReadQuery, TagmaClient};
use kallipai_lesche_common::rooms::{ThreadChoice, ThreadSubscriptionView};

use crate::args::lesche::{LescheCommand, ReadArgs, SendArgs, ThreadCommand};
use crate::{agent_id_from_env, read_text_stdin};

/// Run one `lesche` command against the tagma's relay.
pub(crate) async fn run_lesche(client: &TagmaClient, cmd: &LescheCommand) -> Result<()> {
    match cmd {
        LescheCommand::Send(args) => {
            // Self-only: send as the calling agent (KALLIPAI_ID). The text
            // is the full stdin (multiline). Deliver via the tagma's relay
            // first, then print the stable marker only on success — so a
            // failed POST (relay down, burst cap, etc.) does not let local
            // clients render a message that was never delivered.
            let id = agent_id_from_env()?;
            // Validate the flags before reading stdin: a wrong target, or a
            // quote, or a mention without a room, is answerable without the message body.
            let quote = send_quote(args)?;
            let mentions = send_mentions(args)?;
            let thread = send_thread(args)?;
            let text = read_text_stdin()?;
            client
                .post_message_delivery(
                    &id,
                    &text,
                    MessageDelivery {
                        room: args.room.as_deref(),
                        tagma: args.tagma.as_deref(),
                        quote,
                        mentions: &mentions,
                        thread,
                    },
                )
                .await?;
            println!("{}", kallipai_common::message::marker_line(&text));
        }
        LescheCommand::Rooms => {
            // Self-only: list the calling tagma's joined rooms.
            let id = agent_id_from_env()?;
            let rooms = client.list_joined_rooms(&id).await?;
            if rooms.is_empty() {
                println!("(no rooms joined)");
            } else {
                for room in rooms {
                    println!("{room}");
                }
            }
        }
        LescheCommand::Read(args) => {
            // Self-only: read one conversation's history (room or direct
            // session). The tagma route renders a text block (one bracketed
            // block per message), so print it verbatim (no trailing newline
            // added).
            let id = agent_id_from_env()?;
            // The target check runs for every read; each target assembles
            // its own page.
            reject_backward_cursor_at_non_room(args)?;
            let text = match (&args.room, &args.tagma, &args.session) {
                (Some(room), _, _) => {
                    client
                        .read_room_messages(&id, room, room_read_query(args)?)
                        .await?
                }
                (_, Some(peer), _) => {
                    client
                        .read_direct_session_messages(&id, peer, direct_read_query(args))
                        .await?
                }
                (_, _, Some(session)) => {
                    client
                        .read_direct_session_by_id_messages(&id, session, direct_read_query(args))
                        .await?
                }
                // No target: a bare `--quote` names one row of the bilateral
                // 1:1 -- the form a 1:1 message's read-back command spells.
                // clap ties every other combination to a target, so the arm
                // below is the 1:1 read.
                (None, None, None) => match args.quote {
                    Some(quote) => client.read_quote(&id, quote).await?,
                    None => anyhow::bail!("--room, --tagma, --session or --quote is required"),
                },
            };
            // A `--quote` read shows exactly the row it names: the page
            // fields above are how the row is fetched; this is the verdict
            // on what came back.
            let text = match args.quote {
                Some(seq) => expect_named_block(text, seq)?,
                None => text,
            };
            print!("{text}");
        }
        LescheCommand::Thread(args) => {
            // Self-only: the choice is the calling agent's own standing
            // (KALLIPAI_ID), and the tagma takes the member from the token, so
            // no flag can name another member.
            let id = agent_id_from_env()?;
            match &args.command {
                ThreadCommand::Follow(target) => {
                    let view = client
                        .set_thread_subscription(
                            &id,
                            &target.room,
                            target.root,
                            ThreadChoice::Following,
                        )
                        .await?;
                    println!("{}", render_thread_choice(&target.room, target.root, &view));
                }
                ThreadCommand::Mute(target) => {
                    let view = client
                        .set_thread_subscription(
                            &id,
                            &target.room,
                            target.root,
                            ThreadChoice::Muted,
                        )
                        .await?;
                    println!("{}", render_thread_choice(&target.room, target.root, &view));
                }
                ThreadCommand::Default(target) => {
                    client
                        .clear_thread_subscription(&id, &target.room, target.root)
                        .await?;
                    println!("thread {} in {}: default", target.root, target.room);
                }
            }
        }
        LescheCommand::Sessions => {
            // Self-only: every addressable surface in one list. The tagma
            // aggregates (bilateral + rooms + direct sessions); print one
            // line per surface so the ids are copy-pastable into
            // `send --room` / `send --tagma` / `read --tagma`.
            let id = agent_id_from_env()?;
            let entries = client.list_lesche_sessions(&id).await?;
            if entries.is_empty() {
                println!("(no sessions)");
            }
            for entry in entries {
                println!("{}", render_session_line(&entry));
            }
        }
    }
    Ok(())
}

/// One line saying what the caller holds in that thread now.
///
/// `since_seq` is a boundary, not a click: replies at or before it keep the
/// default, so the line reads `after seq <n>` rather than naming a moment the
/// member acted.
fn render_thread_choice(room: &str, root: i64, view: &ThreadSubscriptionView) -> String {
    let state = view.choice.map(ThreadChoice::as_str).unwrap_or("default");
    match view.since_seq {
        Some(seq) => format!("thread {root} in {room}: {state}, after seq {seq}"),
        None => format!("thread {root} in {room}: {state}"),
    }
}

/// Refuse a backward cursor at a target that has no such rows. `--before`
/// names a position in a room's log and asks for the page ending below it;
/// a direct-session read takes a forward cursor only (`after_seq` and
/// `limit`), so the route would never see the flag and the request would
/// quietly replay that session from its start instead of ending a page.
///
/// A `--quote` rides either target: it names one row, not a page edge, and
/// a direct read reaches that row from below (`direct_read_query`).
///
/// Flag-vs-flag conflicts live in clap; a flag against the target lives
/// here, because clap cannot say what a seq names.
fn reject_backward_cursor_at_non_room(args: &ReadArgs) -> Result<()> {
    if args.room.is_some() || args.before_seq.is_none() {
        return Ok(());
    }
    anyhow::bail!("--before rides a room read: a seq names a row in that room's log");
}

/// The page a room read asks for. `quote` is the one-row lookup behind a
/// `| quote=<seq>` segment, so it fixes the page to a single row and carries no
/// page size of its own (clap refuses `--limit` beside it).
fn room_read_query(args: &ReadArgs) -> Result<RoomReadQuery> {
    if let Some(seq) = args.quote {
        let Some(before_seq) = seq.checked_add(1) else {
            anyhow::bail!("--quote {seq} is out of range");
        };
        return Ok(RoomReadQuery {
            after_seq: None,
            before_seq: Some(before_seq),
            limit: Some(1),
        });
    }
    Ok(RoomReadQuery {
        after_seq: args.after_seq,
        before_seq: args.before_seq,
        limit: args.limit,
    })
}

/// The page a direct-session read asks for. `quote` is the one-row lookup
/// behind a `| quote=<seq>` segment: the direct log is read forward only, so
/// the row at `seq` is reached from below, with a page of one. Without a
/// quote the caller's own cursor and page size pass through.
fn direct_read_query(args: &ReadArgs) -> DirectReadQuery {
    match args.quote {
        Some(seq) => DirectReadQuery {
            // `--quote` is a flag clap keeps at one or above (`.range(1..)`),
            // so this subtraction has no floor to hit.
            after_seq: Some(seq - 1),
            limit: Some(1),
        },
        None => DirectReadQuery {
            after_seq: args.after_seq,
            limit: args.limit,
        },
    }
}

/// The block a `--quote` read may print: the conversation's own render of the
/// row at `seq`. A first block naming another position, or no block at all,
/// means this read holds nothing to show at `seq` -- a verdict to report,
/// never to render as silence.
///
/// Only the first block is this layer's business. The page size belongs to the
/// data plane: the read asks for one row and that side clamps the page to one,
/// so whether an answer holds a second block is not something plain text can
/// decide -- a row's body may carry block headers of its own. What is checked
/// here is whether the first block is the row that was asked for.
///
/// "Nothing to show" is not the same as "no such row": the renderer skips rows
/// whose payload no longer parses, so the verdict stays a statement about this
/// read.
///
/// The prefix carries the seq this read asked for, not a shape test on a
/// block header: when the named row does not exist, the page lands on the
/// row beside it, which names its own seq, and that mismatch is the verdict.
///
/// The prefix itself comes from `kallipai_common::readback`, the same source
/// the tagma's row render writes with, so the check cannot drift from the
/// shape it checks for.
fn expect_named_block(text: String, seq: i64) -> Result<String> {
    if !kallipai_common::readback::names_position(&text, seq) {
        anyhow::bail!("this read holds no readable row at seq {seq}");
    }
    Ok(text)
}

/// The quote position a `lesche send` may carry: a seq names a row in a
/// conversation's own log, so the flag rides a room send or a direct-session
/// send and points at nothing from the bilateral 1:1. A flag against the
/// target lives here rather than in clap (see
/// `reject_backward_cursor_at_non_room`); the tagma refuses the same send
/// with a 400, one hop later.
fn send_quote(args: &SendArgs) -> Result<Option<i64>> {
    match args.quote {
        Some(_) if args.room.is_none() && args.tagma.is_none() => {
            anyhow::bail!("--quote rides a room or direct send: a seq names a row in that log")
        }
        quote => Ok(quote),
    }
}

/// The thread a `lesche send` declares: a position in a room's log, so the
/// flag rides a room send only -- the bilateral 1:1 and a peer tagma's direct
/// session have no threads. The tagma answers the same refusal with a 400.
fn send_thread(args: &SendArgs) -> Result<Option<i64>> {
    match args.thread {
        Some(_) if args.room.is_none() => {
            anyhow::bail!("--thread rides a room send: a thread is a room's discussion")
        }
        thread => Ok(thread),
    }
}

/// The members a `lesche send` names: a participant id names a member of a
/// room, so the flags are refused at either non-room target -- the same
/// refusal the tagma answers with 400, one hop earlier. Nothing is judged
/// about the ids here: their shape, the cap and the roster all live where
/// the room does, so a bad id costs nothing but the hop.
fn send_mentions(args: &SendArgs) -> Result<Vec<String>> {
    if args.room.is_none() && !args.mention.is_empty() {
        anyhow::bail!("--mention rides a room send: a participant id names a member of that room")
    }
    Ok(args.mention.clone())
}

/// One presentation line per addressable surface, shaped so the ids can be
/// copy-pasted straight into `send --room` / `send --tagma` / `read --tagma`.
fn render_session_line(entry: &LescheSessionEntry) -> String {
    match entry.kind.as_str() {
        "room" => match &entry.name {
            Some(name) => format!("room {} ({name})", entry.id),
            None => format!("room {}", entry.id),
        },
        "direct" => format!(
            "direct {} peer={} \"{}\"",
            entry.id,
            entry.peer_tagma.as_deref().unwrap_or(""),
            entry.peer_handle.as_deref().unwrap_or(""),
        ),
        kind => format!("{kind} {}", entry.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::lesche::ThreadArgs;
    use crate::args::{Cli, Commands};
    use clap::Parser;
    use kallipai_lesche_common::rooms::ThreadDelivery;

    /// The room and session the arg tests address. Both are canonical UUIDs,
    /// because a `read` target rides a request path and the CLI refuses any
    /// other spelling of one.
    const ROOM: &str = "11111111-1111-4111-8111-111111111111";
    const SESSION: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn a_send_names_one_target() {
        let Err(err) = Cli::try_parse_from([
            "kallip",
            "lesche",
            "send",
            "--room",
            ROOM,
            "--tagma",
            "tagma-peer",
        ]) else {
            panic!("conflicting targets accepted")
        };
        assert!(err.to_string().contains("cannot be used with"), "{err}");
    }

    /// A read target has to be the id the lesche minted: the value rides a
    /// request path, so any other spelling is refused before a request exists.
    #[test]
    fn a_read_target_must_be_a_canonical_uuid() {
        for argv in [
            vec!["kallip", "lesche", "read", "--room", "../../sessions/abc"],
            vec!["kallip", "lesche", "read", "--session", "../rooms/abc"],
        ] {
            let err = match Cli::try_parse_from(&argv) {
                Ok(_) => panic!("a non-id target parsed: {argv:?}"),
                Err(err) => err,
            };
            assert!(err.to_string().contains("canonical UUID"), "{err}");
        }
    }

    #[test]
    fn a_read_needs_a_target() {
        let Err(err) = Cli::try_parse_from(["kallip", "lesche", "read"]) else {
            panic!("a read without a target accepted")
        };
        assert!(err.to_string().contains("required arguments"), "{err}");
    }

    #[test]
    fn a_read_names_one_target() {
        let Err(err) = Cli::try_parse_from([
            "kallip",
            "lesche",
            "read",
            "--room",
            ROOM,
            "--tagma",
            "tagma-peer",
        ]) else {
            panic!("conflicting targets accepted")
        };
        assert!(err.to_string().contains("cannot be used with"), "{err}");
    }

    #[test]
    fn render_session_line_covers_bilateral_room_and_direct_shapes() {
        let bilateral = LescheSessionEntry {
            kind: "bilateral".into(),
            id: "conv-1".into(),
            name: None,
            peer_tagma: None,
            peer_handle: None,
        };
        assert_eq!(render_session_line(&bilateral), "bilateral conv-1");

        let room = LescheSessionEntry {
            kind: "room".into(),
            id: "room-1".into(),
            name: Some("ops".into()),
            peer_tagma: None,
            peer_handle: None,
        };
        assert_eq!(render_session_line(&room), "room room-1 (ops)");

        let direct = LescheSessionEntry {
            kind: "direct".into(),
            id: "sess-1".into(),
            name: None,
            peer_tagma: Some("tagma-peer".into()),
            peer_handle: Some("Peer".into()),
        };
        assert_eq!(
            render_session_line(&direct),
            "direct sess-1 peer=tagma-peer \"Peer\""
        );
    }

    /// Parse a `lesche read` invocation, so a test exercises the flags and the
    /// dispatch input together.
    fn parsed_read(extra: &[&str]) -> ReadArgs {
        let mut argv = vec!["kallip", "lesche", "read"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("parses");
        match cli.command.expect("command") {
            Commands::Lesche(LescheCommand::Read(args)) => args,
            _ => panic!("expected lesche read"),
        }
    }

    /// Parse a rendered read-back command, the way an agent pastes it: the
    /// tokens after `kallip lesche read` are the flags, and the parser is the
    /// one the CLI runs. A reference whose command the parser rejects is a
    /// command nobody can run, which is what this catches.
    fn parse_read_command(command: &str) -> ReadArgs {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        assert_eq!(
            &tokens[..3],
            ["kallip", "lesche", "read"],
            "a read-back command opens with the subcommand it runs: {command}"
        );
        parsed_read(&tokens[3..])
    }

    /// The three faces' read-back commands are accepted by this CLI, and each
    /// one lands on the log it names.
    #[test]
    fn every_rendered_read_back_command_runs() {
        use kallipai_common::readback::QuoteLog;

        let room = parse_read_command(
            &QuoteLog::Room("9f3a1c2e-8b7d-4f6a-9c1e-2d3b4a5c6d7e").read_command(7),
        );
        assert_eq!(
            room.room.as_deref(),
            Some("9f3a1c2e-8b7d-4f6a-9c1e-2d3b4a5c6d7e")
        );
        assert_eq!(room.quote, Some(7));
        assert_eq!(room.session, None);

        let direct = parse_read_command(
            &QuoteLog::Direct("c4b2a8f0-1d3e-4a5b-8c9d-0e1f2a3b4c5d").read_command(7),
        );
        assert_eq!(
            direct.session.as_deref(),
            Some("c4b2a8f0-1d3e-4a5b-8c9d-0e1f2a3b4c5d")
        );
        assert_eq!(direct.quote, Some(7));
        assert_eq!(direct.room, None);
        assert_eq!(direct.tagma, None);

        let bilateral = parse_read_command(&QuoteLog::Bilateral.read_command(42));
        assert_eq!(bilateral.quote, Some(42));
        assert_eq!(bilateral.room, None);
        assert_eq!(bilateral.tagma, None);
        assert_eq!(bilateral.session, None);
    }

    /// A bare `--quote` is the bilateral 1:1 read: no target flag, one row.
    #[test]
    fn a_bare_quote_names_the_bilateral_row() {
        let args = parsed_read(&["--quote", "9"]);
        assert_eq!(args.quote, Some(9));
        assert_eq!(args.room, None);
        assert_eq!(args.tagma, None);
        assert_eq!(args.session, None);
    }

    /// A session-addressed read names that session's log, then asks it for a
    /// page or for one row -- the two shapes a room read has, and never beside
    /// another target.
    #[test]
    fn a_session_read_names_its_log_and_asks_for_a_page_or_a_row() {
        let row = parsed_read(&["--session", SESSION, "--quote", "7"]);
        assert_eq!(row.session.as_deref(), Some(SESSION));
        assert_eq!(row.quote, Some(7));
        let query = direct_read_query(&row);
        assert_eq!(
            query.after_seq,
            Some(6),
            "the quoted row, reached from below"
        );
        assert_eq!(query.limit, Some(1));

        let page = parsed_read(&["--session", SESSION, "--after", "3"]);
        assert_eq!(page.session.as_deref(), Some(SESSION));
        assert_eq!(page.quote, None);
        let query = direct_read_query(&page);
        assert_eq!(
            query.after_seq,
            Some(3),
            "a page read passes the caller's cursor through"
        );
        assert_eq!(query.limit, None);

        assert!(
            Cli::try_parse_from([
                "kallip",
                "lesche",
                "read",
                "--session",
                SESSION,
                "--quote",
                "7",
                "--room",
                ROOM
            ])
            .is_err(),
            "a read names one log"
        );
        assert!(
            Cli::try_parse_from([
                "kallip",
                "lesche",
                "read",
                "--session",
                SESSION,
                "--quote",
                "7",
                "--tagma",
                "peer"
            ])
            .is_err(),
            "a read names one log"
        );
    }

    /// Parse a `lesche send` invocation the same way.
    fn parsed_send(extra: &[&str]) -> SendArgs {
        let mut argv = vec!["kallip", "lesche", "send"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("parses");
        match cli.command.expect("command") {
            Commands::Lesche(LescheCommand::Send(args)) => args,
            _ => panic!("expected lesche send"),
        }
    }

    #[test]
    fn each_position_flag_lands_in_its_own_field() {
        let backward = parsed_read(&["--room", ROOM, "--before", "5"]);
        assert_eq!(backward.before_seq, Some(5));
        assert_eq!(backward.quote, None);
        let quoted = parsed_read(&["--room", ROOM, "--quote", "7"]);
        assert_eq!(quoted.quote, Some(7));
        assert_eq!(quoted.before_seq, None);
        let forward = parsed_read(&["--room", ROOM, "--after", "3"]);
        assert_eq!(forward.after_seq, Some(3));
        assert_eq!(forward.before_seq, None);
    }

    #[test]
    fn two_cursors_at_once_are_refused() {
        for extra in [
            ["--room", ROOM, "--after", "1", "--before", "5"],
            ["--room", ROOM, "--after", "1", "--quote", "7"],
            ["--room", ROOM, "--before", "5", "--quote", "7"],
            ["--room", ROOM, "--quote", "7", "--limit", "3"],
        ] {
            let mut argv = vec!["kallip", "lesche", "read"];
            argv.extend_from_slice(&extra);
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "two cursors accepted: {extra:?}"
            );
        }
    }

    #[test]
    fn a_read_position_starts_at_one() {
        for extra in [
            ["--room", ROOM, "--before", "0"],
            ["--room", ROOM, "--before", "-3"],
            ["--room", ROOM, "--quote", "0"],
        ] {
            let mut argv = vec!["kallip", "lesche", "read"];
            argv.extend_from_slice(&extra);
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "non-positive position accepted: {extra:?}"
            );
        }
    }

    /// A `--before` cursor needs a page edge to name, and only a room read has
    /// one: a direct session is read forward, a bare direct read carries no
    /// position at all, and a `--quote` names a row rather than an edge, so it
    /// rides either target.
    #[test]
    fn a_backward_cursor_is_refused_only_beside_a_direct_session() {
        let err =
            reject_backward_cursor_at_non_room(&parsed_read(&["--tagma", "peer", "--before", "5"]))
                .expect_err("non-room target");
        assert!(
            err.to_string()
                .contains("--before rides a room read: a seq names a row"),
            "{err}"
        );
        assert!(
            reject_backward_cursor_at_non_room(&parsed_read(&["--tagma", "peer"])).is_ok(),
            "a plain direct read carries no position"
        );
        assert!(
            reject_backward_cursor_at_non_room(&parsed_read(&["--room", ROOM, "--before", "5"]))
                .is_ok(),
            "a room keeps its backward cursor"
        );
        let quoted = parsed_read(&["--tagma", "peer", "--quote", "7"]);
        assert!(
            reject_backward_cursor_at_non_room(&quoted).is_ok(),
            "a quote rides a direct read: it names a row, not a page edge"
        );
    }

    #[test]
    fn a_quote_lookup_asks_for_the_row_above_the_position() {
        let query = room_read_query(&parsed_read(&["--room", ROOM, "--quote", "42"]))
            .expect("quote composes");
        assert_eq!(query.after_seq, None);
        assert_eq!(query.before_seq, Some(43));
        assert_eq!(query.limit, Some(1));
    }

    #[test]
    fn a_direct_quote_lookup_asks_for_the_row_below_the_position() {
        let quoted = parsed_read(&["--tagma", "peer", "--quote", "42"]);
        let query = direct_read_query(&quoted);
        assert_eq!(query.after_seq, Some(41));
        assert_eq!(query.limit, Some(1));
        // The first row of a session sits at the floor: `after_seq = 0` reads
        // from the start, while a room's backward cursor treats 0 as no row.
        let floor = parsed_read(&["--tagma", "peer", "--quote", "1"]);
        let first = direct_read_query(&floor);
        assert_eq!(first.after_seq, Some(0));
        assert_eq!(first.limit, Some(1));
        // Without a quote the caller's own cursor and page size pass through.
        let plain = direct_read_query(&parsed_read(&[
            "--tagma", "peer", "--after", "3", "--limit", "20",
        ]));
        assert_eq!(plain.after_seq, Some(3));
        assert_eq!(plain.limit, Some(20));
    }

    #[test]
    fn a_position_with_no_room_above_it_is_rejected() {
        let args = ReadArgs {
            room: Some("ops".to_owned()),
            tagma: None,
            session: None,
            after_seq: None,
            before_seq: None,
            quote: Some(i64::MAX),
            limit: None,
        };
        let err = room_read_query(&args).expect_err("out of range");
        assert!(err.to_string().contains("is out of range"), "{err}");
    }

    #[test]
    fn a_backward_page_passes_its_cursor_through() {
        let query = room_read_query(&parsed_read(&["--room", ROOM, "--before", "9"]))
            .expect("backward page");
        assert_eq!(query.before_seq, Some(9));
        assert_eq!(query.after_seq, None);
        assert_eq!(query.limit, None);
        // The shape a pager actually sends: the cursor plus a page size.
        let sized = room_read_query(&parsed_read(&[
            "--room", ROOM, "--before", "9", "--limit", "20",
        ]))
        .expect("sized backward page");
        assert_eq!(sized.before_seq, Some(9));
        assert_eq!(sized.limit, Some(20));
        let plain = room_read_query(&parsed_read(&["--room", ROOM])).expect("plain page");
        assert_eq!(plain.after_seq, None);
        assert_eq!(plain.before_seq, None);
        assert_eq!(plain.limit, None);
    }

    #[test]
    fn a_quoted_read_prints_the_named_row_and_reports_its_absence() {
        let block = "[seq=42 from=agent:00000000-0000-4000-8000-0000000000a1 \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n";
        assert_eq!(
            expect_named_block(block.to_owned(), 42).expect("named row"),
            block
        );
        // The leading block alone decides the row: a page whose first block
        // names a longer seq must fail, and so must one whose body merely
        // carries the named seq.
        for (text, seq) in [
            (String::new(), 42),
            (
                "[seq=41 from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n".to_owned(),
                42,
            ),
            ("not a block".to_owned(), 42),
            (
                "[seq=42 from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n".to_owned(),
                4,
            ),
            (
                "[seq=41 from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nlook:\n[seq=42 from=agent:00000000-0000-4000-8000-0000000000b2 at=2026-01-01 0:00:00.0 +00:00:00]\n\n".to_owned(),
                42,
            ),
        ] {
            let err = expect_named_block(text.clone(), seq).expect_err("unnamed row");
            assert!(
                err.to_string()
                    .contains(&format!("no readable row at seq {seq}")),
                "{err}"
            );
        }
    }

    /// The check a `--quote` read runs takes its opening from the renderer's
    /// own source: a header built with it names the row the read asked for, and
    /// the row beside it -- the page a missing position lands on -- does not.
    #[test]
    fn a_rendered_header_names_the_row_the_read_asked_for() {
        let named = format!(
            "{}from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n",
            kallipai_common::readback::block_prefix(42)
        );
        assert_eq!(
            expect_named_block(named.clone(), 42).expect("named row"),
            named
        );
        let beside = format!(
            "{}from=agent:00000000-0000-4000-8000-0000000000a1 at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n",
            kallipai_common::readback::block_prefix(41)
        );
        assert!(
            expect_named_block(beside, 42).is_err(),
            "the row beside the position is another row"
        );
    }

    #[test]
    fn a_quoted_read_prints_a_row_whose_body_looks_like_a_block() {
        let pasted = "[seq=99 from=agent:00000000-0000-4000-8000-0000000000b2 \"Bob\" at=2026-01-01 0:00:00.0 +00:00:00]\nhi\n\n";
        let block = format!(
            "[seq=42 from=agent:00000000-0000-4000-8000-0000000000a1 \"Alice\" at=2026-01-01 0:00:00.0 +00:00:00]\nlook:\n{pasted}"
        );
        assert_eq!(
            expect_named_block(block.clone(), 42).expect("a body quoting a block"),
            block
        );
    }

    #[test]
    fn a_thread_rides_a_room_send_only() {
        assert_eq!(
            send_thread(&parsed_send(&["--room", ROOM, "--thread", "3"])).expect("room send"),
            Some(3)
        );
        assert_eq!(
            send_thread(&parsed_send(&["--room", ROOM])).expect("room send"),
            None
        );
        // A thread is a room's discussion: no room, no thread.
        for flags in [
            vec!["--thread", "3"],
            vec!["--tagma", "peer", "--thread", "3"],
        ] {
            let err = send_thread(&parsed_send(&flags)).expect_err("no room to join a thread in");
            assert!(
                err.to_string().contains("--thread rides a room send"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_quote_rides_a_room_or_direct_send() {
        assert_eq!(
            send_quote(&parsed_send(&["--room", ROOM, "--quote", "7"])).expect("room send"),
            Some(7)
        );
        assert_eq!(
            send_quote(&parsed_send(&["--tagma", "peer", "--quote", "7"]))
                .expect("direct-session send"),
            Some(7)
        );
        assert_eq!(send_quote(&parsed_send(&[])).expect("bilateral send"), None);
        let err = send_quote(&parsed_send(&["--quote", "7"])).expect_err("bilateral send");
        assert!(
            err.to_string().contains("rides a room or direct send"),
            "{err}"
        );
    }

    #[test]
    fn mentions_ride_a_room_send_only() {
        let sent = send_mentions(&parsed_send(&[
            "--room",
            "ops",
            "--mention",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "--mention",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        ]))
        .expect("room send");
        assert_eq!(
            sent,
            vec![
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string(),
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string(),
            ],
            "the flags collect in the order they were given"
        );

        assert!(
            send_mentions(&parsed_send(&[]))
                .expect("plain send")
                .is_empty(),
            "a send that names nobody names nobody"
        );

        let err = send_mentions(&parsed_send(&["--tagma", "peer", "--mention", "not-an-id"]))
            .expect_err("a direct-session send names no room");
        assert!(err.to_string().contains("rides a room send"), "{err}");
    }

    #[test]
    fn a_mention_and_a_quote_travel_together() {
        let args = parsed_send(&[
            "--room",
            "ops",
            "--quote",
            "7",
            "--mention",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        ]);
        assert_eq!(send_quote(&args).expect("room send"), Some(7));
        assert_eq!(
            send_mentions(&args).expect("room send"),
            vec!["aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string()]
        );
    }

    #[test]
    fn a_send_position_starts_at_one() {
        assert!(
            Cli::try_parse_from(["kallip", "lesche", "send", "--room", ROOM, "--quote", "0"])
                .is_err()
        );
    }

    /// Parse a `lesche thread` invocation the same way.
    fn parsed_thread(extra: &[&str]) -> ThreadArgs {
        let mut argv = vec!["kallip", "lesche", "thread"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("parses");
        match cli.command.expect("command") {
            Commands::Lesche(LescheCommand::Thread(args)) => args,
            _ => panic!("expected lesche thread"),
        }
    }

    #[test]
    fn a_thread_verb_names_a_room_and_a_head_position() {
        let ThreadCommand::Mute(target) = parsed_thread(&["mute", ROOM, "9"]).command else {
            panic!("expected the mute verb");
        };
        assert_eq!(target.room, ROOM);
        assert_eq!(target.root, 9);

        // The room id rides a request path, so a non-canonical spelling is
        // refused before a request exists, like every other room target.
        let Err(err) =
            Cli::try_parse_from(["kallip", "lesche", "thread", "follow", "../../agents", "9"])
        else {
            panic!("a room target must be a canonical id")
        };
        assert!(err.to_string().contains("room id"), "{err}");

        // A position starts at one.
        let Err(err) = Cli::try_parse_from(["kallip", "lesche", "thread", "follow", ROOM, "0"])
        else {
            panic!("0 names no row")
        };
        assert!(err.to_string().contains("not in 1.."), "{err}");
    }

    #[test]
    fn a_thread_choice_prints_the_rule_and_its_boundary() {
        let view = ThreadSubscriptionView {
            choice: Some(ThreadChoice::Muted),
            effective: ThreadDelivery::Muted,
            since_seq: Some(17),
        };
        assert_eq!(
            render_thread_choice(ROOM, 9, &view),
            format!("thread 9 in {ROOM}: muted, after seq 17")
        );
    }
}
