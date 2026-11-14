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
//! - a reply inside a thread wakes that thread's agents plus EVERY human member,
//!   minus the sender. Humans are woken regardless of participation on purpose:
//!   a human has no per-thread subscription, the app shows one room timeline,
//!   and a message they never heard about is a hole that does not heal. An agent
//!   re-pulls history on reconnect, so a narrow wake costs it nothing but a
//!   re-read;
//! - mentions are NOT read here. A mention inside a thread is recorded as a
//!   participant row in the same transaction that stores the message, so "the
//!   agent this message named is in the audience" already holds through the
//!   participant term. One source, one implementation.
//!
//! # The rule for readers (do not skip this)
//!
//! Any database read that feeds this computation must never fail a send. A send
//! is a durable fact plus a notification; the notification's audience is a
//! delivery optimisation, so a store hiccup must degrade to the WIDER audience
//! (the whole room), never to a rejected message. `store::append` already
//! applies that rule: a failed participant read warns and hands back an empty
//! set, which lands here as [`Fallback::NoParticipants`].
//!
//! A read that SUCCEEDS but is early is the other direction. `store::append`
//! reads after its own commit, so it can miss a reply that had not committed
//! yet, and a miss only removes members from the thread's set: the audience
//! narrows then and never widens. Inside this post-commit window, widening
//! belongs to the failure path alone. Outside it there is a second widening,
//! by design and earlier in the send: a declared thread that resolves to no
//! row lands the message as a head (`store::resolve_thread_root`), and a head
//! wakes the room.
//!
//! # What is not here
//!
//! The read path. Thread scope is a notification scope, not access control:
//! `room_history` and `kallip lesche read --room` return every row of the room,
//! including threads the reader never took part in. Reading is an explicit act;
//! waking is a system act, and the two are deliberately not aligned.

use std::collections::BTreeSet;

use kallipai_archeion_common::ids::ParticipantKind;
use kallipai_lesche_common::rooms::{MemberId, RoomMembership};

/// Why the whole room was chosen when a thread could have scoped the wake.
///
/// Only [`Fallback::NoParticipants`] is a surprise worth a log line: a head
/// message is the normal, expected whole-room case, while an empty participant
/// set on a reply means the table has no rows for a thread that has replies --
/// a version window the backfill missed, or a store read that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The message heads its own thread, so it defines the audience.
    Head,
    /// The message replies inside a thread the table has no rows for.
    NoParticipants,
}

/// The audience of one message, and how it was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audience {
    /// Who to wake, sender excluded, no id outside the current membership.
    pub ids: BTreeSet<MemberId>,
    /// `None` when the thread scoped the wake; otherwise why the whole room was
    /// used instead (see [`Fallback`]).
    pub fallback: Option<Fallback>,
}

/// Decide who a message wakes.
///
/// `participants` is the thread's recorded agent roster (`store::Appended`),
/// already narrowed to agents by the table's own domain. The membership is the
/// CURRENT room roster: rows outlive members leaving, so the intersection
/// happens here rather than in a cleanup job.
pub fn audience(
    membership: &RoomMembership,
    sender: &MemberId,
    root: Option<i64>,
    participants: &[MemberId],
) -> Audience {
    // The tier term of the scope decision: tiers land after this narrowing, so
    // today every agent is in the tier that hears its threads. Kept as a named
    // `true` rather than omitted so the day tiers land this is where the term
    // goes -- and `tier_term_is_constant_until_tiers_land` fails if it silently
    // stops being one.
    let in_tier = |_id: &MemberId| true;

    let scope = match (root, participants.is_empty()) {
        (None, _) => Some(Fallback::Head),
        (Some(_), true) => Some(Fallback::NoParticipants),
        (Some(_), false) => None,
    };

    let mut ids = BTreeSet::new();
    for member in &membership.members {
        if member.id == *sender {
            continue;
        }
        let woken = match scope {
            // Whole room: no thread to narrow by.
            Some(_) => true,
            None => match member.kind {
                // A human has no per-thread subscription (see the module docs).
                ParticipantKind::Human => true,
                ParticipantKind::Agent => participants.contains(&member.id) && in_tier(&member.id),
            },
        };
        if woken {
            ids.insert(member.id.clone());
        }
    }

    Audience {
        ids,
        fallback: scope,
    }
}

#[cfg(test)]
mod tests {
    //! The predicate in numbers: who a head wakes, who a reply wakes, and the
    //! three ways the whole room comes back.

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

    /// The example that keeps the predicate honest: A heads a thread, B replied
    /// in it, C is in the room but never took part, h is a human who never took
    /// part. A reply by A wakes B and h, and leaves C alone.
    #[test]
    fn a_reply_wakes_the_thread_and_every_human_not_the_bystander() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("c", ParticipantKind::Agent),
            ("h", ParticipantKind::Human),
        ]);
        let got = audience(&membership, &mid("a"), Some(1), &[mid("a"), mid("b")]);
        assert_eq!(got.ids, BTreeSet::from([mid("b"), mid("h")]));
        assert_eq!(got.fallback, None, "a thread with rows scopes the wake");
    }

    /// A head message has no rows to narrow by: everyone but the sender.
    #[test]
    fn a_head_wakes_the_whole_room_and_says_why() {
        let membership = room(&[
            ("a", ParticipantKind::Agent),
            ("b", ParticipantKind::Agent),
            ("h", ParticipantKind::Human),
        ]);
        let got = audience(&membership, &mid("a"), None, &[]);
        assert_eq!(got.ids, BTreeSet::from([mid("b"), mid("h")]));
        assert_eq!(got.fallback, Some(Fallback::Head));
    }

    /// A reply whose thread has no rows is missing data, not "nobody": fall back
    /// to the whole room and name the reason so the route can warn.
    #[test]
    fn a_reply_without_participants_falls_back_to_the_whole_room() {
        let membership = room(&[("a", ParticipantKind::Agent), ("b", ParticipantKind::Agent)]);
        let got = audience(&membership, &mid("a"), Some(7), &[]);
        assert_eq!(got.ids, BTreeSet::from([mid("b")]));
        assert_eq!(got.fallback, Some(Fallback::NoParticipants));
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
            &[mid("a"), mid("b"), mid("gone")],
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
        let got = audience(&membership, &mid("a"), Some(1), &[mid("b"), mid("c")]);
        assert_eq!(
            got.ids,
            BTreeSet::from([mid("b"), mid("c")]),
            "every recorded agent is in the only tier that exists"
        );
    }
}
