// The lesche data plane's HTTP verbs, as effects, plus the promise face the
// UI awaits. One verb per route, plus the `me/events` stream
// (rooms and direct sessions). The lesche (default :7200) is the rooms data
// plane: room envelopes, room history, room management, and the lesche
// `me/events` SSE.
// The bilateral conversations, the key exchange, and the keryx `me/events`
// stream live in @kallipai/kallipai-keryx-client.
//
// The exchange itself lives in @kallipai/kallipai-common: this plane has no
// transport of its own, so the verbs call `dataPlaneRequest` and the stream
// verb calls `eventFrames`. Every call is credentialed (`credentials:
// "include"`: the session cookie is the auth) and every state-changing one
// carries the CSRF marker, which the lesche's `csrf_guard` requires on
// cookie-bearing mutating requests. A non-2xx is a `DataPlaneApiError`
// (`{ status, message }`) in the failure channel; a dead exchange is a
// `TransportError`.

import {
  type DataPlaneFailure,
  dataPlaneRequest,
  eventFrames,
  type EventFrame,
  runClientEffect,
  runClientStream,
} from "@kallipai/kallipai-common";
import { Effect, type Stream } from "effect";

import type {
  AddTagmaRequest,
  CreateInviteRequest,
  CreateInviteResponse,
  Envelope,
  LescheEvent,
  RoomInviteView,
  RoomMessageView,
  RoomRosterView,
  RoomUnreadView,
  RoomView,
  TagmaRoomView,
  ThreadChoice,
  ThreadSubscriptionRequest,
  ThreadSubscriptionView,
  Visibility,
} from "./types.ts";

/** The lesche `/me/events` frame: the room-family stream. */
export type MeEventFrame = EventFrame<LescheEvent>;

// -- rooms (multi-member data plane) -----------------------------------------

/** `POST /rooms/{id}/envelopes` — store + fan a room envelope to the
 * room's other live members. Returns on 202; offline members pull the row via
 * `fetchRoomMessages`. The payload is the plaintext `RoomMessage` JSON (the
 * lesche stores it opaquely; member access is enforced server-side). 404 =
 * the caller is not a room member. */
export function postRoomEnvelope(
  base: string,
  roomId: string,
  envelope: Envelope,
): Effect.Effect<void, DataPlaneFailure> {
  return dataPlaneRequest<void>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/envelopes`,
    method: "POST",
    body: envelope,
  });
}

/** `GET /rooms/{id}/messages` — the room's message history: one page in
 * one direction. `afterSeq` is exclusive forward (`seq > afterSeq`);
 * `beforeSeq` is exclusive backward (the newest rows with `seq < beforeSeq`,
 * returned ascending). Sending both is a 400 -- one page moves one way.
 * `thread` narrows the page to one discussion: the head row at that seq plus
 * every reply naming it. It reads forward only, so the relay rejects it next
 * to `beforeSeq`.
 * `limit` caps the page. 404 = not a member. */
export function fetchRoomMessages(
  base: string,
  roomId: string,
  opts?: {
    afterSeq?: number;
    beforeSeq?: number;
    thread?: number;
    limit?: number;
  },
): Effect.Effect<RoomMessageView[], DataPlaneFailure> {
  const params = new URLSearchParams();
  if (opts?.afterSeq !== undefined) {
    params.set("after_seq", String(opts.afterSeq));
  }
  if (opts?.beforeSeq !== undefined) {
    params.set("before_seq", String(opts.beforeSeq));
  }
  if (opts?.thread !== undefined) params.set("thread", String(opts.thread));
  if (opts?.limit !== undefined) params.set("limit", String(opts.limit));
  const query = params.toString();
  return dataPlaneRequest<RoomMessageView[]>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/messages${
      query ? `?${query}` : ""
    }`,
    method: "GET",
  });
}

/** `POST /rooms` -- create a room; the caller is the founding member.
 * `name` is required; `description` and `visibility` default to empty and
 * `private` respectively. All three fields are always sent so the server's
 * `Json<CreateRoomRequest>` extractor does not reject the body. */
