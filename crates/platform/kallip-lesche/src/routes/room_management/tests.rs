//! Room lifecycle + membership, ported to the lesche-local graph. Identity
//! (user existence, tagma enrollment) is attested through the mock control
//! plane rather than a local users/tagmata table; the membership graph itself
//! lives in a real (testcontainers) Postgres.

use super::invites::{CreateInviteRequest, accept_invite, create_invite, list_my_invites};
use super::lifecycle::{CreateRoomRequest, create_room, list_public_rooms, list_rooms};
use super::members::{AddTagmaRequest, add_tagma, join_public_room, remove_member};
use super::roster::{list_my_tagma_rooms, list_tagma_rooms, room_roster};
use crate::db::entity::{room_member_revocations, room_members, rooms};
use crate::routes::test_support::{as_tagma, as_user, db_state, seed_room};
use axum::Json;
use axum::extract::{Path, State};
use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::ids::{ParticipantId, ParticipantKind, TagmaId, UserId};
use kallip_lesche_common::rooms::{RoomMemberProfile, RoomRosterView, Visibility};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;

fn dummy_key() -> Ed25519PublicKey {
    Ed25519PublicKey([0u8; 32].to_vec())
}

/// Create a private room (the default visibility) as `user`. Every test that
/// does not care about visibility uses this; the create-room body requires a
/// non-empty `name`.
async fn create_private_room(
    state: &crate::state::SharedConvState,
    user: &UserId,
) -> Result<Json<super::lifecycle::RoomView>, kallip_common::protocol::ApiError> {
    create_room(
        State(state.clone()),
        as_user(user),
        Json(CreateRoomRequest {
            name: "test".to_string(),
            description: String::new(),
            visibility: Visibility::Private,
        }),
    )
    .await
}

/// Create a room with an explicit visibility.
async fn create_room_visible(
    state: &crate::state::SharedConvState,
    user: &UserId,
    visibility: Visibility,
) -> Result<Json<super::lifecycle::RoomView>, kallip_common::protocol::ApiError> {
    create_room(
        State(state.clone()),
        as_user(user),
        Json(CreateRoomRequest {
            name: "test".to_string(),
            description: String::new(),
            visibility,
        }),
    )
    .await
}

/// Drive `add_tagma` as `caller` and return the rejection error (a fresh room
/// is created each call so the failures are independent). Used by the
/// oracle-uniformity test.
async fn add_tagma_err(
    state: &crate::state::SharedConvState,
    caller: &UserId,
    tagma: TagmaId,
) -> kallip_common::protocol::ApiError {
    let Json(room) = create_private_room(state, caller).await.expect("create");
    add_tagma(
        State(state.clone()),
        as_user(caller),
        Path(room.room_id),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect_err("rejected")
}

/// Drive `remove_member` as `caller` against `participant_id` and return
/// the rejection error. Used by the oracle-uniformity test; a fresh room per
/// call keeps the failures independent.
async fn remove_err(
    state: &crate::state::SharedConvState,
    caller: &UserId,
    room_id: String,
    participant_id: ParticipantId,
) -> kallip_common::protocol::ApiError {
    remove_member(
        State(state.clone()),
        as_user(caller),
        Path((room_id, participant_id.as_ref().to_string())),
    )
    .await
    .expect_err("rejected")
}

/// Drive `create_invite` as `inviter` for `invitee` and return the rejection
/// error (a fresh room per call so the failures are independent).
async fn invite_err(
    state: &crate::state::SharedConvState,
    inviter: &UserId,
    invitee: UserId,
) -> kallip_common::protocol::ApiError {
    let Json(room) = create_private_room(state, inviter).await.expect("create");
    create_invite(
        State(state.clone()),
        as_user(inviter),
        Path(room.room_id),
        Json(CreateInviteRequest {
            invitee_username: invitee.to_string(),
        }),
    )
    .await
    .expect_err("rejected")
}

#[tokio::test]
async fn create_room_rejects_empty_name() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());

    // A whitespace-only name is rejected before any row is written.
    let err = create_room(
        State(state.clone()),
        as_user(&alice),
        Json(CreateRoomRequest {
            name: "   ".to_string(),
            description: String::new(),
            visibility: Visibility::Private,
        }),
    )
    .await
    .expect_err("empty name must be rejected");
    assert_eq!(err.status, 400);

    // No room was created.
    let Json(rooms_view) = list_rooms(State(state), as_user(&alice))
        .await
        .expect("list");
    assert!(rooms_view.is_empty());
}

#[tokio::test]
async fn create_room_lists_creator_as_member() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");

    let Json(rooms_view) = list_rooms(State(state.clone()), as_user(&alice))
        .await
        .expect("list");
    assert_eq!(rooms_view.len(), 1);
    assert_eq!(rooms_view[0].room_id, room.room_id);

    // A non-member sees the room as absent.
    let bob = UserId::from("bob".to_string());
    let Json(bob_rooms) = list_rooms(State(state), as_user(&bob))
        .await
        .expect("list bob");
    assert!(bob_rooms.is_empty());
}

