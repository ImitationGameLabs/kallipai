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
