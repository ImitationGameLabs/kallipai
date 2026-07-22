// Browser clients for the agora suite. The control plane (agora: passkey
// ceremonies, /me, tagma lifecycle, pinned-key fetch) and the data plane
// (lesche: conversations, key exchange, envelopes, app SSE) are separate
// services on separate origins, so they get separate clients sharing a common
// base. Every fetch carries `credentials: "include"` (the session cookie is the
// auth; it is shared cross-subdomain between agora and lesche) and every non-GET
// carries the CSRF marker (`X-Requested-With: kallip`), which both services'
// `csrf_guard` requires on cookie-bearing mutating requests. Non-2xx responses
// become `AgoraApiError` (`{ status, message }`).

import { parseSseStream } from "@kallipai/kallip-common";
import { AgoraApiError } from "./types.ts";
import type {
  AgoraEvent,
  AuthFinishResponse,
  CreateConversationResponse,
  KeyExchangeInit,
  KeyExchangeResponse,
  LoginBeginResponse,
  LoginFinishRequest,
  MeResponse,
  MintTagmaResponse,
  RegisterBeginResponse,
  RegisterFinishRequest,
  RenameTagmaRequest,
  TagmaInfo,
  TagmaView,
  Envelope,
} from "./types.ts";

/** CSRF marker the agora's `csrf_guard` checks (see `session.rs:21-24`). */
export const CSRF_HEADER = "X-Requested-With";
export const CSRF_HEADER_VALUE = "kallip";

/** Request bodies for the ceremony begins. Mirrors the agora DTOs in
 * `crates/kallip-agora/src/routes/auth.rs` (`RegisterBeginRequest`,
 * `LoginBeginRequest`): email is the login id; username is the in-site handle. */
export interface RegisterBeginRequest {
  readonly invite_code: string;
  readonly email: string;
  readonly username: string;
  readonly display_name?: string;
}
export interface LoginBeginRequest {
  readonly email: string;
}

/**
 * Shared base for the agora-suite browser clients (the agora control plane and
 * the lesche data plane): a base URL + the JSON/CSRF fetch helper. Both
 * services accept the session cookie (`credentials: "include"`) and require the
 * `X-Requested-With` CSRF marker on cookie-bearing mutating requests. Non-2xx
 * responses become `AgoraApiError` (`{ status, message }`).
 */
export abstract class BaseClient {
  constructor(protected readonly baseUrl: string) {}

  /** JSON fetch with the CSRF marker on non-GETs; `AgoraApiError` on non-2xx. */
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
    if (!resp.ok) throw await agoraError(resp);
    // 204 No Content (revoke) -- nothing to parse.
    if (resp.status === 204 || resp.headers.get("content-length") === "0") {
      return undefined as T;
    }
    return (await resp.json()) as T;
  }
}

/**
 * Control-plane client (the agora service, default :7100): passkey ceremonies,
 * `/me`, and the tagma lifecycle. Also exposes `getTagma` — the pinned device
 * key is TOFU from the control plane, even though the key exchange itself runs
 * on the lesche (see {@link LescheClient}).
 */
export class AgoraClient extends BaseClient {
  // -- auth ceremonies ------------------------------------------------------

  registerBegin(body: RegisterBeginRequest): Promise<RegisterBeginResponse> {
    return this.json("/v1/auth/register/begin", "POST", body);
  }

  registerFinish(body: RegisterFinishRequest): Promise<AuthFinishResponse> {
    return this.json("/v1/auth/register/finish", "POST", body);
  }

  loginBegin(body: LoginBeginRequest): Promise<LoginBeginResponse> {
    return this.json("/v1/auth/login/begin", "POST", body);
  }

  loginFinish(body: LoginFinishRequest): Promise<AuthFinishResponse> {
    return this.json("/v1/auth/login/finish", "POST", body);
  }

  logout(): Promise<void> {
    return this.json("/v1/auth/logout", "POST", undefined);
  }

  // -- profile --------------------------------------------------------------

  me(): Promise<MeResponse> {
    return this.json("/v1/me", "GET");
  }

  // -- tagmata (unified pending + enrolled lifecycle) -----------------------

  /** `POST /v1/tagmata` — mint a pending tagma (an enrollment code). The
   * plaintext `code` is returned once. */
  mintTagma(): Promise<MintTagmaResponse> {
    return this.json("/v1/tagmata", "POST", {});
  }