#[tokio::test]
async fn invite_accept_adds_member_and_bumps_epoch() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect("invite");
    let invite_id = invite.invite_id.to_string();

    accept_invite(
        State(state.clone()),
        as_user(&bob),
        Path((room.room_id.clone(), invite_id)),
    )
    .await
    .expect("accept");
    let after = rooms::Entity::find_by_id(room.room_id.clone())
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.membership_epoch, 2);
    let bob_pid = ParticipantId::for_user(&bob);
    let bob_member = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.room_id.clone()))
        .filter(room_members::Column::MemberId.eq(bob_pid.as_ref().to_string()))
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert!(bob_member.is_some());
}

#[tokio::test]
async fn invite_unknown_invitee_is_404() {
    // The handle resolve (registry facts) rejects a nonexistent invitee before
    // any row is written.
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    // bob is NOT seeded -> handle resolves to None -> invitee rejected.
    let bob = UserId::from("bob".to_string());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let err = create_invite(
        State(state),
        as_user(&alice),
        Path(room.room_id),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect_err("unknown invitee rejected");
    assert_eq!(err.status, 404);
}

/// Inviting by `@handle` (with the sigil) resolves the same user: the server
/// strips the leading `@` before resolving. This is the UX path -- the input
/// placeholder is `@username`.
#[tokio::test]
async fn invite_accepts_at_prefixed_handle() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let bob = UserId::from("bob".to_string());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: format!("@{}", bob),
        }),
    )
    .await
    .expect("invite by @handle");
    // The invite landed for bob (accept succeeds as bob).
    accept_invite(
        State(state),
        as_user(&bob),
        Path((room.room_id, invite.invite_id.to_string())),
    )
    .await
    .expect("accept");
}

/// The inbox resolves each invite's inviter to their @handle (never a raw user
/// id): bob sees the invite from alice as `invited_by: "@alice"`.
#[tokio::test]
async fn list_my_invites_resolves_inviter_handle() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let bob = UserId::from("bob".to_string());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect("invite");

    // bob's inbox carries the inviter's handle, not their user id.
    let Json(inbox) = list_my_invites(State(state), as_user(&bob))
        .await
        .expect("list");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].invite_id, invite.invite_id);
    assert_eq!(inbox[0].invited_by, "@alice");
}

/// A malformed handle (fails the registry's normalize) collapses to the same
/// 404 as an unknown handle -- the existence-oracle invariant.
#[tokio::test]
async fn invite_malformed_handle_is_404() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let err = create_invite(
        State(state),
        as_user(&alice),
        Path(room.room_id),
        Json(CreateInviteRequest {
            // Underscore is not a valid handle char; normalize rejects it.
            invitee_username: "no_good".to_string(),
        }),
    )
    .await
    .expect_err("malformed handle rejected");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn add_tagma_attests_enrollment_and_bumps_epoch() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    add_tagma(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("add tagma");
    let after = rooms::Entity::find_by_id(room.room_id.clone())
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.membership_epoch, 2);
}

#[tokio::test]
async fn add_unenrolled_tagma_is_404() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    // tagma is NOT seeded -> tagma_profile returns None -> not room-joinable.
    let tagma = TagmaId::from("ghost".to_string());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let err = add_tagma(
        State(state),
        as_user(&alice),
        Path(room.room_id),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect_err("unenrolled tagma rejected");
    assert_eq!(err.status, 404);
}

/// The room-add gate derives `owner_disabled` locally (parity with the old
/// `tagma_enrolled`): an enrolled tagma whose owner is disabled is rejected.
#[tokio::test]
async fn add_tagma_rejects_owner_disabled_tagma() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    control.disable_user(&alice);
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let err = add_tagma(
        State(state),
        as_user(&alice),
        Path(room.room_id),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect_err("owner-disabled tagma rejected");
    assert_eq!(err.status, 404);
}

/// Existence-oracle preservation: unknown / revoked / owner-disabled tagmas all
/// produce the byte-identical 404 body at the room-add gate (the serialized
/// `ApiError` body is `{"error":{"message": ...}}`, status is on the response
/// line), so a probe cannot distinguish the reason. The relay never branches on
/// *why*.
#[tokio::test]
async fn add_tagma_oracle_body_is_uniform_across_failure_modes() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let owner = UserId::from("owner".to_string());
    control.seed_user(owner.clone());

    let unknown = TagmaId::from("ghost".to_string());
    let revoked = TagmaId::from("rev".to_string());
    control.enroll_tagma(&revoked, owner.clone(), dummy_key(), "tok-rev");
    control.revoke_tagma(&revoked);
    let disabled_owner = TagmaId::from("dis".to_string());
    control.enroll_tagma(&disabled_owner, owner.clone(), dummy_key(), "tok-dis");
    control.disable_user(&owner);

    let e_unknown = add_tagma_err(&state, &alice, unknown).await;
    let e_revoked = add_tagma_err(&state, &alice, revoked).await;
    let e_disabled = add_tagma_err(&state, &alice, disabled_owner).await;
    // Same status + identical message => byte-identical body.
    assert_eq!(e_unknown.status, 404);
    assert_eq!(e_unknown.message, e_revoked.message);
    assert_eq!(e_unknown.message, e_disabled.message);
}

/// The invite gate derives `!disabled` locally (parity with the old
/// `user_exists`, which filtered `disabled_at IS NULL`): a disabled invitee is
/// rejected with the same 404 as an unknown one.
#[tokio::test]
async fn invite_rejects_disabled_invitee() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let bob = UserId::from("bob".to_string());
    control.seed_user(bob.clone());
    control.disable_user(&bob);

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let err = create_invite(
        State(state),
        as_user(&alice),
        Path(room.room_id),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect_err("disabled invitee rejected");
    assert_eq!(err.status, 404);
}

