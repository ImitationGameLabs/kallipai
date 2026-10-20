// Browser client for the lesche data-plane relay. The lesche (default :7200)
// is the rooms data plane: room envelopes, room history, room management,
// and the lesche `me/events` SSE (the room family). The bilateral
// conversations, the key exchange, and the keryx `me/events` stream live in
// @kallipai/kallipai-keryx-client. The session cookie is shared
// cross-subdomain with the archeion (`KALLIPAI_ARCHEION_SESSION_COOKIE_DOMAIN`),
// so the same credentialed fetch works. Every fetch carries
// `credentials: "include"` (the session cookie is the auth) and every
// non-GET carries the CSRF marker (`X-Requested-With: kallipai`), which the
// lesche's `csrf_guard` requires on cookie-bearing mutating requests.
// Non-2xx responses become `DataPlaneApiError` (`{ status, message }`). The
// JSON/CSRF fetch discipline and the SSE frame machine live in
// @kallipai/kallipai-common, shared with the keryx client.

import {
  BaseClient,
  type EventFrame,
  readEventFrames,
} from "@kallipai/kallipai-common";
import type {
  AddTagmaRequest,
  CreateInviteRequest,
  CreateInviteResponse,
  Envelope,
  LescheEvent,
  RoomInviteView,
  RoomMessageView,
  RoomRosterView,
  RoomView,
  TagmaRoomView,
  Visibility,
} from "./types.ts";

/** The lesche `/me/events` frame: the room-family stream. */
export type MeEventFrame = EventFrame<LescheEvent>;

/**
 * Data-plane client (the lesche service, default :7200): the rooms family --
 * room envelopes, room history, room management, and the lesche `me/events`
 * SSE.
 */
export class LescheClient extends BaseClient {
  // -- rooms (multi-member data plane) ---------------------------------------

  /** `POST /rooms/{id}/envelopes` — store + fan a room envelope to the
   * room's other live members. Returns on 202; offline members pull the row via
   * `fetchRoomMessages`. The payload is the plaintext `RoomMessage` JSON (the
   * lesche stores it opaquely; member access is enforced server-side). 404 =
   * the caller is not a room member. */
  postRoomEnvelope(roomId: string, envelope: Envelope): Promise<void> {
    return this.json(
      `/rooms/${encodeURIComponent(roomId)}/envelopes`,
      "POST",
      envelope,
    );
  }

  /** `GET /rooms/{id}/messages` — the room's message history: one page in
   * one direction. `afterSeq` is exclusive forward (`seq > afterSeq`);
   * `beforeSeq` is exclusive backward (the newest rows with `seq < beforeSeq`,
   * returned ascending). Sending both is a 400 -- one page moves one way.
   * `limit` caps the page. 404 = not a member. */
  fetchRoomMessages(
    roomId: string,
    opts?: { afterSeq?: number; beforeSeq?: number; limit?: number },
  ): Promise<RoomMessageView[]> {
    const params = new URLSearchParams();
    if (opts?.afterSeq !== undefined) {
      params.set("after_seq", String(opts.afterSeq));
    }
    if (opts?.beforeSeq !== undefined) {
      params.set("before_seq", String(opts.beforeSeq));
    }
    if (opts?.limit !== undefined) params.set("limit", String(opts.limit));
    const query = params.toString();
    return this.json(
      `/rooms/${encodeURIComponent(roomId)}/messages${query ? `?${query}` : ""}`,
      "GET",
    );
  }

  /** `GET /me/events` — the multiplexed SSE stream of the user's conversation
   * deliveries plus tagma presence (`tagma_online` / `tagma_offline`, with an
   * initial presence snapshot on connect). A long-lived fetch parsed with the
   * shared `parseSseStream`. Yields {@linkcode MeEventFrame}: the marker
   * (`kind: "stream"`) first, then payload frames carrying their
   * `<epoch>:<seq>` cursor (or nulls when the server sends no id). The caller
   * owns reconnect/backoff; the generator ends when the stream closes or
   * `signal` aborts. */
  async *meEvents(signal?: AbortSignal): AsyncGenerator<MeEventFrame> {
    yield* readEventFrames<LescheEvent>(this.baseUrl + "/me/events", signal);
  }

  // --- room management (relocated from archeion) -------------------------------