export function createRoom(
  base: string,
  body: {
    name: string;
    description?: string;
    visibility?: Visibility;
  },
): Effect.Effect<RoomView, DataPlaneFailure> {
  return dataPlaneRequest<RoomView>({
    baseUrl: base,
    path: "/rooms",
    method: "POST",
    body: {
      name: body.name,
      description: body.description ?? "",
      visibility: body.visibility ?? "private",
    },
  });
}

/** `GET /rooms` — the caller's rooms (current membership), newest-joined. */
export function listRooms(
  base: string,
): Effect.Effect<RoomView[], DataPlaneFailure> {
  return dataPlaneRequest<RoomView[]>({
    baseUrl: base,
    path: "/rooms",
    method: "GET",
  });
}

/** `GET /rooms/public` -- public (plaintext, open-access) rooms the caller
 * may join without an invite, newest-created. */
export function listPublicRooms(
  base: string,
): Effect.Effect<RoomView[], DataPlaneFailure> {
  return dataPlaneRequest<RoomView[]>({
    baseUrl: base,
    path: "/rooms/public",
    method: "GET",
  });
}

/** `POST /rooms/{id}/join` -- join a public room without an invite
 * (open-access). 403 if the room is private (use the invite flow); 204 on a
 * join or an idempotent re-join by an existing member. */
export function joinRoom(
  base: string,
  roomId: string,
): Effect.Effect<void, DataPlaneFailure> {
  return dataPlaneRequest<void>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/join`,
    method: "POST",
  });
}

// -- the stream ---------------------------------------------------------------

/** `GET /me/events` — the multiplexed stream of envelope deliveries for
 * rooms and direct sessions, room membership changes and peer presence, the
 * caller's read-cursor echoes, plus tagma presence (`tagma_online` /
 * `tagma_offline`, with an initial snapshot on connect) and operator
 * status/signals. A long-lived fetch parsed with the
 * shared frame machine: the marker (`kind: "stream"`) first, then payload
 * frames carrying their `<epoch>:<seq>` cursor (or nulls when the server sends
 * no id). A refusal is a `DataPlaneApiError` in the failure channel, a dead
 * body a `TransportError`; the caller owns reconnect/backoff, and the stream
 * ends when the body closes or the consumer stops pulling. */
export function roomEvents(
  base: string,
): Stream.Stream<MeEventFrame, DataPlaneFailure> {
  return eventFrames<LescheEvent>(base + "/me/events");
}

// -- room management (relocated from archeion) --------------------------------

/** `PUT /rooms/{id}/read-cursor` -- clamp-advance the caller's read
 * watermark in a room (the unread backbone). Member-only; 204 on success
 * (the server fans a room_read_cursor_changed echo to the caller's other
 * live sessions). */
export function setRoomReadCursor(
  base: string,
  roomId: string,
  lastReadSeq: number,
): Effect.Effect<void, DataPlaneFailure> {
  return dataPlaneRequest<void>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/read-cursor`,
    method: "PUT",
    body: {
      last_read_seq: lastReadSeq,
    },
  });
}

/** `GET /rooms/{id}/unread` -- the caller's unread badge in a room, with the
 * read cursor it was counted against. Member-only. */
export function fetchRoomUnread(
  base: string,
  roomId: string,
): Effect.Effect<RoomUnreadView, DataPlaneFailure> {
  return dataPlaneRequest<RoomUnreadView>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/unread`,
    method: "GET",
  });
}

/** `GET /rooms/{id}/threads/{root}/subscription` -- the caller's standing in
 * one thread: the choice they hold, the rule delivery applies, and the room
 * position that rule starts after. Member-only, and `{root}` must head a
 * thread of this room: a non-member, a room that does not exist, and a
 * position that is no thread's head are all one 404. */
export function fetchThreadSubscription(
  base: string,
  roomId: string,
  root: number,
): Effect.Effect<ThreadSubscriptionView, DataPlaneFailure> {
  return dataPlaneRequest<ThreadSubscriptionView>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/threads/${root}/subscription`,
    method: "GET",
  });
}

/** `PUT /rooms/{id}/threads/{root}/subscription` -- record the caller's
 * choice in one thread. The member is the authenticated caller, so the body
 * names only the choice; a write is idempotent and answers the standing it
 * left (a repeated write keeps the position the first one took). */
