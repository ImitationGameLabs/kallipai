// Wire types for the `kallipai-lesche` data-plane relay's `/` HTTP surface:
// the rooms family. These mirror the serde DTOs in
// `crates/platform/kallipai-lesche-common/src/{message,event,rooms}.rs` and
// the lesche's routes. The bilateral conversation wire types and the
// projection family live in @kallipai/kallipai-keryx-client; the shared chat
// vocabulary and the envelope/status carriers live in
// `@kallipai/kallipai-common`, re-exported below.
//
// The chat wire types (`AuthoredEvent`, `SignalEvent`, `TagmaReply`,
// `FailoverChainExhausted`) live in `@kallipai/kallipai-common` and are
// re-exported here, because the direct (offline) client shares them -- the
// external chat-room vocabulary is transport-agnostic.

// Re-export the shared chat vocabulary (one source of truth in kallipai-common).
export type {
  AuthoredEvent,
  Envelope,
  FailoverChainExhausted,
  HistoryEntry,
  FileAttachment,
  Participant,
  SignalEvent,
  TagmaReply,
  TagmaStatusPayload,
} from "@kallipai/kallipai-common";
// The data-plane error class is shared with the keryx client (both services
// emit the same error envelope); it lives in kallipai-common.
export { DataPlaneApiError } from "@kallipai/kallipai-common";
import type {
  Envelope,
  Participant,
  ParticipantKind,
  SignalEvent,
  TagmaStatusPayload,
} from "@kallipai/kallipai-common";

/**
 * One message row from a room's history (`GET /rooms/{id}/messages`). The
 * lesche stores room payloads opaquely; `ciphertext` (base64) is the plaintext
 * `RoomMessage` JSON bytes (rooms are server-readable). `seq` is per-room,
 * monotonic from 1 (the `after_seq` cursor); `created_at` is ISO8601.
 */
export interface RoomMessageView {
  readonly seq: number;
  readonly sender: Participant;
  readonly epoch: number;
  readonly ciphertext: string;
  readonly created_at: string;
}

// --- room management (relocated from archeion) --------------------------------
// The chat domain lives in lesche; these mirror the lesche route DTOs in
// `crates/platform/kallipai-lesche/src/routes/room_management/`.

export type { ParticipantKind } from "@kallipai/kallipai-common";

/**
 * Room visibility -- the public/private distinction. Immutable after create.
 * - `private` (default): invite/accept membership (lesche-enforced access).
 * - `public`: open-access (discover + join without invite).
 * Both store room payloads in plaintext on the server.
 */
export type Visibility = "private" | "public";

/** A roster member with its server-resolved display identity: the stable
 * `handle` (`<id-prefix>@<owner>` for an agent, `@username` for a human -- the
 * same authoritative text the relay stamps on room-message senders) plus the
 * mutable `label` (an agent's owner-set label, a human's `display_name`). The
 * view prepends `label` to `handle` at render; `label` is omitted on the wire
 * (undefined) when the registry did not resolve the member.
 *
 * `online` is the member's live connection state at roster-fetch time (an agent
 * holds a tunnel / a human holds an app stream). It is soft, per-incarnation
 * state; the `room_member_online` / `room_member_offline` SSE deltas keep it
 * live between fetches, and the roster re-fetch resyncs it. */
export interface RoomMemberProfile {
  readonly id: string;
  readonly kind: ParticipantKind;
  /** The mutable display name. Absent (undefined) when unresolved. */
  readonly label?: string;
  /** The stable, unforgeable handle. Always present. */
  readonly handle: string;
  /** Live connection state at fetch time. Soft; the SSE deltas are the live
   * layer, this is the fetch-time ground truth. */
  readonly online: boolean;
  /** The agent's `tagma_id`, so a roster row can deep-link to its profile
   * without reversing the one-way participant id. Absent for humans. */
  readonly tagma_id?: string;
}

/** `POST /rooms` -> the new room; the caller is the founding member. */
export interface RoomView {
  readonly room_id: string;
  readonly created_at: string;
  readonly name: string;
  readonly description: string;
  readonly visibility: Visibility;
  /** The caller's per-room read watermark (the unread backbone). 0 for a
   * room never read. Present on list-rooms responses; the public discovery
   * listing always carries 0 (the caller is not a member). */
  readonly last_read_seq: number;
}