/// Existence-oracle preservation at the invite gate: an unknown invitee and a
/// disabled invitee produce the byte-identical 404 body (no leak of *why*).
#[tokio::test]
async fn invite_oracle_body_is_uniform_across_failure_modes() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let ghost = UserId::from("ghost".to_string()); // not seeded
    let disabled = UserId::from("dis".to_string());
    control.seed_user(disabled.clone());
    control.disable_user(&disabled);

    let e_unknown = invite_err(&state, &alice, ghost).await;
    let e_disabled = invite_err(&state, &alice, disabled).await;
    assert_eq!(e_unknown.status, 404);
    assert_eq!(e_unknown.message, e_disabled.message);
}

/// Look up a roster member's profile by its member id.
fn profile_for<'a>(roster: &'a RoomRosterView, pid: &ParticipantId) -> &'a RoomMemberProfile {
    roster
        .members
        .iter()
        .find(|m| m.id.as_ref() == pid.as_ref())
        .expect("member present in roster")
}

/// The roster resolves each member's display identity from the registry's raw
/// facts: an agent gets its label + `<prefix>@<owner>` handle; a human gets
/// `display_name` + `@username`.
#[tokio::test]
async fn roster_resolves_member_display_names() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user_with(alice.clone(), "alice", Some("Alice"));
    let bob = UserId::from("bob".to_string());
    control.seed_user_with(bob.clone(), "bob", Some("Bob"));
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");
    control.set_tagma_label(&tagma, Some("Helper".to_string()));

    seed_room(
        state.db.as_ref().unwrap(),
        "room-1",
        &alice,
        &[&bob],
        &[&tagma],
    )
    .await;

    let Json(roster) = room_roster(State(state), as_user(&alice), Path("room-1".to_string()))
        .await
        .expect("roster");

    let bob_p = profile_for(&roster, &ParticipantId::for_user(&bob));
    assert_eq!(bob_p.handle, "@bob");
    assert_eq!(bob_p.label.as_deref(), Some("Bob"));

    let tagma_pid = ParticipantId::for_tagma(&tagma);
    let tagma_p = profile_for(&roster, &tagma_pid);
    assert_eq!(
        tagma_p.handle,
        format!("{}@alice", &tagma_pid.as_ref()[..6])
    );
    assert_eq!(tagma_p.label.as_deref(), Some("Helper"));

    // Members are returned member_id-ascending (stable client-side diff).
    let pids: Vec<String> = roster
        .members
        .iter()
        .map(|m| m.id.as_ref().to_string())
        .collect();
    let mut sorted = pids.clone();
    sorted.sort();
    assert_eq!(pids, sorted, "roster members are member_id-ascending");
}

/// A member the registry does NOT resolve (a tagma never enrolled / a user never
/// seeded) degrades to a prefix-only handle with no label.
#[tokio::test]
async fn roster_degrades_to_prefix_for_unresolved_members() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user_with(alice.clone(), "alice", None);
    let ghost_tagma = TagmaId::from("ghost-t".to_string()); // never enrolled
    let ghost_user = UserId::from("ghost-u".to_string()); // never seeded

    seed_room(
        state.db.as_ref().unwrap(),
        "room-1",
        &alice,
        &[&ghost_user],
        &[&ghost_tagma],
    )
    .await;

    let Json(roster) = room_roster(State(state), as_user(&alice), Path("room-1".to_string()))
        .await
        .expect("roster");

    let t_pid = ParticipantId::for_tagma(&ghost_tagma);
    let t_p = profile_for(&roster, &t_pid);
    assert_eq!(t_p.handle, format!("agent {}", &t_pid.as_ref()[..6]));
    assert!(t_p.label.is_none());

    let u_pid = ParticipantId::for_user(&ghost_user);
    let u_p = profile_for(&roster, &u_pid);
    assert_eq!(u_p.handle, format!("user {}", &u_pid.as_ref()[..6]));
    assert!(u_p.label.is_none());
}

/// Display policy: revocation is an authz state, not a display state -- a
/// revoked tagma that is still a room member shows its label + handle.
#[tokio::test]
async fn roster_shows_label_for_revoked_tagma_member() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user_with(alice.clone(), "alice", None);
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");
    control.set_tagma_label(&tagma, Some("Helper".to_string()));
    control.revoke_tagma(&tagma);

    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&tagma]).await;

    let Json(roster) = room_roster(State(state), as_user(&alice), Path("room-1".to_string()))
        .await
        .expect("roster");

    let tagma_pid = ParticipantId::for_tagma(&tagma);
    let tagma_p = profile_for(&roster, &tagma_pid);
    assert_eq!(tagma_p.label.as_deref(), Some("Helper"));
    assert_eq!(
        tagma_p.handle,
        format!("{}@alice", &tagma_pid.as_ref()[..6])
    );

    // Alice has no display_name -> label None (and the wire omits the field).
    let alice_p = profile_for(&roster, &ParticipantId::for_user(&alice));
    assert!(alice_p.label.is_none());
    assert_eq!(alice_p.handle, "@alice");
}