export function setThreadSubscription(
  base: string,
  roomId: string,
  root: number,
  state: ThreadChoice,
): Effect.Effect<ThreadSubscriptionView, DataPlaneFailure> {
  const body: ThreadSubscriptionRequest = { state };
  return dataPlaneRequest<ThreadSubscriptionView>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/threads/${root}/subscription`,
    method: "PUT",
    body,
  });
}

/** `DELETE /rooms/{id}/threads/{root}/subscription` -- drop the caller's
 * choice and take the thread's default. 204; holding no row is not an
 * error. */
export function clearThreadSubscription(
  base: string,
  roomId: string,
  root: number,
): Effect.Effect<void, DataPlaneFailure> {
  return dataPlaneRequest<void>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/threads/${root}/subscription`,
    method: "DELETE",
  });
}

/** `GET /rooms/{id}` — a room's live roster (member-only). */
export function fetchRoomRoster(
  base: string,
  roomId: string,
): Effect.Effect<RoomRosterView, DataPlaneFailure> {
  return dataPlaneRequest<RoomRosterView>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}`,
    method: "GET",
  });
}

/** `GET /rooms/invites` — the caller's pending invites (the inbox). */
export function listMyRoomInvites(
  base: string,
): Effect.Effect<RoomInviteView[], DataPlaneFailure> {
  return dataPlaneRequest<RoomInviteView[]>({
    baseUrl: base,
    path: "/rooms/invites",
    method: "GET",
  });
}

/** `POST /rooms/{id}/invites` — invite a user by @username. 409 if one is
 * already pending. The server strips a leading `@` and resolves the handle. */
export function createRoomInvite(
  base: string,
  roomId: string,
  inviteeUsername: string,
): Effect.Effect<CreateInviteResponse, DataPlaneFailure> {
  const body: CreateInviteRequest = { invitee_username: inviteeUsername };
  return dataPlaneRequest<CreateInviteResponse>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/invites`,
    method: "POST",
    body,
  });
}

/** `POST /rooms/{id}/invites/{invite_id}/accept` — accept (invitee-only). */
export function acceptRoomInvite(
  base: string,
  roomId: string,
  inviteId: string,
): Effect.Effect<void, DataPlaneFailure> {
  return dataPlaneRequest<void>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/invites/${encodeURIComponent(
      inviteId,
    )}/accept`,
    method: "POST",
  });
}

/** `DELETE /rooms/{id}/members/{member_id}` — remove a member
 * (self = leave). Keyed by the opaque derived member id (the identifier every
 * room surface already carries). Authorization is server-side: self, the
 * tagma's owner, or the room creator; anything else is a 404. */
export function removeRoomMember(
  base: string,
  roomId: string,
  memberId: string,
): Effect.Effect<void, DataPlaneFailure> {
  return dataPlaneRequest<void>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/members/${encodeURIComponent(
      memberId,
    )}`,
    method: "DELETE",
  });
}

/** `POST /rooms/{id}/tagmata` — add a tagma (idempotent). */
export function addRoomTagma(
  base: string,
  roomId: string,
  tagmaId: string,
): Effect.Effect<void, DataPlaneFailure> {
  const body: AddTagmaRequest = { tagma_id: tagmaId };
  return dataPlaneRequest<void>({
    baseUrl: base,
    path: `/rooms/${encodeURIComponent(roomId)}/tagmata`,
    method: "POST",
    body,
  });
}

/** `GET /me/tagmata/{id}/rooms` — the rooms one of the caller's tagmata
 * has joined (the "Manage rooms" dialog source). The caller must own the
 * tagma (registry-attested server-side). Distinct from the tagma-self
 * discovery route, which the tagma polls from Rust, not from this client. */
export function listMyTagmaRooms(
  base: string,
  tagmaId: string,
): Effect.Effect<TagmaRoomView[], DataPlaneFailure> {
  return dataPlaneRequest<TagmaRoomView[]>({
    baseUrl: base,
    path: `/me/tagmata/${encodeURIComponent(tagmaId)}/rooms`,
    method: "GET",
  });
}