/** `GET /rooms/{id}` -- a single room's live membership snapshot (member-only;
 * a non-member gets 404). `is_creator` is server-authoritative. `members` carry
 * server-resolved display identity. */
export interface RoomRosterView {
  readonly room_id: string;
  readonly members: readonly RoomMemberProfile[];
  readonly membership_epoch: number;
  readonly is_creator: boolean;
  readonly visibility: Visibility;
}

/** The bare membership atom (id + kind only, no display identity) used by the
 * tagma-side room discovery view. Distinct from `RoomMemberProfile`, which
 * carries server-resolved handle/label for the user-device roster. */
export interface RoomMember {
  readonly id: string;
  readonly kind: ParticipantKind;
}

/** `GET /me/tagmata/{id}/rooms` -- one row of the owner's view of a tagma's
 * joined rooms: each with its live membership snapshot and whether THIS tagma
 * is the room's creator. `name` lets an owner label a room they are not
 * themselves a member of when managing their agent's joined rooms. */
export interface TagmaRoomView {
  readonly room_id: string;
  readonly members: readonly RoomMember[];
  readonly membership_epoch: number;
  readonly is_creator: boolean;
  readonly visibility: Visibility;
  readonly name?: string;
}

/** `GET /rooms/invites` -- one of the caller's PENDING invites. */
export interface RoomInviteView {
  readonly invite_id: string;
  readonly room_id: string;
  /** The inviter's @handle (resolved server-side); never a raw user id. */
  readonly invited_by: string;
  readonly created_at: string;
  readonly expires_at: string;
}

/** `POST /rooms/{id}/invites` body / 201 response. */
export interface CreateInviteRequest {
  readonly invitee_username: string;
}
export interface CreateInviteResponse {
  readonly invite_id: string;
  readonly expires_at: string;
}

/** `POST /rooms/{id}/tagmata` body. */
export interface AddTagmaRequest {
  readonly tagma_id: string;
}

/** An event on the app's multiplexed SSE stream (`GET /me/events`). serde
 * tag = `type`, snake_case. `envelope` carries E2EE conversation content. The
 * presence pair (`tagma_online`/`tagma_offline`), `tagma_status`, and
 * `tagma_signal` are plaintext operator metadata, user-scoped like presence.
 * `room_membership_changed` is a transient nudge that a room's membership
 * changed so the frontend refreshes the roster (not buffered; a dropped frame
 * is backstopped by room polling). The `room_member_online`/`room_member_offline`
 * pair fans a peer's room presence transition; it is an idempotent set add/
 * remove (per-room), and the roster's `online` field resyncs it. */
export type LescheEvent =
  | { readonly type: "envelope"; readonly envelope: Envelope }
  | { readonly type: "tagma_online"; readonly tagma_id: string }
  | { readonly type: "tagma_offline"; readonly tagma_id: string }
  | ({
      readonly type: "tagma_status";
      readonly tagma_id: string;
    } & TagmaStatusPayload)
  | {
      readonly type: "tagma_signal";
      readonly tagma_id: string;
      readonly event: SignalEvent;
    }
  | {
      // A room's membership changed (member added/removed). The user-device
      // analog of the tagma `Wake`: drives a roster refresh. Transient: not
      // buffered, no sender exclusion.
      readonly type: "room_membership_changed";
      readonly room_id: string;
    }
  | {
      // A room peer came online (agent tunnel / human app stream). Idempotent
      // set-add for `room_id`; `member_id` joins with `RoomMemberProfile.id`.
      readonly type: "room_member_online";
      readonly room_id: string;
      readonly member_id: string;
    }
  | {
      // A room peer went offline. Idempotent set-remove; the roster re-fetch
      // resyncs. Same audience/scope as the online variant.
      readonly type: "room_member_offline";
      readonly room_id: string;
      readonly member_id: string;
    }
  | {
      // The caller's own read cursor in a room advanced (a PUT echo fanned to
      // the user's other live sessions; the initiating session receives its
      // own write too). Idempotent: advance the read watermark and re-derive
      // the unread count between the known and read watermarks -- never
      // blindly zero (an out-of-order or own-echo event must not resurrect
      // counted messages or lose unread ones). Transient: the room list's
      // last_read_seq field is the fetch-time ground truth.
      readonly type: "room_read_cursor_changed";
      readonly room_id: string;
      readonly last_read_seq: number;
    };