/// Display policy: disability is an authz state, not a display state -- a
/// disabled human that is still a room member shows their display name + handle.
#[tokio::test]
async fn roster_shows_name_for_disabled_human_member() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user_with(alice.clone(), "alice", None);
    let bob = UserId::from("bob".to_string());
    control.seed_user_with(bob.clone(), "bob", Some("Bob"));
    control.disable_user(&bob);

    seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[&bob], &[]).await;

    let Json(roster) = room_roster(State(state), as_user(&alice), Path("room-1".to_string()))
        .await
        .expect("roster");

    let bob_p = profile_for(&roster, &ParticipantId::for_user(&bob));
    assert_eq!(bob_p.label.as_deref(), Some("Bob"));
    assert_eq!(bob_p.handle, "@bob");
}

#[tokio::test]
async fn creator_removes_other_member_hard_deletes_and_audits() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect("invite");
    accept_invite(
        State(state.clone()),
        as_user(&bob),
        Path((room.room_id.clone(), invite.invite_id.to_string())),
    )
    .await
    .expect("accept");

    // alice is the creator; she removes bob by his derived participant id.
    let bob_pid = ParticipantId::for_user(&bob);
    remove_member(
        State(state.clone()),
        as_user(&alice),
        Path((room.room_id.clone(), bob_pid.as_ref().to_string())),
    )
    .await
    .expect("remove");

    // Live row gone, audit row present.
    let live = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.room_id.clone()))
        .filter(room_members::Column::MemberId.eq(bob_pid.as_ref().to_string()))
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert!(live.is_none());
    let audits = room_member_revocations::Entity::find()
        .filter(room_member_revocations::Column::RoomId.eq(room.room_id))
        .all(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(audits.len(), 1);
}

#[tokio::test]
async fn roster_member_sees_room_non_member_is_404() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let carol = UserId::from("carol".to_string());
    control.seed_user(alice.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");

    let Json(roster) = room_roster(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
    )
    .await
    .expect("member roster");
    assert_eq!(roster.membership_epoch, 1);
    assert!(roster.is_creator);

    let err = room_roster(State(state), as_user(&carol), Path(room.room_id))
        .await
        .expect_err("non-member 404");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn tagma_discovery_lists_own_rooms_and_elects_creator() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    add_tagma(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("add tagma");

    // The tagma lists its rooms; it is the only agent, so it is the creator.
    let Json(views) = list_tagma_rooms(
        State(state.clone()),
        as_tagma(&tagma),
        Path(tagma.to_string()),
    )
    .await
    .expect("discovery");
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].room_id.as_ref(), room.room_id);
    assert!(views[0].is_creator);
    assert_eq!(views[0].membership_epoch, 2);

    // A different tagma sees none of these rooms.
    let other = TagmaId::from("tagma-other".to_string());
    let Json(other_views) = list_tagma_rooms(
        State(state.clone()),
        as_tagma(&other),
        Path(other.to_string()),
    )
    .await
    .expect("discovery other");
    assert!(other_views.is_empty());

    // Self-only: a mismatched path tagma id is forbidden.
    let err = list_tagma_rooms(State(state), as_tagma(&other), Path(tagma.to_string()))
        .await
        .expect_err("path mismatch");
    assert_eq!(err.status, 403);
}

/// The owner-side view (`list_my_tagma_rooms`) returns the same rooms the
/// tagma-self poll sees, but authenticated by the user cookie + registry
/// ownership attestation. The owner sees the room the tagma joined and its
/// `is_creator` election (it is the only agent).
#[tokio::test]
async fn list_my_tagma_rooms_owner_sees_joined_rooms() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    add_tagma(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("add tagma");

    let Json(views) = list_my_tagma_rooms(
        State(state.clone()),
        as_user(&alice),
        Path(tagma.to_string()),
    )
    .await
    .expect("owner list");
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].room_id.as_ref(), room.room_id);
    assert!(views[0].is_creator);
    // The room's `name` round-trips through the optional wire field.
    assert_eq!(views[0].name.as_deref(), Some("test"));
}

/// Ownership gate + existence-oracle: a user who does NOT own the tagma (and an
/// unknown tagma) both collapse to the byte-identical "unknown tagma" 404.
#[tokio::test]
async fn list_my_tagma_rooms_non_owner_and_unknown_are_404() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");

    let e_non_owner =
        list_my_tagma_rooms(State(state.clone()), as_user(&bob), Path(tagma.to_string()))
            .await
            .expect_err("non-owner 404");
    let e_unknown = list_my_tagma_rooms(State(state), as_user(&alice), Path("ghost".to_string()))
        .await
        .expect_err("unknown tagma 404");
    assert_eq!(e_non_owner.status, 404);
    assert_eq!(e_unknown.status, 404);
    assert_eq!(e_non_owner.message, e_unknown.message);
}

#[tokio::test]
async fn duplicate_pending_invite_is_409() {
    // A second live invite for the same (room, invitee) hits the row-locked
    // prior -> 409 (the same status the partial-unique-index race maps to).
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let _ = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect("first invite");

    let err = create_invite(
        State(state),
        as_user(&alice),
        Path(room.room_id),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect_err("duplicate pending rejected");
    assert_eq!(err.status, 409);
}

#[tokio::test]
async fn double_accept_is_409() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect("invite");
    let invite_id = invite.invite_id.to_string();

    accept_invite(
        State(state.clone()),
        as_user(&bob),
        Path((room.room_id.clone(), invite_id.clone())),
    )
    .await
    .expect("first accept");

    let err = accept_invite(State(state), as_user(&bob), Path((room.room_id, invite_id)))
        .await
        .expect_err("second accept rejected");
    assert_eq!(err.status, 409);
}

