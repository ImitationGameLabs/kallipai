// Error taxonomy. Two distinct failure families, kept apart on purpose:
//
// - KallipError wraps an ApiError and represents a structured tagma/agora error
//   (the {"error":{"message":...}} envelope). The HTTP status rides the response
//   line, not the JSON body, so it is carried alongside the message.
//
// - TransportError covers everything that is NOT an ApiError: network drops,
//   decode failures, crypto failures, replay-window violations, key-exchange
//   timeouts. Callers can branch on `instanceof` to tell them apart.

export interface ApiError {
  // HTTP status from the response line (not serialized in the JSON body).
  readonly status: number;
  // Message parsed from the {"error":{"message":...}} envelope.
  readonly message: string;
}

export class KallipError extends Error {
  readonly api: ApiError;

  constructor(api: ApiError) {
    super(api.message);
    this.name = "KallipError";
    this.api = api;
  }
}

export class TransportError extends Error {
  constructor(message: string, options?: { readonly cause?: unknown }) {
    super(message, options);
    this.name = "TransportError";
  }
}

/** Read an `ApiError` from a non-2xx `Response`. Shared by every HTTP client so
 * the `{"error":{"message":"..."}}` envelope is parsed in one place. The status
 * rides the response line; the message falls back from the envelope to a
 * plain-text body (e.g. a CSRF-guard 403 string), then to `statusText`. The
 * body is read ONCE as text (then parsed), because `.json()` would consume the
 * stream and leave a following `.text()` empty. */
export async function readApiError(resp: Response): Promise<ApiError> {
  let message = resp.statusText;
  try {
    const text = await resp.text();
    if (text) {
      try {
        const body = JSON.parse(text) as { error?: { message?: string } };
        if (body?.error?.message) message = body.error.message;
        else message = text;
      } catch {
        // Non-JSON body (e.g. a CSRF-guard plain string): use it verbatim.
        message = text;
      }
    }
  } catch {
    // Empty or unreadable body: keep statusText.
  }
  return { status: resp.status, message };
}
