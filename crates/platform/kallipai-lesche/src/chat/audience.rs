//! Who a room message wakes: the thread-scoped audience.
//!
//! A room message is delivered on the thread it belongs to. This module is the
//! ONE place that DEFINES who that is: the fan
//! ([`crate::chat::fan::fan_envelope_to`]) only carries the answer out, and the
//! route logs the answer it passed on.
//!
//! That rule has a second implementation and no type ties the two together:
//! `store::unread_for_member` restates it in SQL to count a member's badge,
//! because a badge answers what reached me, and a row that never reached me
//! cannot count. Changing one side means changing the other, and the only
//! thing that will say so is a cross-module test --
//! `the_badge_moves_exactly_for_the_members_the_audience_named` in the room
//! tests. A term added on one side alone is the kind of drift it catches.
//!
//! # The rule
//!
//! - a row that heads its own thread (`root` is `None`: every message with no
//!   declared thread, and every row stored before the column existed) wakes the
//!   whole room minus the sender: a head opens a new topic, so nobody is a
//!   participant yet and the room is the only audience there is; narrowing
//!   takes a reply;
//! - a reply inside a thread wakes, minus the sender, the members the thread
//!   REACHES: an agent it recorded (`participants`) or one that chose to
//!   follow it, and every human member that has not muted it. A human is
//!   woken by a thread it never took part in on purpose -- a message a human
//!   never heard about is a hole that does not heal -- while an agent re-pulls
//!   history on reconnect, so a narrow wake costs it nothing but a re-read;
//!   a thread only humans replied in records no participant row, so its replies
//!   reach the humans and any follower, not the room's agents;
//! - a recorded CHOICE outranks both defaults: `following` wakes its member on
//!   every reply after the choice, and `muted` on none of them except a message
//!   that names them (see the mention bullet). The choice is judged at the
//!   message's own position, never at now, so a mute cannot take back a row the
//!   badge already counted -- see [`choice`];
//! - a mention pierces a mute: a member who withdrew from a thread did not
//!   withdraw from being addressed, so a message that names a muted member wakes
//!   them anyway ([`Audience::mentioned`]). Mentions are not parsed here: the
//!   sender's list arrives from the route, the same list the store records as
//!   participant rows, so one source decides both;
//!
//! # The rule for readers (do not skip this)
//!
//! Any database read that feeds this computation must never fail a send. A send
//! is a durable fact plus a notification; the notification's audience is a
//! delivery optimisation, so a store hiccup must never reject a message.
//! Both tables widen on a failed read, and the same rule gives the same answer
//! on each: a wake costs an agent a re-pull, while a message it was never
//! woken for is lost to it, so an extra wake is the error to prefer. A failed
//! choice read widens to the whole room ([`Fallback::NoSubscriptions`]); a
//! failed participant read counts every agent as recorded in the thread
//! ([`Fallback::UnreadableParticipants`]), which comes to the whole room minus
//! whatever mute the choice read did return. Each names its own table at WARN,
//! because that one line is all a later reader has to go on.
//!
//! An empty participant set is a different answer: a thread only humans replied
//! in records no row, so its replies reach the humans and the followers while
//! the room's agents stay out ([`Fallback::NoParticipants`]).
//!
//! A read that SUCCEEDS but is early is subtler, and the two tables behave in
//! opposite directions. `store::append` reads after its own commit, so it can
//! miss a row that had not committed yet. A missed PARTICIPANT row only
//! removes members from the thread's set: that direction narrows. A missed
//! CHOICE row can widen, because its member then falls through to the default,
//! and the default wakes them -- a human above all. That widening is allowed
//! for the reason the failure path is: the widened side is a DEFAULT, so no
//! choice the read could see was overridden, and the cost is a re-pull, not a
//! lost row. Nothing here wakes a member past a choice that was read.
//!
//! Outside that window there is one more widening, by design and earlier in
//! the send: a declared thread that resolves to no row lands the message as a
//! head (`store::resolve_thread_root`), and a head wakes the room.
//!
//! # What is not here
//!
//! The read path. Thread scope is a notification scope, not access control:
//! `room_history` and `kallip lesche read --room` return every row of the room,
//! including threads the reader never took part in. Reading is an explicit act;
//! waking is a system act, and the two are deliberately not aligned.

