// Browser client for the keryx data-plane relay. The keryx (default :7210) is
// the bilateral data plane: conversation setup, the synchronous key exchange,
// envelope posting, and the bilateral `me/events` SSE (the conversation
// family plus tagma presence/status/signal and file deliveries). The session
// cookie is shared cross-subdomain with the archeion
// (`KALLIPAI_ARCHEION_SESSION_COOKIE_DOMAIN`), so the same credentialed fetch
// works. Every call is credentialed (`credentials: "include"`: the session
// cookie is the auth) and every state-changing one carries the CSRF marker
// (`X-Requested-With: kallipai`), which the keryx's `csrf_guard` requires on
// cookie-bearing mutating requests. The JSON/CSRF fetch discipline and the SSE
// frame machine live in @kallipai/kallipai-common, shared with the lesche
// client: this plane has no transport of its own, so the verbs call
// `dataPlaneRequest` and the stream verb calls `eventFrames`. A non-2xx answer
// arrives as a `DataPlaneApiError` (`{ status, message }`).

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
  CreateConversationResponse,
  Envelope,
  KeyExchangeInit,
  KeyExchangeResponse,
  KeryxEvent,
} from "./types.ts";

/** The keryx `/me/events` frame: the bilateral-family stream. */
export type KeryxEventFrame = EventFrame<KeryxEvent>;

// -- the verbs ----------------------------------------------------------------

/** `POST /conversations { tagma_id }` -- resolve the single conversation a
 * tagma owns with its operator (idempotent). */
export function createConversation(
  base: string,
  tagmaId: string,
): Effect.Effect<CreateConversationResponse, DataPlaneFailure> {
  return dataPlaneRequest<CreateConversationResponse>({
    baseUrl: base,
    path: "/conversations",
    method: "POST",
    body: { tagma_id: tagmaId },
  });
}

/** `POST /conversations/{id}/key-exchange/init` -- synchronous request/reply
 * returning the responder's signed key-exchange response inline (200). 503 =
 * the tagma is offline, 409 = a key exchange is already in flight, 504 =
 * timed out. */
export function keyExchangeInit(
  base: string,
  conversationId: string,
  init: KeyExchangeInit,
): Effect.Effect<KeyExchangeResponse, DataPlaneFailure> {
  return dataPlaneRequest<KeyExchangeResponse>({
    baseUrl: base,
    path: `/conversations/${encodeURIComponent(conversationId)}/key-exchange/init`,
    method: "POST",
    body: init,
  });
}

/** `POST /conversations/{id}/envelopes` -- route an encrypted envelope to
 * the other endpoint. Returns on 202 Accepted. 503 = the peer is offline,
 * 409 = stale/duplicate sequence_n. */
export function postEnvelope(
  base: string,
  conversationId: string,
  envelope: Envelope,
): Effect.Effect<void, DataPlaneFailure> {
  return dataPlaneRequest<void>({
    baseUrl: base,
    path: `/conversations/${encodeURIComponent(conversationId)}/envelopes`,
    method: "POST",
    body: envelope,
  });
}

// -- the stream ---------------------------------------------------------------

/** `GET /me/events` -- the bilateral family stream: conversation envelopes,
 * tagma presence/status/signal, and file deliveries. Same frame discipline as
 * the lesche twin (`MeEventFrame`): the marker (`kind: "stream"`) first, then
 * payload frames carrying their `<epoch>:<seq>` cursor (or nulls when the
 * server sends no id). A refusal is a `DataPlaneApiError` in the failure
 * channel, a dead body a `TransportError`; the caller owns reconnect/backoff,
 * and the stream ends when the body closes or the consumer stops pulling. */
export function keryxEvents(
  base: string,
): Stream.Stream<KeryxEventFrame, DataPlaneFailure> {
  return eventFrames<KeryxEvent>(base + "/me/events");
}

/**
 * The keryx data plane (the keryx service, default :7210): conversation
 * setup, the synchronous key exchange, envelope posting, and the bilateral
 * `me/events` SSE.
 *
 * The promise face of those verbs, as this package offers it to consumers
 * (`kallipai-ui` is today's): one `runClientEffect` per call and
 * `runClientStream` for the stream. Behaviour lives in the verbs above; a
 * caller that can hold an Effect uses them directly. Do not add behavior here.
 * A rejection is the failure channel itself: `DataPlaneApiError` for a
 * refusal, `TransportError` for a dead exchange.
 */
export class KeryxClient {
  constructor(private readonly base: string) {}

  /** {@linkcode createConversation}. */
  createConversation(tagmaId: string): Promise<CreateConversationResponse> {
    return runClientEffect(createConversation(this.base, tagmaId));
  }

  /** {@linkcode keyExchangeInit}. */
  keyExchangeInit(
    conversationId: string,
    init: KeyExchangeInit,
  ): Promise<KeyExchangeResponse> {
    return runClientEffect(keyExchangeInit(this.base, conversationId, init));
  }

  /** {@linkcode postEnvelope}. */
  postEnvelope(conversationId: string, envelope: Envelope): Promise<void> {
    return runClientEffect(postEnvelope(this.base, conversationId, envelope));
  }

  /** The generator face of {@linkcode keryxEvents}: same frames, same failure
   * channel surfaced as a thrown `DataPlaneApiError`/`TransportError`, and it
   * ends cleanly when `signal` aborts. */
  keryxEvents(signal?: AbortSignal): AsyncGenerator<KeryxEventFrame> {
    return runClientStream(keryxEvents(this.base), signal);
  }
}