  /** `GET /v1/tagmata` — the caller's tagmata (pending + enrolled, not revoked),
   * newest first. Registry view only; liveness is NOT included (it arrives via
   * `meEvents`). */
  listTagmata(): Promise<TagmaView[]> {
    return this.json("/v1/tagmata", "GET");
  }

  /** `PATCH /v1/tagmata/{id}` — set or clear the label (pending or enrolled).
   * Returns on 204. */
  renameTagma(id: string, label: string | null): Promise<void> {
    const body: RenameTagmaRequest = { label };
    return this.json(`/v1/tagmata/${encodeURIComponent(id)}`, "PATCH", body);
  }

  /** `DELETE /v1/tagmata/{id}` — revoke (pending or enrolled). For an enrolled
   * tagma the agora cuts the herald off on its next request. Returns on 204. */
  revokeTagma(id: string): Promise<void> {
    return this.json(
      `/v1/tagmata/${encodeURIComponent(id)}`,
      "DELETE",
      undefined,
    );
  }

  /** `GET /v1/tagmata/{id}` — the tagma's pinned Ed25519 device key (TOFU). The
   * app verifies the lesche's key-exchange signature against it. */
  getTagma(id: string): Promise<TagmaInfo> {
    return this.json(`/v1/tagmata/${encodeURIComponent(id)}`, "GET");
  }
}

/**
 * Data-plane client (the lesche service, default :7200): conversation setup,
 * the synchronous key exchange, envelope posting, and the multiplexed app SSE.
 * The session cookie is shared cross-subdomain with the agora
 * (`KALLIP_AGORA_SESSION_COOKIE_DOMAIN`), so the same credentialed fetch works.
 */
export class LescheClient extends BaseClient {
  /** `POST /v1/conversations { tagma_id }` — resolve the single conversation a
   * tagma owns with its operator (idempotent). */
  createConversation(tagmaId: string): Promise<CreateConversationResponse> {
    return this.json("/v1/conversations", "POST", { tagma_id: tagmaId });
  }

  /** `POST /v1/conversations/{id}/key-exchange/init` — synchronous request/reply
   * returning the herald's signed key-exchange response inline (200). 503 = the
   * tagma is offline, 409 = a key exchange is already in flight, 504 = timed
   * out. */
  keyExchangeInit(
    conversationId: string,
    init: KeyExchangeInit,
  ): Promise<KeyExchangeResponse> {
    return this.json(
      `/v1/conversations/${encodeURIComponent(conversationId)}/key-exchange/init`,
      "POST",
      init,
    );
  }

  /** `POST /v1/conversations/{id}/envelopes` — route an encrypted envelope to
   * the other endpoint. Returns on 202 Accepted. 503 = the peer is offline,
   * 409 = stale/duplicate sequence_n. */
  postEnvelope(conversationId: string, envelope: Envelope): Promise<void> {
    return this.json(
      `/v1/conversations/${encodeURIComponent(conversationId)}/envelopes`,
      "POST",
      envelope,
    );
  }

  /** `GET /v1/me/events` — the multiplexed SSE stream of the user's conversation
   * deliveries plus tagma presence (`tagma_online` / `tagma_offline`, with an
   * initial presence snapshot on connect). A long-lived fetch parsed with the
   * shared `parseSseStream`; each `data:` payload is an `AgoraEvent`. The
   * caller owns reconnect/backoff; the generator ends when the stream closes
   * or `signal` aborts. */
  async *meEvents(signal?: AbortSignal): AsyncGenerator<AgoraEvent> {
    const resp = await fetch(this.baseUrl + "/v1/me/events", {
      method: "GET",
      headers: { accept: "text/event-stream" },
      credentials: "include",
      signal,
    });
    if (!resp.ok) {
      throw await agoraError(resp);
    }
    for await (const ev of parseSseStream(resp, signal)) {
      yield JSON.parse(ev.data) as AgoraEvent;
    }
  }
}

/** Build an `AgoraApiError` from a non-2xx response: the agora's `ApiError` body
 * is `{ status, message }`; fall back to `statusText` for a non-JSON body (e.g. a
 * 403 from the CSRF guard). */
async function agoraError(resp: Response): Promise<AgoraApiError> {
  let message = resp.statusText;
  try {
    const errorBody = (await resp.json()) as { message?: string };
    if (errorBody.message) message = errorBody.message;
  } catch {
    // Non-JSON error body; keep statusText.
  }
  return new AgoraApiError(resp.status, message);
}