use std::collections::BTreeSet;

use kallipai_archeion_common::ids::ParticipantKind;
use kallipai_lesche_common::rooms::{MemberId, RoomMembership, ThreadChoice};

use crate::chat::db::store::ThreadSubscription;

/// Why a reply's scope was not what its thread decided.
///
/// A head message is the normal whole-room case and consults neither table, so
/// it contributes no entry. The rest name a table that did not answer: an empty
/// participant set on a reply (a version window the backfill missed, or simply
/// a thread only humans replied in), a participant read that failed, and a
/// choice read that failed.
///
/// A reply carries one entry PER TABLE, because the two failures happen
/// together and each is its own fact: an empty participant table is still
/// empty when a member's own choice silenced them, and a failed choice read is
/// still failed when the participant read failed too. Collapsing them into one
/// answer would let whichever was judged first hide the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The reply's thread recorded no participants: every agent falls to its
    /// default. Says nothing about the members' own choices, which still apply.
    NoParticipants,
    /// The reply's thread has rows, but its choices could not be read.
    NoSubscriptions,
    /// The reply's thread has rows, but its participants could not be read;
    /// every agent is treated as recorded in it.
    UnreadableParticipants,
}

/// The audience of one message, and how it was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audience {
    /// Who to wake, sender excluded, no id outside the current membership.
    pub ids: BTreeSet<MemberId>,
    /// Every way the thread's tables failed to scope this reply, one entry per
    /// table (`[]` for a head, which consults neither). The wake may still be
    /// narrow: a member's own choice applies under either failure.
    pub fallback: Vec<Fallback>,
    /// Members the thread's choices silenced. `0` where the room was woken
    /// without reading a choice (a head, or an unreadable choice table); a reply
    /// whose thread has no participants still counts a mute here, and then
    /// reports no fallback. Logged so the route can say how much narrower than
    /// the room's default a wake came out, and why.
    pub muted: usize,
    /// Members a mention woke past their own mute. Counted apart from
    /// `following`, which is a choice: a mention is the sender's act, not
    /// the member's. Logged for the same reason as the other two.
    pub mentioned: usize,
    /// Members a `following` choice woke, rather than the default. Same
    /// logging purpose as `muted`.
    pub following: usize,
}

