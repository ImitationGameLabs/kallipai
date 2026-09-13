// Browser client for the lesche data-plane relay. The lesche (default :7200) is
// the data plane: conversation setup, the synchronous key exchange, envelope
// posting, and the multiplexed app SSE. The session cookie is shared
// cross-subdomain with the archeion (`KALLIP_ARCHEION_SESSION_COOKIE_DOMAIN`), so the
// same credentialed fetch works. Every fetch carries `credentials: "include"`
// (the session cookie is the auth) and every non-GET carries the CSRF marker
// (`X-Requested-With: kallip`), which the lesche's `csrf_guard` requires on
// cookie-bearing mutating requests. Non-2xx responses become `LescheApiError`
// (`{ status, message }`).

import { parseSseStream, readApiError } from "@kallipai/kallip-common";
import { LescheApiError } from "./types.ts";
import type {
  AddTagmaRequest,
  CreateConversationResponse,
  CreateInviteRequest,
  CreateInviteResponse,
  Envelope,
  KeyExchangeInit,
  KeyExchangeResponse,
  LescheEvent,
  RoomInviteView,
  RoomMessageView,
  RoomRosterView,
  RoomView,
  TagmaRoomView,
  Visibility,
} from "./types.ts";

/** CSRF marker the lesche's `csrf_guard` checks. */
const CSRF_HEADER = "X-Requested-With";
const CSRF_HEADER_VALUE = "kallip";

/** Connect/first-byte window for SSE fetches: long enough for a cold TLS
 * handshake through a slow proxy, short enough that a hung connection is
 * abandoned before the user gives up. Body lifetime is explicitly not
 * covered -- see {@linkcode sseFetch}. */
export const SSE_CONNECT_TIMEOUT_MS = 30_000;

/** One frame of the `GET /me/events` stream, shaped for cursor tracking.
 *
 * - `stream`: the open marker — `event: stream` with
 *   `{ epoch, next_seq }` data. `nextSeq` is the seq of the first frame
 *   allocated after the marker capture; frames allocated earlier may still
 *   arrive first (pre-adoption).
 * - `event`: a payload frame. `epoch`/`seq` come from the SSE `id:` field
 *   (`"<epoch>:<seq>"`); both are `null` when the server sent no id
 *   (an older server, or a non-kallip intermediary) — consumers treat that
 *   as unversioned and disable gap detection, degrading to exactly the
 *   pre-cursor behavior.
 */
export type MeEventFrame =
  | { kind: "stream"; epoch: number; nextSeq: number }
  | {
      kind: "event";
      epoch: number | null;
      seq: number | null;
      event: LescheEvent;
    };

/**
 * Fetch a lesche SSE endpoint with a connect/first-byte timeout. The window
 * covers connection establishment and response headers only: once the response
 * arrives the timer is cleared, because a body-scoped timer would kill every
 * healthy long-lived stream at the window edge and drive a periodic reconnect
 * storm (amplified by idle-sensitive intermediaries that drop quiet streams).
 * The external `signal` -- the caller's stream lifetime, also passed to
 * `parseSseStream` -- is bridged into the internal controller for the fetch
 * itself and unhooked once headers arrive; an already-aborted external signal
 * short-circuits before any request is made.
 */
export async function sseFetch(
  url: string,
  signal: AbortSignal | undefined,
  connectTimeoutMs = SSE_CONNECT_TIMEOUT_MS,
): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), connectTimeoutMs);
  const onExternalAbort = () => controller.abort();
  signal?.addEventListener("abort", onExternalAbort);
  if (signal?.aborted) controller.abort();
  try {
    return await fetch(url, {
      method: "GET",
      headers: { accept: "text/event-stream" },
      credentials: "include",
      signal: controller.signal,
    });
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", onExternalAbort);
  }
}
/**
 * Shared base for the lesche browser client: a base URL + the JSON/CSRF fetch
 * helper. The session cookie (`credentials: "include"`) is the auth; the
 * `X-Requested-With` CSRF marker is required on cookie-bearing mutating
 * requests. Non-2xx responses become `LescheApiError` (`{ status, message }`).
 *
 * Internal to this package: the lesche surface has a single client, so the base
 * is not re-exported.
 */
