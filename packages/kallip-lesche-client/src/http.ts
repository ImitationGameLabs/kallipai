// Browser client for the lesche data-plane relay. The lesche (default :7200) is
// the data plane: conversation setup, the synchronous key exchange, envelope
// posting, and the multiplexed app SSE. The session cookie is shared
// cross-subdomain with the agora (`KALLIP_AGORA_SESSION_COOKIE_DOMAIN`), so the
// same credentialed fetch works. Every fetch carries `credentials: "include"`
// (the session cookie is the auth) and every non-GET carries the CSRF marker
// (`X-Requested-With: kallip`), which the lesche's `csrf_guard` requires on
// cookie-bearing mutating requests. Non-2xx responses become `LescheApiError`
// (`{ status, message }`).

import { parseSseStream } from "@kallipai/kallip-common";
import { LescheApiError } from "./types.ts";
import type {
  CreateConversationResponse,
  Envelope,
  KeyExchangeInit,
  KeyExchangeResponse,
  LescheEvent,
} from "./types.ts";

/** CSRF marker the lesche's `csrf_guard` checks. */
const CSRF_HEADER = "X-Requested-With";
const CSRF_HEADER_VALUE = "kallip";

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
  /** `POST /v1/conversations { tagma_id }` — resolve the single conversation a
   * tagma owns with its operator (idempotent). */
  createConversation(tagmaId: string): Promise<CreateConversationResponse> {
    return this.json("/v1/conversations", "POST", { tagma_id: tagmaId });
  }

  /** `POST /v1/conversations/{id}/key-exchange/init` — synchronous request/reply
   * returning the responder's signed key-exchange response inline (200). 503 = the
   * tagma is offline, 409 = a key exchange is already in flight, 504 = timed
   * out. */
  keyExchangeInit(
    conversationId: string,
    init: KeyExchangeInit,
  ): Promise<KeyExchangeResponse> {
    return this.json(
      `/v1/conversations/${encodeURIComponent(
        conversationId,
      )}/key-exchange/init`,
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
   * shared `parseSseStream`; each `data:` payload is a `LescheEvent`. The
   * caller owns reconnect/backoff; the generator ends when the stream closes
   * or `signal` aborts. */
  async *meEvents(signal?: AbortSignal): AsyncGenerator<LescheEvent> {
    const resp = await fetch(this.baseUrl + "/v1/me/events", {
      method: "GET",
      headers: { accept: "text/event-stream" },
      credentials: "include",
      signal,
    });
    if (!resp.ok) {
      throw await lescheError(resp);
    }
    for await (const ev of parseSseStream(resp, signal)) {
      yield JSON.parse(ev.data) as LescheEvent;
    }
  }
}

/** Build a `LescheApiError` from a non-2xx response: the lesche's `ApiError`
 * body is `{ status, message }`; fall back to `statusText` for a non-JSON body
 * (e.g. a 403 from the CSRF guard). */
async function lescheError(resp: Response): Promise<LescheApiError> {
  let message = resp.statusText;
  try {
    const errorBody = (await resp.json()) as { message?: string };
    if (errorBody.message) message = errorBody.message;
  } catch {
    // Non-JSON error body; keep statusText.
  }
  return new LescheApiError(resp.status, message);
}