/// Decide who a message wakes.
///
/// `participants` is the thread's recorded agent roster, `overrides` its
/// recorded choices, and `mentions` the members THIS message names -- the
/// first two taken from [`crate::chat::db::store::Appended`], the third from
/// the payload the route just stored. The first two arrive as raw rows, not
/// sets: the position a choice starts at is part of the decision, and
/// collapsing the rows into a following set and a muted set would drop it.
/// The membership is the CURRENT room roster: rows outlive members leaving,
/// so the intersection happens here rather than in a cleanup job.
///
/// `participants` and `overrides` each keep a failed read (`None`) apart from
/// a successful one that found nothing (`Some`, empty or not): empty is an
/// ordinary answer on both tables, and the two decide differently, so one
/// representation cannot hold both.
///
/// `mentions` is the one input no choice can veto, and it holds only the
/// message being decided, never the thread: a mute is a decision about a
/// discussion, a mention is somebody addressing this member in it. A head
/// consults neither table -- it defines the thread, so `seq` describes
/// nothing there yet.
///
/// A reply judges each member on its own -- a recorded choice first, then the
/// default that choice replaces -- so an empty participant set cannot take
/// the decision away from a member who muted the thread, and a failed
/// participant read widens only the members no choice reached. `fallback` is
/// computed after the loop and names every table that could not answer.
pub fn audience(
    membership: &RoomMembership,
    sender: &MemberId,
    root: Option<i64>,
    seq: i64,
    participants: Option<&[MemberId]>,
    overrides: Option<&[ThreadSubscription]>,
    mentions: &[MemberId],
) -> Audience {
    // The tier term of the scope decision: tiers land after this narrowing, so
    // today every agent is in the tier that hears its threads. Kept as a named
    // `true` rather than omitted so the day tiers land this is where the term
    // goes -- and `tier_term_is_constant_until_tiers_land` fails if it silently
    // stops being one.
    let in_tier = |_id: &MemberId| true;

    // A head defines its own thread, so neither table has anything to say, and
    // a reply whose choices could not be read widens to the room with a reason.
    // Every other reply judges each member on its own: a recorded choice first,
    // then the default that choice replaces. An empty participant set must not
    // take that decision away from a member who muted the thread.
    let head = root.is_none();
    let choices_readable = overrides.is_some();
    let rows = overrides.unwrap_or(&[]);

    let mut ids = BTreeSet::new();
    let mut muted = 0usize;
    let mut following = 0usize;
    let mut mentioned = 0usize;
    for member in &membership.members {
        if member.id == *sender {
            continue;
        }
        let woken = if head || !choices_readable {
            // Whole room: no thread to narrow by, or no choice to narrow with.
            true
        } else {
            match choice(rows, &member.id, seq) {
                Some(ThreadChoice::Muted) => {
                    // A mention pierces a mute: the member withdrew from the
                    // thread, not from being addressed by name.
                    if mentions.contains(&member.id) {
                        mentioned += 1;
                        in_tier(&member.id)
                    } else {
                        muted += 1;
                        false
                    }
                }
                Some(ThreadChoice::Following) => {
                    following += 1;
                    in_tier(&member.id)
                }
                // No choice reaches this row: use the default it replaces.
                None => match member.kind {
                    ParticipantKind::Human => true,
                    ParticipantKind::Agent => {
                        // A failed read counts every agent as recorded: a
                        // missed wake loses the message for that agent, an
                        // extra one costs it a re-pull.
                        let recorded = participants.is_none_or(|rows| rows.contains(&member.id));
                        recorded && in_tier(&member.id)
                    }
                },
            }
        };
        if woken {
            ids.insert(member.id.clone());
        }
    }

    // What the tables could not answer, one entry per table. Each table's
    // answer stands on its own: neither is inferred from how the wake came out
    // (a counted mute says nothing about the other table), and an empty
    // participant set is the ordinary shape a thread of humans gives, so it is
    // named whether or not somebody also recorded a choice.
    let mut fallback = Vec::new();
    if !head {
        if participants.is_none() {
            fallback.push(Fallback::UnreadableParticipants);
        } else if matches!(participants, Some(rows) if rows.is_empty()) {
            fallback.push(Fallback::NoParticipants);
        }
        if !choices_readable {
            fallback.push(Fallback::NoSubscriptions);
        }
    }

    Audience {
        ids,
        fallback,
        muted,
        following,
        mentioned,
    }
}

/// The choice a member recorded about the thread that holds `seq`, judged at
/// that position. `None` is the default, and the default is not a choice.
///
/// `since_seq` is where the row's state starts. `muted_from_seq` outlives a
/// re-follow on purpose: a member who muted at 12 and re-followed at 20 is on
/// the default at 25 and still muted at 15, so a row in the gap between the
/// two is judged by the earlier mute's bound rather than by the state that
/// superseded it. `store::unread_for_member` restates this in SQL; the two
/// must agree row for row.
pub fn choice(rows: &[ThreadSubscription], member: &MemberId, seq: i64) -> Option<ThreadChoice> {
    let row = rows.iter().find(|row| row.member == *member)?;
    if seq > row.since_seq {
        return Some(row.state);
    }
    row.muted_from_seq
        .filter(|from| seq > *from)
        .map(|_| ThreadChoice::Muted)
}