#[tokio::test]
async fn accept_by_non_invitee_is_404() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    let carol = UserId::from("carol".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect("invite");
    let invite_id = invite.invite_id.to_string();

    // Carol is not the invitee; she gets the same 404 as a missing invite.
    let err = accept_invite(
        State(state),
        as_user(&carol),
        Path((room.room_id, invite_id)),
    )
    .await
    .expect_err("non-invitee rejected");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn invite_by_non_member_is_404() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    // bob is not a member of alice's room -> membership gate 404.
    let err = create_invite(
        State(state),
        as_user(&bob),
        Path(room.room_id),
        Json(CreateInviteRequest {
            invitee_username: alice.to_string(),
        }),
    )
    .await
    .expect_err("non-member rejected");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn creator_removes_tagma_they_do_not_own_hard_deletes_and_audits() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let owner = UserId::from("owner".to_string());
    control.seed_user(alice.clone());
    control.seed_user(owner.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    // tagma owned by `owner`, but alice (the room creator) adds it.
    control.enroll_tagma(&tagma, owner.clone(), dummy_key(), "tok");

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    add_tagma(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("add");
    // alice is the creator (not the tagma's owner) -> creator branch removes it.
    let tagma_pid = ParticipantId::for_tagma(&tagma);
    remove_member(
        State(state.clone()),
        as_user(&alice),
        Path((room.room_id.clone(), tagma_pid.as_ref().to_string())),
    )
    .await
    .expect("remove tagma");

    // Live row gone; the audit row is self-describing with kind=agent.
    let live = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.room_id.clone()))
        .filter(room_members::Column::MemberId.eq(tagma_pid.as_ref().to_string()))
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert!(live.is_none());
    let audits = room_member_revocations::Entity::find()
        .filter(room_member_revocations::Column::RoomId.eq(room.room_id))
        .all(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].kind, "agent");
    assert_eq!(audits[0].source_id, tagma.to_string());
}

/// Any member may remove themselves (the leave path). bob is a non-creator
/// member; he removes his own row by his derived participant id.
#[tokio::test]
async fn any_member_can_leave_self() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect("invite");
    accept_invite(
        State(state.clone()),
        as_user(&bob),
        Path((room.room_id.clone(), invite.invite_id.to_string())),
    )
    .await
    .expect("accept");

    let bob_pid = ParticipantId::for_user(&bob);
    remove_member(
        State(state.clone()),
        as_user(&bob),
        Path((room.room_id.clone(), bob_pid.as_ref().to_string())),
    )
    .await
    .expect("self-leave");

    let live = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.room_id))
        .filter(room_members::Column::MemberId.eq(bob_pid.as_ref().to_string()))
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert!(live.is_none());
}

/// A tagma's owner may remove it from a room they are a member of but did not
/// create. owner is invited into alice's room and owns the tagma alice added.
#[tokio::test]
async fn owner_removes_own_tagma_without_being_creator() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let owner = UserId::from("owner".to_string());
    control.seed_user(alice.clone());
    control.seed_user(owner.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, owner.clone(), dummy_key(), "tok");

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    add_tagma(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("add");
    // owner is a member (invited + accepted) but NOT the creator.
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: owner.to_string(),
        }),
    )
    .await
    .expect("invite");
    accept_invite(
        State(state.clone()),
        as_user(&owner),
        Path((room.room_id.clone(), invite.invite_id.to_string())),
    )
    .await
    .expect("accept");

    let tagma_pid = ParticipantId::for_tagma(&tagma);
    remove_member(
        State(state.clone()),
        as_user(&owner),
        Path((room.room_id.clone(), tagma_pid.as_ref().to_string())),
    )
    .await
    .expect("owner pulls own tagma");

    let live = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.room_id))
        .filter(room_members::Column::MemberId.eq(tagma_pid.as_ref().to_string()))
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert!(live.is_none());
}

/// The owner-pulls-agent path works CROSS-ROOM: the owner is neither the
/// creator nor even a member of the room their tagma is in. This is the case
/// `require_member_locked` used to forbid; the new authz admits it.
#[tokio::test]
async fn non_member_owner_removes_own_tagma_succeeds() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let owner = UserId::from("owner".to_string());
    control.seed_user(alice.clone());
    control.seed_user(owner.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, owner.clone(), dummy_key(), "tok");

    // alice creates the room and adds owner's tagma; owner is never invited.
    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    add_tagma(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("add");

    let tagma_pid = ParticipantId::for_tagma(&tagma);
    remove_member(
        State(state.clone()),
        as_user(&owner),
        Path((room.room_id.clone(), tagma_pid.as_ref().to_string())),
    )
    .await
    .expect("cross-room owner pull");

    let live = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.room_id))
        .filter(room_members::Column::MemberId.eq(tagma_pid.as_ref().to_string()))
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert!(live.is_none());
}