  /** `POST /rooms` -- create a room; the caller is the founding member.
   * `name` is required; `description` and `visibility` default to empty and
   * `private` respectively. All three fields are always sent so the server's
   * `Json<CreateRoomRequest>` extractor does not reject the body. */
  createRoom(body: {
    name: string;
    description?: string;
    visibility?: Visibility;
  }): Promise<RoomView> {
    return this.json("/rooms", "POST", {
      name: body.name,
      description: body.description ?? "",
      visibility: body.visibility ?? "private",
    });
  }

  /** `GET /rooms` — the caller's rooms (current membership), newest-joined. */
  listRooms(): Promise<RoomView[]> {
    return this.json("/rooms", "GET");
  }

  /** `GET /rooms/public` -- public (plaintext, open-access) rooms the caller
   * may join without an invite, newest-created. */
  listPublicRooms(): Promise<RoomView[]> {
    return this.json("/rooms/public", "GET");
  }

  /** `POST /rooms/{id}/join` -- join a public room without an invite
   * (open-access). 403 if the room is private (use the invite flow); 204 on a
   * join or an idempotent re-join by an existing member. */
  joinRoom(roomId: string): Promise<void> {
    return this.json(`/rooms/${encodeURIComponent(roomId)}/join`, "POST");
  }

  /** `PUT /rooms/{id}/read-cursor` -- clamp-advance the caller's read
   * watermark in a room (the unread backbone). Member-only; 204 on success
   * (the server fans a room_read_cursor_changed echo to the caller's other
   * live sessions). */
  setRoomReadCursor(roomId: string, lastReadSeq: number): Promise<void> {
    return this.json(
      `/rooms/${encodeURIComponent(roomId)}/read-cursor`,
      "PUT",
      {
        last_read_seq: lastReadSeq,
      },
    );
  }

  /** `GET /rooms/{id}` — a room's live roster (member-only). */
  fetchRoomRoster(roomId: string): Promise<RoomRosterView> {
    return this.json(`/rooms/${encodeURIComponent(roomId)}`, "GET");
  }

  /** `GET /rooms/invites` — the caller's pending invites (the inbox). */
  listMyRoomInvites(): Promise<RoomInviteView[]> {
    return this.json("/rooms/invites", "GET");
  }

  /** `POST /rooms/{id}/invites` — invite a user by @username. 409 if one is
   * already pending. The server strips a leading `@` and resolves the handle. */
  createRoomInvite(
    roomId: string,
    inviteeUsername: string,
  ): Promise<CreateInviteResponse> {
    const body: CreateInviteRequest = { invitee_username: inviteeUsername };
    return this.json(
      `/rooms/${encodeURIComponent(roomId)}/invites`,
      "POST",
      body,
    );
  }

  /** `POST /rooms/{id}/invites/{invite_id}/accept` — accept (invitee-only). */
  acceptRoomInvite(roomId: string, inviteId: string): Promise<void> {
    return this.json(
      `/rooms/${encodeURIComponent(roomId)}/invites/${encodeURIComponent(inviteId)}/accept`,
      "POST",
      undefined,
    );
  }

  /** `DELETE /rooms/{id}/members/{member_id}` — remove a member
   * (self = leave). Keyed by the opaque derived member id (the identifier every
   * room surface already carries). Authorization is server-side: self, the
   * tagma's owner, or the room creator; anything else is a 404. */
  removeRoomMember(roomId: string, memberId: string): Promise<void> {
    return this.json(
      `/rooms/${encodeURIComponent(roomId)}/members/${encodeURIComponent(memberId)}`,
      "DELETE",
      undefined,
    );
  }

  /** `POST /rooms/{id}/tagmata` — add a tagma (idempotent). */
  addRoomTagma(roomId: string, tagmaId: string): Promise<void> {
    const body: AddTagmaRequest = { tagma_id: tagmaId };
    return this.json(
      `/rooms/${encodeURIComponent(roomId)}/tagmata`,
      "POST",
      body,
    );
  }

  /** `GET /me/tagmata/{id}/rooms` — the rooms one of the caller's tagmata
   * has joined (the "Manage rooms" dialog source). The caller must own the
   * tagma (registry-attested server-side). Distinct from the tagma-self
   * discovery route, which the tagma polls from Rust, not from this client. */
  listMyTagmaRooms(tagmaId: string): Promise<TagmaRoomView[]> {
    return this.json(`/me/tagmata/${encodeURIComponent(tagmaId)}/rooms`, "GET");
  }
}