#[cfg(test)]
mod tests {
    //! The predicate in numbers: who a head wakes, who a reply wakes, what a
    //! member's choice does to either, and the ways the whole room comes back.
    //!
    //! Each test is one line of the rule in the module docs; the store-side
    //! tests own the rows, these own the decision.

    use super::*;
    use kallipai_lesche_common::rooms::RoomMember;

    fn mid(tag: &str) -> MemberId {
        MemberId::from(tag.to_string())
    }

    fn room(members: &[(&str, ParticipantKind)]) -> RoomMembership {
        RoomMembership {
            members: members
                .iter()
                .map(|(tag, kind)| RoomMember {
                    id: mid(tag),
                    kind: *kind,
                })
                .collect(),
            membership_epoch: 1,
        }
    }

    /// A choice row as `store` hands it over: `since_seq` is where the state
    /// starts and `muted_from_seq` the mute bound a later re-follow keeps.
    fn kept(
        member: &str,
        state: ThreadChoice,
        since_seq: i64,
        muted_from_seq: Option<i64>,
    ) -> ThreadSubscription {
        ThreadSubscription {
            member: mid(member),
            state,
            since_seq,
            muted_from_seq,
        }
    }

    fn muted_at(member: &str, seq: i64) -> ThreadSubscription {
        kept(member, ThreadChoice::Muted, seq, Some(seq))
    }

    fn following_from(member: &str, seq: i64) -> ThreadSubscription {
        kept(member, ThreadChoice::Following, seq, None)
    }

