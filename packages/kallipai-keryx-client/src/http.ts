// Browser client for the keryx data-plane relay. The keryx (default :7210) is
// the bilateral data plane: conversation setup, the synchronous key exchange,
// envelope posting, and the bilateral `me/events` SSE (the conversation
// family plus tagma presence/status/signal and file deliveries). The session
// cookie is shared cross-subdomain with the archeion
// (`KALLIPAI_ARCHEION_SESSION_COOKIE_DOMAIN`), so the same credentialed fetch
// works. Every fetch carries `credentials: "include"` (the session cookie is
// the auth) and every non-GET carries the CSRF marker
// (`X-Requested-With: kallipai`), which the keryx's `csrf_guard` requires on
// cookie-bearing mutating requests. Non-2xx responses become
// `DataPlaneApiError` (`{ status, message }`). The JSON/CSRF fetch discipline
// and the SSE frame machine live in @kallipai/kallipai-common, shared with
// the lesche client.

import { BaseClient, readEventFrames } from "@kallipai/kallipai-common";
import type { EventFrame } from "@kallipai/kallipai-common";
import type {
  CreateConversationResponse,
  Envelope,
  KeyExchangeInit,
  KeyExchangeResponse,
  KeryxEvent,
} from "./types.ts";

/** The keryx `/me/events` frame: the bilateral-family stream. */
export type KeryxEventFrame = EventFrame<KeryxEvent>;

/**
 * Data-plane client (the keryx service, default :7210): the bilateral
 * conversation family -- conversation setup, the synchronous key exchange,
 * envelope posting, and the bilateral `me/events` SSE.
 */
export class KeryxClient extends BaseClient {
  /** `POST /conversations { tagma_id }` -- resolve the single conversation a
   * tagma owns with its operator (idempotent). */
  createConversation(tagmaId: string): Promise<CreateConversationResponse> {
    return this.json("/conversations", "POST", { tagma_id: tagmaId });
  }

  /** `POST /conversations/{id}/key-exchange/init` -- synchronous request/reply
   * returning the responder's signed key-exchange response inline (200). 503 =
   * the tagma is offline, 409 = a key exchange is already in flight, 504 =
   * timed out. */
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

  /** `POST /conversations/{id}/envelopes` -- route an encrypted envelope to
   * the other endpoint. Returns on 202 Accepted. 503 = the peer is offline,
   * 409 = stale/duplicate sequence_n. */
  postEnvelope(conversationId: string, envelope: Envelope): Promise<void> {
    return this.json(
      `/conversations/${encodeURIComponent(conversationId)}/envelopes`,
      "POST",
      envelope,
    );
  }

  /** `GET /me/events` -- the bilateral family stream: conversation
   * envelopes, tagma presence/status/signal, and file deliveries. Same frame
   * discipline as the lesche twin (`MeEventFrame`); both delegate to the
   * shared frame machine in kallipai-common. A long-lived fetch parsed with
   * the shared `parseSseStream`. Yields {@linkcode KeryxEventFrame}: the
   * marker (`kind: "stream"`) first, then payload frames carrying their
   * `<epoch>:<seq>` cursor (or nulls when the server sends no id). The caller
   * owns reconnect/backoff; the generator ends when the stream closes or
   * `signal` aborts. */
  async *keryxEvents(signal?: AbortSignal): AsyncGenerator<KeryxEventFrame> {
    yield* readEventFrames<KeryxEvent>(this.baseUrl + "/me/events", signal);
  }
}