/// The owner path uses a raw `owner_user_id == caller` compare, NOT
/// `bilateral_resolvable` (which requires `enrolled`): an owner may pull a
/// revoked tagma out of a room. Locks the design against a future "simplify".
#[tokio::test]
async fn owner_removes_revoked_own_tagma_succeeds() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let owner = UserId::from("owner".to_string());
    control.seed_user(alice.clone());
    control.seed_user(owner.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, owner.clone(), dummy_key(), "tok");

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    add_tagma(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("add");
    control.revoke_tagma(&tagma);

    let tagma_pid = ParticipantId::for_tagma(&tagma);
    remove_member(
        State(state.clone()),
        as_user(&owner),
        Path((room.room_id.clone(), tagma_pid.as_ref().to_string())),
    )
    .await
    .expect("owner pulls revoked own tagma");

    let live = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.room_id))
        .filter(room_members::Column::MemberId.eq(tagma_pid.as_ref().to_string()))
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert!(live.is_none());
}

/// Existence-oracle preservation at the remove gate: a non-member probe, a
/// member removing another without creator rights, a member removing a tagma
/// they do not own, and a missing participant id all produce the byte-identical
/// 404 body (no leak of room / target / reason).
#[tokio::test]
async fn remove_member_oracle_body_is_uniform_across_failure_modes() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    let carol = UserId::from("carol".to_string());
    let owner = UserId::from("owner".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());
    control.seed_user(carol.clone());
    control.seed_user(owner.clone());
    let foreign_tagma = TagmaId::from("foreign".to_string());
    control.enroll_tagma(&foreign_tagma, owner.clone(), dummy_key(), "tok");

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    // bob is a second member (the target of a non-creator removal attempt).
    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(CreateInviteRequest {
            invitee_username: bob.to_string(),
        }),
    )
    .await
    .expect("invite");
    accept_invite(
        State(state.clone()),
        as_user(&bob),
        Path((room.room_id.clone(), invite.invite_id.to_string())),
    )
    .await
    .expect("accept");
    // A tagma bob does not own (owned by `owner`).
    add_tagma(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
        Json(AddTagmaRequest {
            tagma_id: foreign_tagma.to_string(),
        }),
    )
    .await
    .expect("add tagma");

    let bob_pid = ParticipantId::for_user(&bob);
    let foreign_pid = ParticipantId::for_tagma(&foreign_tagma);
    let missing_pid = ParticipantId::from("does-not-exist".to_string());

    // (a) non-member (carol) probing a real target.
    let e_nonmember = remove_err(&state, &carol, room.room_id.clone(), bob_pid.clone()).await;
    // (b) member (bob) removing another member (alice) without creator rights.
    let alice_pid = ParticipantId::for_user(&alice);
    let e_no_rights = remove_err(&state, &bob, room.room_id.clone(), alice_pid).await;
    // (c) member (bob) removing a tagma he does not own.
    let e_not_owner = remove_err(&state, &bob, room.room_id.clone(), foreign_pid).await;
    // (d) a participant id that maps to no row.
    let e_missing = remove_err(&state, &alice, room.room_id.clone(), missing_pid).await;

    // Same status + identical message => byte-identical body. Assert status on
    // each so a regression that flips one path to 403 cannot slip past.
    assert_eq!(e_nonmember.status, 404);
    assert_eq!(e_no_rights.status, 404);
    assert_eq!(e_not_owner.status, 404);
    assert_eq!(e_missing.status, 404);
    assert_eq!(e_nonmember.message, e_no_rights.message);
    assert_eq!(e_nonmember.message, e_not_owner.message);
    assert_eq!(e_nonmember.message, e_missing.message);
}

#[tokio::test]
async fn new_room_defaults_to_private() {
    // A default-visibility create (private) round-trips through list_rooms + the
    // persisted row.
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    assert_eq!(room.visibility, Visibility::Private);

    let Json(rooms_view) = list_rooms(State(state.clone()), as_user(&alice))
        .await
        .expect("list");
    assert_eq!(rooms_view.len(), 1);
    assert_eq!(rooms_view[0].visibility, Visibility::Private);

    let row = rooms::Entity::find_by_id(room.room_id)
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.visibility, "private");
}

#[tokio::test]
async fn public_room_is_listed_in_discovery_private_is_not() {
    // Only public rooms surface in the open-access discovery list; a private
    // room (even one the caller owns) never appears there.
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(_private) = create_private_room(&state, &alice).await.expect("private");
    let Json(public) = create_room_visible(&state, &alice, Visibility::Public)
        .await
        .expect("public");

    // Bob (not yet a member of either) sees only the public room.
    let Json(discovered) = list_public_rooms(State(state.clone()), as_user(&bob))
        .await
        .expect("discover");
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].room_id, public.room_id);
    assert_eq!(discovered[0].visibility, Visibility::Public);
}

#[tokio::test]
async fn join_public_room_adds_member_and_bumps_epoch() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_room_visible(&state, &alice, Visibility::Public)
        .await
        .expect("public");

    join_public_room(
        State(state.clone()),
        as_user(&bob),
        Path(room.room_id.clone()),
    )
    .await
    .expect("bob joins");

    // Bob is now a member and the epoch advanced.
    let bob_pid = ParticipantId::for_user(&bob);
    let member = room_members::Entity::find()
        .filter(room_members::Column::RoomId.eq(room.room_id.clone()))
        .filter(room_members::Column::MemberId.eq(bob_pid.as_ref().to_string()))
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap();
    assert!(member.is_some());
    let row = rooms::Entity::find_by_id(room.room_id)
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.membership_epoch, 2);
}