    /// A heads a thread, B replied in it, C never took part, h is a human who
    /// never took part. A reply by A wakes B and h, and leaves C alone.
    #[test]
    fn a_reply_wakes_the_thread_and_every_human_not_the_bystander() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("c", ParticipantKind::Agent),
            ("h", ParticipantKind::Human),
        ]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(1),
            2,
            Some(&[mid("a"), mid("b")]),
            Some(&[]),
            &[],
        );
        assert_eq!(got.ids, BTreeSet::from([mid("b"), mid("h")]));
        assert!(got.fallback.is_empty());
        assert_eq!((got.muted, got.following), (0, 0));
    }

    /// A head consults neither table: everyone but the sender, and it stays
    /// that way even when the choice read failed.
    #[test]
    fn a_head_wakes_the_whole_room_and_ignores_both_tables() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("h", ParticipantKind::Human),
        ]);
        let got = audience(&membership, &mid("a"), None, 1, None, None, &[]);
        assert_eq!(got.ids, BTreeSet::from([mid("b"), mid("h")]));
        assert!(
            got.fallback.is_empty(),
            "a head consults neither table, so there is nothing to name"
        );
        assert_eq!((got.muted, got.following), (0, 0));
    }

    /// A reply whose thread has no rows is still named at WARN, and the wake
    /// falls to each member's default: the agent is not recorded there, so it is
    /// not woken.
    #[test]
    fn a_reply_without_participants_names_the_hole_and_keeps_the_default() {
        let membership = room(&[("a", ParticipantKind::Agent), ("b", ParticipantKind::Agent)]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(7),
            8,
            Some(&[]),
            Some(&[]),
            &[],
        );
        assert!(got.ids.is_empty());
        assert_eq!(got.fallback, vec![Fallback::NoParticipants]);
        assert_eq!((got.muted, got.following), (0, 0));
    }

    /// A thread only humans replied in writes no participant row, and that must
    /// not cost its members their choices: one human mutes it and stays out, the
    /// other human keeps the default and is woken. The empty table is still
    /// named -- a counted mute says nothing about the other table's answer.
    #[test]
    fn a_mute_inside_a_thread_of_humans_still_holds() {
        let membership = room(&[
            ("a", ParticipantKind::Human),
            ("h", ParticipantKind::Human),
            ("k", ParticipantKind::Human),
        ]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(7),
            8,
            Some(&[]),
            Some(&[muted_at("h", 7)]),
            &[],
        );
        assert_eq!(got.ids, BTreeSet::from([mid("k")]));
        assert_eq!(
            got.fallback,
            vec![Fallback::NoParticipants],
            "an empty participant table is still the table's own answer"
        );
        assert_eq!((got.muted, got.following), (1, 0));
    }

    /// The same fallback on the other table: a reply whose choices could not
    /// be read wakes the room, and the reason names the right read.
    #[test]
    fn a_reply_without_choices_falls_back_to_the_whole_room() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("c", ParticipantKind::Agent),
        ]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(1),
            2,
            Some(&[mid("a"), mid("b")]),
            None,
            &[],
        );
        assert_eq!(got.ids, BTreeSet::from([mid("b"), mid("c")]));
        assert_eq!(got.fallback, vec![Fallback::NoSubscriptions]);
        assert_eq!((got.muted, got.following), (0, 0));
    }

    /// The sender is out even when it is a participant of its own thread, and a
    /// participant who left the room is not woken.
    #[test]
    fn the_sender_and_departed_participants_are_not_woken() {
        let membership = room(&[("a", ParticipantKind::Agent), ("b", ParticipantKind::Human)]);
        let got = audience(
            &membership,
            &mid("b"),
            Some(3),
            4,
            Some(&[mid("a"), mid("b"), mid("gone")]),
            Some(&[]),
            &[],
        );
        assert_eq!(got.ids, BTreeSet::from([mid("a")]));
    }

    /// Until delivery tiers land the tier term is `true`; this fails the day it
    /// stops being one, which is the day the docs above must change too.
    #[test]
    fn tier_term_is_constant_until_tiers_land() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("c", ParticipantKind::Agent),
        ]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(1),
            2,
            Some(&[mid("b"), mid("c")]),
            Some(&[]),
            &[],
        );
        assert_eq!(got.ids, BTreeSet::from([mid("b"), mid("c")]));
    }

    /// A muted agent is left out even when the thread recorded it, and the
    /// audience says how many members a choice silenced.
    #[test]
    fn a_muted_agent_is_left_out_of_the_thread_it_joined() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("h", ParticipantKind::Human),
        ]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(1),
            2,
            Some(&[mid("a"), mid("b")]),
            Some(&[muted_at("b", 1)]),
            &[],
        );
        assert_eq!(got.ids, BTreeSet::from([mid("h")]));
        assert_eq!((got.muted, got.following), (1, 0));
    }

    /// A mute reaches humans too, which is what makes an empty scoped
    /// audience reachable: nobody to wake, and no fallback to hide behind.
    #[test]
    fn a_thread_every_member_muted_wakes_nobody() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("h", ParticipantKind::Human),
        ]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(1),
            2,
            Some(&[mid("a"), mid("b")]),
            Some(&[muted_at("b", 1), muted_at("h", 1)]),
            &[],
        );
        assert!(got.ids.is_empty());
        assert!(got.fallback.is_empty());
        assert_eq!((got.muted, got.following), (2, 0));
    }

    /// Following wakes an agent the thread never recorded, and the audience
    /// reports it as a choice rather than as the default.
    #[test]
    fn a_following_agent_outside_the_roster_is_woken() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("c", ParticipantKind::Agent),
        ]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(1),
            2,
            Some(&[mid("a"), mid("b")]),
            Some(&[following_from("c", 1)]),
            &[],
        );
        assert_eq!(got.ids, BTreeSet::from([mid("b"), mid("c")]));
        assert_eq!((got.muted, got.following), (0, 1));
    }

    /// A re-follow leaves the earlier mute bound in place: rows between the mute
    /// and the re-follow stay silent, and rows after the re-follow wake.
    #[test]
    fn a_mute_holds_inside_the_gap_a_re_follow_left() {
        let membership = room(&[("a", ParticipantKind::Agent), ("b", ParticipantKind::Agent)]);
        let rows = [kept("b", ThreadChoice::Following, 20, Some(12))];
        let participants = [mid("a"), mid("b")];
        let inside = audience(
            &membership,
            &mid("a"),
            Some(1),
            15,
            Some(&participants),
            Some(&rows),
            &[],
        );
        assert!(inside.ids.is_empty());
        assert_eq!((inside.muted, inside.following), (1, 0));
        let after = audience(
            &membership,
            &mid("a"),
            Some(1),
            25,
            Some(&participants),
            Some(&rows),
            &[],
        );
        assert_eq!(after.ids, BTreeSet::from([mid("b")]));
        assert_eq!((after.muted, after.following), (0, 1));
    }

    /// A choice starts at its own `since_seq`: a row from before it is on the
    /// default, and the boundary itself belongs to the default too.
    #[test]
    fn a_choice_does_not_reach_the_rows_before_it() {
        let membership = room(&[("a", ParticipantKind::Agent), ("b", ParticipantKind::Agent)]);
        let rows = [following_from("b", 10)];
        assert_eq!(choice(&rows, &mid("b"), 10), None);
        let before = audience(
            &membership,
            &mid("a"),
            Some(1),
            5,
            Some(&[mid("a")]),
            Some(&rows),
            &[],
        );
        assert!(before.ids.is_empty());
        let after = audience(
            &membership,
            &mid("a"),
            Some(1),
            15,
            Some(&[mid("a")]),
            Some(&rows),
            &[],
        );
        assert_eq!(after.ids, BTreeSet::from([mid("b")]));
    }

    /// A failed participant read widens like a failed choice read: without the
    /// table every agent counts as recorded in the thread, because a message an
    /// agent was never woken for is a message the thread lost for it. A mute
    /// the choice read did return still holds.
    #[test]
    fn a_failed_participant_read_wakes_every_agent() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("c", ParticipantKind::Agent),
        ]);
        let got = audience(&membership, &mid("a"), Some(1), 2, None, Some(&[]), &[]);
        assert_eq!(got.ids, BTreeSet::from([mid("b"), mid("c")]));
        assert_eq!(got.fallback, vec![Fallback::UnreadableParticipants]);
        let muted = audience(
            &membership,
            &mid("a"),
            Some(1),
            2,
            None,
            Some(&[muted_at("c", 1)]),
            &[],
        );
        assert_eq!(muted.ids, BTreeSet::from([mid("b")]));
        assert_eq!((muted.muted, muted.following), (1, 0));
    }

    /// A mention pierces a mute, and the audience counts it apart from the
    /// choices: the member withdrew from the thread, not from being addressed.
    #[test]
    fn a_mention_wakes_a_muted_member() {
        let membership = room(&[("a", ParticipantKind::Agent), ("b", ParticipantKind::Agent)]);
        let got = audience(
            &membership,
            &mid("a"),
            Some(1),
            2,
            None,
            Some(&[muted_at("b", 1)]),
            &[mid("b")],
        );
        assert_eq!(got.ids, BTreeSet::from([mid("b")]));
        assert_eq!((got.muted, got.mentioned, got.following), (0, 1, 0));
    }
    /// Both tables failing is two facts, not one: the audience carries an entry
    /// for each, so a log line cannot claim the table it did not name answered.
    #[test]
    fn a_reply_whose_two_reads_failed_names_both_tables() {
        let membership = room(&[("a", ParticipantKind::Agent), ("b", ParticipantKind::Agent)]);
        let got = audience(&membership, &mid("a"), Some(1), 2, None, None, &[]);
        assert_eq!(
            got.fallback,
            vec![Fallback::UnreadableParticipants, Fallback::NoSubscriptions]
        );
        assert_eq!(got.ids, BTreeSet::from([mid("b")]));
    }
}
