// Wire types for the `kallip-lesche` data-plane relay's `/v1` HTTP surface.
// These mirror the serde DTOs in
// `crates/platform/kallip-lesche-common/src/{control,message,event}.rs` and the
// lesche's `crates/platform/kallip-lesche/src/routes/conversations.rs`. The
// lesche forwards `Envelope.ciphertext` and the byte fields below without
// interpreting them; every base64 string is STANDARD base64 (padded, +//),
// matching agora-common's bytes.rs.
//
// The chat wire types (`AuthoredEvent`, `SignalEvent`, `TagmaReply`,
// `FailoverChainExhausted`) live in `@kallipai/kallip-common` and are
// re-exported here, because the direct (offline) client shares them — the
// external chat-room vocabulary is transport-agnostic.

// Re-export the shared chat vocabulary (one source of truth in kallip-common).
export type {
  AuthoredEvent,
  FailoverChainExhausted,
  HistoryEntry,
  Participant,
  SignalEvent,
  TagmaReply,
} from "@kallipai/kallip-common";
import type {
  Participant,
  ParticipantKind,
  SignalEvent,
} from "@kallipai/kallip-common";

/** `POST /v1/conversations { tagma_id }` -- resolves the single conversation a
 * tagma owns with its operator (idempotent; id derived from the tagma). */
export interface CreateConversationResponse {
  readonly conversation_id: string;
}

/** The unit the lesche forwards. `sequence_n` is per-conversation, per-sender,
 * monotonic from 0; it doubles as the AEAD nonce counter. `ciphertext` is
 * standard-base64 AEAD output (ChaCha20-Poly1305, tag appended). `timestamp` is
 * ISO 8601 with fractional seconds (time::serde::iso8601); do not round-trip a
 * received value through `Date`. */
export interface Envelope {
  readonly conversation_id: string;
  readonly sender: Participant;
  readonly sequence_n: number;
  readonly trace_id: string;
  readonly timestamp: string;
  readonly ciphertext: string;
}

/** App -> tagma: one semantic op against the tagma, encrypted in an envelope.
 * serde tag = `op`, snake_case. `req_id` correlates the op with its TagmaReply. */
export type TagmaRequest =
  | {
      readonly op: "send_message";
      readonly req_id: number;
      readonly text: string;
    }
  | { readonly op: "interrupt"; readonly req_id: number };

/** App -> tagma: a control op that does NOT drive the agent (today: the
 * cursor-based history pull). Carried in the same encrypted envelope channel as
 * TagmaRequest; the relay dispatches by the `op` discriminant (disjoint from
 * TagmaRequest's). serde tag = `op`, snake_case. */
export type TagmaControl =
  | {
      readonly op: "history";
      readonly req_id: number;
      /** rows with id > after (incremental catch-up). */
      readonly after: number | null;
      /** rows with id < before (scroll-up lazy load). */
      readonly before: number | null;
      readonly limit: number;
    }
  | {
      /** A management operation (budget, agents, profiles, schedules). The
       * relay dispatches it in-process against the tagma route handlers. */
      readonly op: "manage";
      readonly req_id: number;
      readonly method: string;
      readonly path: string;
      readonly body: unknown;
    };

/** App -> tagma (relayed by the lesche): start a 1-RTT key exchange, carrying
 * the app's ephemeral X25519 public key (standard base64). */
export interface KeyExchangeInit {
  readonly ephemeral_public: string;
}

/** Responder -> app: the tagma's ephemeral X25519 public key plus an Ed25519
 * signature over the kex transcript (standard base64). */
export interface KeyExchangeResponse {
  readonly ephemeral_public: string;
  readonly signature: string;
}

/**
 * One message row from a room's history (`GET /v1/rooms/{id}/messages`). The
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

// --- room management (relocated from agora) --------------------------------
// The chat domain lives in lesche; these mirror the lesche route DTOs in
// `crates/platform/kallip-lesche/src/routes/room_management/`.

export type { ParticipantKind } from "@kallipai/kallip-common";

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

/** `POST /v1/rooms` -> the new room; the caller is the founding member. */
export interface RoomView {
  readonly room_id: string;
  readonly created_at: string;
  readonly name: string;
  readonly description: string;
  readonly visibility: Visibility;
}

/** `GET /v1/rooms/{id}` -- a single room's live membership snapshot (member-only;
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

/** `GET /v1/me/tagmata/{id}/rooms` -- one row of the owner's view of a tagma's
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

/** `GET /v1/rooms/invites` -- one of the caller's PENDING invites. */
export interface RoomInviteView {
  readonly invite_id: string;
  readonly room_id: string;
  /** The inviter's @handle (resolved server-side); never a raw user id. */
  readonly invited_by: string;
  readonly created_at: string;
  readonly expires_at: string;
}

/** `POST /v1/rooms/{id}/invites` body / 201 response. */
export interface CreateInviteRequest {
  readonly invitee_username: string;
}
export interface CreateInviteResponse {
  readonly invite_id: string;
  readonly expires_at: string;
}

/** `POST /v1/rooms/{id}/tagmata` body. */
export interface AddTagmaRequest {
  readonly tagma_id: string;
}

/** An event on the app's multiplexed SSE stream (`GET /v1/me/events`). serde
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
  | {
      readonly type: "tagma_status";
      readonly tagma_id: string;
      readonly root_state: "idle" | "busy" | "faulted";
      readonly subagents_total: number;
      readonly subagents_active: number;
      readonly token_budget: number;
      readonly token_consumed: number;
    }
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
    };

/**
 * Lesche API error. Mirrors `kallip_common::protocol::ApiError`. This is a
 * distinct surface from `kallip-ui`'s tagma-transport `classifyError` -- the
 * lesche errors are routed through the realtime/channels stores, not the shared
 * AppShell banner.
 */
export class LescheApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "LescheApiError";
  }
}
