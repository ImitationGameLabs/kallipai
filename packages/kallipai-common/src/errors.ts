// Error taxonomy. Three classes, one per thing a caller can learn:
//
// - KallipaiError is the control-plane refusal: archeion and the tagma manage
//   routes answered with the {"error":{"message":...}} envelope. The HTTP
//   status rides the response line, not the JSON body, so it is carried
//   alongside the message.
//
// - DataPlaneApiError is the relay data plane's refusal: lesche rooms and
//   keryx bilateral messages send the same envelope, and their callers route
//   the failure through the realtime/channel stores.
//
// - TransportError is no answer at all, on any plane: the exchange died
//   before the service could speak (network drop, unreadable body, crypto or
//   replay-window rejection, key exchange timing out).
//
// Every class carries a `_tag`; branch sites compare it. Callers that still
// branch on `instanceof` keep working, and this sentence goes away once no
// branch site uses it.

import { Data } from "effect";

export interface ApiError {
  // HTTP status from the response line (not serialized in the JSON body).
  readonly status: number;
  // Message parsed from the {"error":{"message":...}} envelope.
  readonly message: string;
  // Stranded profile-set bindings, present only on the tagma 409 that
  // `PUT /profiles` returns for a non-forced dangling save.
  readonly dangling?: readonly string[];
  // Machine-readable reason from the envelope's `error.code`, present when
  // the status line alone would let a caller draw the wrong conclusion.
  readonly code?: string;
}

/** Tagma/archeion control-plane failure: the service answered, and the answer
 * is the structured `{"error":...}` envelope it sends for refusals. */
export class KallipaiError extends Data.TaggedError("KallipaiError")<{
  readonly api: ApiError;
  readonly message: string;
}> {
  constructor(api: ApiError) {
    super({ api, message: api.message });
  }
}

/** No answer arrived, whatever plane the request was bound for: the exchange
 * ended before the service could speak (network drop, window closing,
 * unreadable body, crypto or replay rejection). `message` is the reason the
 * exchange gave, or one fixed sentence when the answer arrived but could not
 * be read; either way it carries no route, so user-facing copy renders it as
 * is. */
export class TransportError extends Data.TaggedError("TransportError")<{
  readonly message: string;
  readonly cause?: unknown;
}> {
  constructor(message: string, options?: { readonly cause?: unknown }) {
    super({ message, cause: options?.cause });
  }
}

/** The one sentence a plane gives when an answer arrived but could not be
 * read (not JSON, truncated, or cut off mid-body). The exchange counts as
 * dead rather than refused: nothing in the status line explains it. */
export const UNREADABLE_BODY_MESSAGE = "the response body could not be read";

/** Parse the `{"error":{"message","dangling"}}` envelope shared by every
 * client path: HTTP responses (`readApiError`) and relay manage_result
 * bodies (`OnlineBackend`) must agree on what the server sent, so the
 * extraction lives here instead of once per transport. */
export function parseErrorEnvelope(body: unknown): {
  dangling?: readonly string[];
  code?: string;
  message?: string;
} {
  const envelope = body as {
    error?: { message?: string; dangling?: readonly string[]; code?: string };
  };
  const dangling = Array.isArray(envelope?.error?.dangling)
    ? envelope.error.dangling
    : undefined;
  return {
    message: envelope?.error?.message,
    dangling,
    code: envelope?.error?.code,
  };
}

/** Read an `ApiError` from a non-2xx `Response`. Shared by every HTTP client so
 * the `{"error":{"message":"..."}}` envelope is parsed in one place. The status
 * rides the response line; the message falls back from the envelope to a
 * plain-text body (e.g. a CSRF-guard 403 string), then to `statusText`. The
 * body is read ONCE as text (then parsed), because `.json()` would consume the
 * stream and leave a following `.text()` empty. */
export async function readApiError(resp: Response): Promise<ApiError> {
  let message = resp.statusText;
  let dangling: readonly string[] | undefined;
  let code: string | undefined;
  try {
    const text = await resp.text();
    if (text) {
      try {
        const {
          message: msg,
          dangling: d,
          code: c,
        } = parseErrorEnvelope(JSON.parse(text));
        if (msg) message = msg;
        else message = text;
        dangling = d;
        code = c;
      } catch {
        // Non-JSON body (e.g. a CSRF-guard plain string): use it verbatim.
        message = text;
      }
    }
  } catch {
    // Empty or unreadable body: keep statusText.
  }
  return { status: resp.status, message, dangling, code };
}

/**
 * Data-plane API error, thrown by the lesche and keryx browser clients on
 * a non-2xx response: the parsed body as an `Error` with the HTTP status
 * carried alongside. Both services emit the same
 * `{"error":{"message":...}}` envelope, so one class covers both planes
 * and callers match the tag once regardless of which relay rejected
 * the call. Mirrors
 * `kallipai_common::protocol::ApiError`, and stays distinct from the
 * no-answer failure above -- a data-plane error carries the service's own
 * refusal, and routes through the realtime/channels stores rather than a
 * transport classifier.
 *
 * `code` carries the envelope's machine-readable reason when the relay sent
 * one. The consumers that read a status as a fact about the peer need it: a
 * `503 auth_backend_unavailable` is the relay's own authentication backend
 * failing, not the peer being offline, and the two must not collapse into
 * the same guess.
 */
export class DataPlaneApiError extends Data.TaggedError("DataPlaneApiError")<{
  readonly status: number;
  readonly message: string;
  readonly code?: string;
}> {
  constructor(status: number, message: string, code?: string) {
    super({ status, message, code });
  }
}