abstract class BaseClient {
  constructor(protected readonly baseUrl: string) {}

  /** JSON fetch with the CSRF marker on non-GETs; `LescheApiError` on non-2xx. */
  protected async json<T>(
    path: string,
    method: string,
    body?: unknown,
  ): Promise<T> {
    const headers: Record<string, string> = { accept: "application/json" };
    // Send the CSRF marker on every non-GET unconditionally: it is required on
    // cookie-bearing mutating requests and harmless otherwise.
    const isStateChanging = method !== "GET";
    if (isStateChanging) {
      headers[CSRF_HEADER] = CSRF_HEADER_VALUE;
      if (body !== undefined) headers["content-type"] = "application/json";
    }
    const resp = await fetch(this.baseUrl + path, {
      method,
      headers,
      credentials: "include",
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    if (!resp.ok) throw await lescheError(resp);
    // 204 No Content -- nothing to parse.
    if (resp.status === 204 || resp.headers.get("content-length") === "0") {
      return undefined as T;
    }
    return (await resp.json()) as T;
  }
}

/**
 * Data-plane client (the lesche service, default :7200): conversation setup,
 * the synchronous key exchange, envelope posting, and the multiplexed app SSE.
 */
export class LescheClient extends BaseClient {
  /** `POST /conversations { tagma_id }` — resolve the single conversation a
   * tagma owns with its operator (idempotent). */
  createConversation(tagmaId: string): Promise<CreateConversationResponse> {
    return this.json("/conversations", "POST", { tagma_id: tagmaId });
  }

  /** `POST /conversations/{id}/key-exchange/init` — synchronous request/reply
   * returning the responder's signed key-exchange response inline (200). 503 = the
   * tagma is offline, 409 = a key exchange is already in flight, 504 = timed
   * out. */
  keyExchangeInit(
    conversationId: string,
    init: KeyExchangeInit,
  ): Promise<KeyExchangeResponse> {
    return this.json(
      `/conversations/${encodeURIComponent(conversationId)}/key-exchange/init`,
      "POST",
      init,
    );
  }

  /** `POST /conversations/{id}/envelopes` — route an encrypted envelope to
   * the other endpoint. Returns on 202 Accepted. 503 = the peer is offline,
   * 409 = stale/duplicate sequence_n. */
  postEnvelope(conversationId: string, envelope: Envelope): Promise<void> {
    return this.json(
      `/conversations/${encodeURIComponent(conversationId)}/envelopes`,
      "POST",
      envelope,
    );
  }

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

  /** `GET /rooms/{id}/messages` — the room's message history. `afterSeq` is
   * exclusive (rows with `seq > afterSeq`); `limit` caps the page. 404 = not a
   * member. */
  fetchRoomMessages(
    roomId: string,
    opts?: { afterSeq?: number; limit?: number },
  ): Promise<RoomMessageView[]> {
    const params = new URLSearchParams();
    if (opts?.afterSeq !== undefined) {
      params.set("after_seq", String(opts.afterSeq));
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
    const resp = await sseFetch(this.baseUrl + "/me/events", signal);
    if (!resp.ok) {
      throw await lescheError(resp);
    }
    for await (const ev of parseSseStream(resp, signal)) {
      if (ev.event === "stream") {
        const { epoch, next_seq } = JSON.parse(ev.data) as {
          epoch: number;
          next_seq: number;
        };
        yield { kind: "stream", epoch, nextSeq: next_seq };
        continue;
      }
      const event = JSON.parse(ev.data) as LescheEvent;
      const m = ev.id !== undefined ? /^(\d+):(\d+)$/.exec(ev.id) : null;
      yield m
        ? { kind: "event", epoch: Number(m[1]), seq: Number(m[2]), event }
        : { kind: "event", epoch: null, seq: null, event };
    }
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

/** Build a `LescheApiError` from a non-2xx response. Envelope parsing is
 * shared (`readApiError`) so the lesche client cannot drift from the
 * `{"error":{"message":...}}` shape the server emits. */
async function lescheError(resp: Response): Promise<LescheApiError> {
  const { status, message } = await readApiError(resp);
  return new LescheApiError(status, message);
}