/**
 * Data-plane client (the lesche service, default :7200): the rooms family --
 * room envelopes, room history, room management, and the lesche `me/events`
 * SSE.
 *
 * The promise face of the verbs above, as this package offers it to consumers
 * (`kallipai-ui` is today's): one `runClientEffect` per call, and
 * `runClientStream` for the stream. Behaviour lives in the verbs; a caller
 * that can hold an Effect uses them directly. Do not add behavior here. A
 * rejection is the failure channel itself: `DataPlaneApiError` for a refusal,
 * `TransportError` for a dead exchange.
 */
export class LescheClient {
  private readonly base: string;

  constructor(baseUrl: string) {
    // Trim a trailing slash so `${base}${path}` stays single-slash.
    this.base = baseUrl.replace(/\/+$/, "");
  }

  postRoomEnvelope(roomId: string, envelope: Envelope): Promise<void> {
    return runClientEffect(postRoomEnvelope(this.base, roomId, envelope));
  }

  fetchRoomMessages(
    roomId: string,
    opts?: {
      afterSeq?: number;
      beforeSeq?: number;
      thread?: number;
      limit?: number;
    },
  ): Promise<RoomMessageView[]> {
    return runClientEffect(fetchRoomMessages(this.base, roomId, opts));
  }

  /** The generator face of {@linkcode roomEvents}: same frames, same failure
   * channel surfaced as a thrown `DataPlaneApiError`/`TransportError`, and it
   * ends cleanly when `signal` aborts. */
  meEvents(signal?: AbortSignal): AsyncGenerator<MeEventFrame> {
    return runClientStream(roomEvents(this.base), signal);
  }

  createRoom(body: {
    name: string;
    description?: string;
    visibility?: Visibility;
  }): Promise<RoomView> {
    return runClientEffect(createRoom(this.base, body));
  }

  listRooms(): Promise<RoomView[]> {
    return runClientEffect(listRooms(this.base));
  }

  listPublicRooms(): Promise<RoomView[]> {
    return runClientEffect(listPublicRooms(this.base));
  }

  joinRoom(roomId: string): Promise<void> {
    return runClientEffect(joinRoom(this.base, roomId));
  }

  setRoomReadCursor(roomId: string, lastReadSeq: number): Promise<void> {
    return runClientEffect(setRoomReadCursor(this.base, roomId, lastReadSeq));
  }

  fetchRoomUnread(roomId: string): Promise<RoomUnreadView> {
    return runClientEffect(fetchRoomUnread(this.base, roomId));
  }

  fetchThreadSubscription(
    roomId: string,
    root: number,
  ): Promise<ThreadSubscriptionView> {
    return runClientEffect(fetchThreadSubscription(this.base, roomId, root));
  }

  setThreadSubscription(
    roomId: string,
    root: number,
    state: ThreadChoice,
  ): Promise<ThreadSubscriptionView> {
    return runClientEffect(
      setThreadSubscription(this.base, roomId, root, state),
    );
  }

  clearThreadSubscription(roomId: string, root: number): Promise<void> {
    return runClientEffect(clearThreadSubscription(this.base, roomId, root));
  }

  fetchRoomRoster(roomId: string): Promise<RoomRosterView> {
    return runClientEffect(fetchRoomRoster(this.base, roomId));
  }

  listMyRoomInvites(): Promise<RoomInviteView[]> {
    return runClientEffect(listMyRoomInvites(this.base));
  }

  createRoomInvite(
    roomId: string,
    inviteeUsername: string,
  ): Promise<CreateInviteResponse> {
    return runClientEffect(
      createRoomInvite(this.base, roomId, inviteeUsername),
    );
  }

  acceptRoomInvite(roomId: string, inviteId: string): Promise<void> {
    return runClientEffect(acceptRoomInvite(this.base, roomId, inviteId));
  }

  removeRoomMember(roomId: string, memberId: string): Promise<void> {
    return runClientEffect(removeRoomMember(this.base, roomId, memberId));
  }

  addRoomTagma(roomId: string, tagmaId: string): Promise<void> {
    return runClientEffect(addRoomTagma(this.base, roomId, tagmaId));
  }

  listMyTagmaRooms(tagmaId: string): Promise<TagmaRoomView[]> {
    return runClientEffect(listMyTagmaRooms(this.base, tagmaId));
  }
}