#[tokio::test]
async fn join_public_room_is_idempotent() {
    // A second join by an already-member is a no-op: no second epoch bump.
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());

    let Json(room) = create_room_visible(&state, &alice, Visibility::Public)
        .await
        .expect("public");

    join_public_room(
        State(state.clone()),
        as_user(&alice),
        Path(room.room_id.clone()),
    )
    .await
    .expect("re-join by creator is a no-op");
    let row = rooms::Entity::find_by_id(room.room_id.clone())
        .one(state.db.as_ref().unwrap())
        .await
        .unwrap()
        .unwrap();
    // Creator was already the founding member -> epoch stays at 1.
    assert_eq!(row.membership_epoch, 1);
}

#[tokio::test]
async fn join_private_room_is_indistinguishable_from_unknown() {
    // A private room must be entered via the invite flow, but /join collapses
    // that refusal to the SAME 404 an unknown room returns -- distinguishing them
    // would leak room existence to a non-member probing ids (existence-oracle).
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let bob = UserId::from("bob".to_string());
    control.seed_user(alice.clone());
    control.seed_user(bob.clone());

    let Json(room) = create_private_room(&state, &alice).await.expect("private");
    let private_err = join_public_room(State(state.clone()), as_user(&bob), Path(room.room_id))
        .await
        .expect_err("private room join refused");
    let unknown_err = join_public_room(
        State(state),
        as_user(&bob),
        Path("00000000-0000-0000-0000-000000000000".to_string()),
    )
    .await
    .expect_err("unknown room join refused");
    assert_eq!(private_err.status, 404);
    assert_eq!(unknown_err.status, 404);
    assert_eq!(private_err.message, unknown_err.message);
}

#[tokio::test]
async fn list_rooms_is_newest_joined_first() {
    // The "my rooms" list is documented newest-joined first. `create_room`
    // stamps `joined_at = now`, which is not controllable, so the three rooms
    // are inserted directly with distinct, increasing join timestamps; the list
    // must then come back newest-first (the second `rooms::find().is_in` fetch
    // returns rows in DB/heap order, so ordering must be re-applied from the
    // membership query).
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let db = state.db.as_ref().expect("db present");
    let joined = [
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        OffsetDateTime::from_unix_timestamp(1_700_000_010).unwrap(),
        OffsetDateTime::from_unix_timestamp(1_700_000_020).unwrap(),
    ];
    for (i, ts) in joined.iter().enumerate() {
        let room_id = format!("0000000{i}-0000-0000-0000-000000000000");
        rooms::ActiveModel {
            id: Set(room_id.clone()),
            created_by_user_id: Set(alice.to_string()),
            created_at: Set(*ts),
            membership_epoch: Set(1),
            name: Set(format!("room{i}")),
            description: Set(String::new()),
            visibility: Set(Visibility::Private.as_str().to_string()),
        }
        .insert(db)
        .await
        .expect("insert room");
        room_members::ActiveModel {
            room_id: Set(room_id),
            member_id: Set(ParticipantId::for_user(&alice).as_ref().to_string()),
            kind: Set(ParticipantKind::Human.as_str().to_string()),
            source_id: Set(alice.to_string()),
            joined_at: Set(*ts),
            added_by: Set(alice.to_string()),
        }
        .insert(db)
        .await
        .expect("insert member");
    }

    let Json(rooms_view) = list_rooms(State(state), as_user(&alice))
        .await
        .expect("list");
    assert_eq!(rooms_view.len(), 3);
    // Newest-joined first: room2 (latest) -> room1 -> room0.
    assert_eq!(rooms_view[0].name, "room2");
    assert_eq!(rooms_view[1].name, "room1");
    assert_eq!(rooms_view[2].name, "room0");
}

/// Insert `n` human members directly into an existing room (bypassing the
/// handlers) to drive it toward the member cap without issuing dozens of
/// invites. Each inserted id is `{prefix}{i}`.
///
/// The cap (`MAX_ROOM_MEMBERS`) is enforced inside the txn, after the
/// idempotency check, so a re-add of an already-present member stays a no-op
/// regardless of room size -- the tests below rely on both halves of that.
async fn seed_extra_members(
    db: &crate::db::Db,
    room_id: &str,
    added_by: &UserId,
    n: usize,
    prefix: &str,
) {
    let now = OffsetDateTime::now_utc();
    for i in 0..n {
        let u = UserId::from(format!("{prefix}{i}"));
        room_members::ActiveModel {
            room_id: Set(room_id.to_string()),
            member_id: Set(ParticipantId::for_user(&u).as_ref().to_string()),
            kind: Set(ParticipantKind::Human.as_str().to_string()),
            source_id: Set(u.to_string()),
            joined_at: Set(now),
            added_by: Set(added_by.to_string()),
        }
        .insert(db)
        .await
        .expect("insert extra member");
    }
}

#[tokio::test]
async fn add_tagma_below_cap_succeeds() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");

    // One short of the cap (creator + (cap - 2) extras), so adding the tagma
    // lands at exactly the cap and succeeds.
    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let db = state.db.as_ref().expect("db present");
    seed_extra_members(
        db,
        &room.room_id,
        &alice,
        (super::shared::MAX_ROOM_MEMBERS - 2) as usize,
        "u",
    )
    .await;

    add_tagma(
        State(state),
        as_user(&alice),
        Path(room.room_id),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("add tagma below cap");
}

#[tokio::test]
async fn add_tagma_at_cap_is_409() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");

    // At the cap (creator + (cap - 1) extras), so a fresh tagma is refused.
    let Json(room) = create_private_room(&state, &alice).await.expect("create");
    let db = state.db.as_ref().expect("db present");
    seed_extra_members(
        db,
        &room.room_id,
        &alice,
        (super::shared::MAX_ROOM_MEMBERS - 1) as usize,
        "u",
    )
    .await;

    let err = add_tagma(
        State(state),
        as_user(&alice),
        Path(room.room_id),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect_err("cap blocks add");
    assert_eq!(err.status, 409);
}

#[tokio::test]
async fn add_tagma_idempotent_at_cap_is_noop() {
    // Re-adding a tagma already in the room is a no-op even at the cap: the cap
    // check runs only for a genuinely new member.
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let tagma = TagmaId::from("tagma-1".to_string());
    control.enroll_tagma(&tagma, alice.clone(), dummy_key(), "tok");

    // Seed at the cap with the tagma already present (1 creator + (cap - 2)
    // humans + 1 tagma).
    let db = state.db.as_ref().expect("db present");
    let extras: Vec<UserId> = (0..super::shared::MAX_ROOM_MEMBERS - 2)
        .map(|i| UserId::from(format!("u{i}")))
        .collect();
    let extra_refs: Vec<&UserId> = extras.iter().collect();
    seed_room(db, "room-1", &alice, &extra_refs, &[&tagma]).await;

    add_tagma(
        State(state),
        as_user(&alice),
        Path("room-1".to_string()),
        Json(AddTagmaRequest {
            tagma_id: tagma.to_string(),
        }),
    )
    .await
    .expect("idempotent re-add at cap");
}

#[tokio::test]
async fn join_public_room_at_cap_is_409() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    control.seed_user(alice.clone());
    let late = UserId::from("late".to_string());
    control.seed_user(late.clone());

    // Public room at the cap (creator + (cap - 1) extras); a late self-join is
    // refused.
    let Json(room) = create_room_visible(&state, &alice, Visibility::Public)
        .await
        .expect("public");
    let db = state.db.as_ref().expect("db present");
    seed_extra_members(
        db,
        &room.room_id,
        &alice,
        (super::shared::MAX_ROOM_MEMBERS - 1) as usize,
        "u",
    )
    .await;

    let err = join_public_room(State(state), as_user(&late), Path(room.room_id))
        .await
        .expect_err("cap blocks join");
    assert_eq!(err.status, 409);
}

#[tokio::test]
async fn accept_invite_at_cap_blocks_new_member() {
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let carol = UserId::from("carol".to_string());
    control.seed_user(alice.clone());
    control.seed_user(carol.clone());

    // Private room at the cap (creator + (cap - 1) extras); carol is not a member.
    let db = state.db.as_ref().expect("db present");
    let extras: Vec<UserId> = (0..super::shared::MAX_ROOM_MEMBERS - 1)
        .map(|i| UserId::from(format!("u{i}")))
        .collect();
    let extra_refs: Vec<&UserId> = extras.iter().collect();
    seed_room(db, "room-1", &alice, &extra_refs, &[]).await;

    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path("room-1".to_string()),
        Json(CreateInviteRequest {
            invitee_username: carol.to_string(),
        }),
    )
    .await
    .expect("invite");

    let err = accept_invite(
        State(state),
        as_user(&carol),
        Path(("room-1".to_string(), invite.invite_id.to_string())),
    )
    .await
    .expect_err("cap blocks accept");
    assert_eq!(err.status, 409);
}

#[tokio::test]
async fn accept_invite_at_cap_lets_existing_member_re_accept() {
    // An already-member re-accepting their invite is a no-op member write (the
    // invite is still stamped) and must not hit the cap even in a full room.
    let (state, control, _container) = db_state().await;
    let alice = UserId::from("alice".to_string());
    let carol = UserId::from("carol".to_string());
    control.seed_user(alice.clone());
    control.seed_user(carol.clone());

    // Private room at the cap with carol already present (creator + carol +
    // (cap - 2) extras).
    let db = state.db.as_ref().expect("db present");
    let mut humans: Vec<UserId> = (0..super::shared::MAX_ROOM_MEMBERS - 2)
        .map(|i| UserId::from(format!("u{i}")))
        .collect();
    humans.insert(0, carol.clone());
    let human_refs: Vec<&UserId> = humans.iter().collect();
    seed_room(db, "room-1", &alice, &human_refs, &[]).await;

    let (_, Json(invite)) = create_invite(
        State(state.clone()),
        as_user(&alice),
        Path("room-1".to_string()),
        Json(CreateInviteRequest {
            invitee_username: carol.to_string(),
        }),
    )
    .await
    .expect("invite");

    accept_invite(
        State(state),
        as_user(&carol),
        Path(("room-1".to_string(), invite.invite_id.to_string())),
    )
    .await
    .expect("re-accept at cap stamps invite");
}
