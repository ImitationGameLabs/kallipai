// Shared browser fetch discipline for the kallipai plane services (the lesche
// rooms relay, the keryx bilateral relay, and the files transfer plane): the
// credentialed JSON/CSRF fetch every plane builds on, the connect-window SSE
// fetch, and the `/me/events` frame machine. Single-homed here so a plane
// split cannot fork the wire disciplines -- the CSRF marker pair, the 30s
// hard timeouts, and the frame cursor parsing stay identical across clients.

import { Effect, Stream } from "effect";

import { parseSseStream, type RawSseEvent } from "./sse.ts";
import {
  type ApiError,
  DataPlaneApiError,
  readApiError,
  TransportError,
  UNREADABLE_BODY_MESSAGE,
} from "./errors.ts";

/** CSRF marker the data-plane services' `csrf_guard` checks on cookie-bearing
 * mutating requests. */
export const CSRF_HEADER = "X-Requested-With";
export const CSRF_HEADER_VALUE = "kallipai";

/** Connect/first-byte window for SSE fetches: long enough for a cold TLS
 * handshake through a slow proxy, short enough that a hung connection is
 * abandoned before the user gives up. Body lifetime is explicitly not
 * covered -- see {@linkcode sseRequest}. */
const SSE_CONNECT_TIMEOUT_MS = 30_000;

/** One frame of the `GET /me/events` stream, shaped for cursor tracking.
 *
 * - `stream`: the open marker -- `event: stream` with
 *   `{ epoch, next_seq }` data. `nextSeq` is the seq of the first frame
 *   allocated after the marker capture; frames allocated earlier may still
 *   arrive first (pre-adoption).
 * - `event`: a payload frame. `epoch`/`seq` come from the SSE `id:` field
 *   (`"<epoch>:<seq>"`); both are `null` when the server sent no id
 *   (an older server, or a non-kallipai intermediary) -- consumers treat that
 *   as unversioned and disable gap detection, degrading to exactly the
 *   pre-cursor behavior.
 */
export type EventFrame<E> =
  | { kind: "stream"; epoch: number; nextSeq: number }
  | {
      kind: "event";
      epoch: number | null;
      seq: number | null;
      event: E;
    };

/** The connect window closing before headers arrived: no answer, not a
 * refusal, so it is a `TransportError` like every other dead exchange. */
export const SSE_CONNECT_TIMEOUT_MESSAGE =
  "the stream did not answer within the connect window";

/** What a stream connect states: the credential mode it lives in, plus the
 * window and extra headers it wants. */
export interface SseRequestOptions {
  /** Credential mode for the connect: the cookie planes send 'include', the
   *  bearer-only direct plane sends 'omit'. Required, so a new plane states
   *  which world it lives in instead of inheriting one. */
  readonly credentials: RequestCredentials;
  /** Connect/first-byte window; omitted takes the shared default. */
  readonly connectTimeoutMs?: number;
  /** Extra request headers, e.g. the bearer a token-authenticated stream
   *  needs. Additive over the accept header this seam sets. */
  readonly headers?: Readonly<Record<string, string>>;
}

/**
 * Open a data-plane SSE endpoint. The window covers connection establishment
 * and response headers only, and that is structural rather than timed: the
 * effect completes when the headers arrive, so no timer survives into the
 * body and a quiet stream is never cut off at the window edge (the reconnect
 * storm that would cause is exactly what the window must not do).
 *
 * The answer is not interpreted here -- a non-2xx is a `Response` like any
 * other; {@linkcode eventFrames} maps it onto the plane's failure. Body
 * lifetime belongs to cancellation: interrupting the stream tears it down.
 */
export function sseRequest(
  url: string,
  options: SseRequestOptions,
): Effect.Effect<Response, TransportError> {
  return Effect.timeoutOrElse(
    fetchRequest({
      baseUrl: "",
      path: url,
      method: "GET",
      // The accept header is written last: a plane adds headers to this
      // connect, it does not get to replace the one the stream's contract
      // requires.
      headers: { ...options.headers, accept: "text/event-stream" },
      credentials: options.credentials,
      // The connect window is this effect's own; a whole-exchange window here
      // would cut healthy long-lived streams.
      timeoutMs: null,
    }),
    {
      duration: options.connectTimeoutMs ?? SSE_CONNECT_TIMEOUT_MS,
      orElse: () =>
        Effect.fail(new TransportError(SSE_CONNECT_TIMEOUT_MESSAGE)),
    },
  );
}

/**
 * The shared `/me/events` frame machine as a stream: open the endpoint, fail a
 * refusal as `DataPlaneApiError`, then emit each payload frame with its
 * `<epoch>:<seq>` cursor (or nulls when the server sent no id). A failure --
 * the refusal, a body that died, a frame that is not JSON -- is a value in
 * `E` and ends that attempt; reconnecting stays the caller's decision.
 *
 * The frame parsing and the cursor mapping live here and nowhere else; the
 * Promise boundary in {@linkcode runClientStream} adapts this stream and must
 * not grow a second copy of either.
 */
export function eventFrames<E>(
  url: string,
): Stream.Stream<EventFrame<E>, DataPlaneFailure> {
  return Stream.unwrap(
    Effect.gen(function* () {
      const resp = yield* sseRequest(url, { credentials: "include" });
      if (!resp.ok) {
        const api = yield* Effect.promise(() => readApiError(resp));
        return yield* Effect.fail(dataPlaneFailure(apiFailure(api)));
      }
      // The body outlives every fiber that reads it, so its teardown needs to
      // be owned here: the signal cancels a pull still waiting in
      // `parseSseStream`, and the cancel covers the case where the parser
      // already released its reader, which a released lock does not cancel.
      const body = yield* Effect.acquireRelease(
        Effect.sync(() => ({
          abort: new AbortController(),
          body: resp.body,
        })),
        (held) =>
          Effect.sync(() => {
            held.abort.abort();
            void held.body?.cancel().catch(() => {});
          }),
      );
      return Stream.fromAsyncIterable(
        parseSseStream(resp, body.abort.signal),
        bodyFailure,
      ).pipe(Stream.mapEffect(decodeFrame<E>));
    }),
  );
}

/** A body that stopped mid-stream, or a frame that is not JSON: nothing in a
 * status line explains it, so the exchange counts as dead. */
function bodyFailure(cause: unknown): DataPlaneFailure {
  return new TransportError(UNREADABLE_BODY_MESSAGE, { cause });
}

/** One raw SSE event as the frame the cursors are tracked on. */
function decodeFrame<E>(
  ev: RawSseEvent,
): Effect.Effect<EventFrame<E>, DataPlaneFailure> {
  return Effect.try({
    try: (): EventFrame<E> => {
      if (ev.event === "stream") {
        const { epoch, next_seq } = JSON.parse(ev.data) as {
          epoch: number;
          next_seq: number;
        };
        return { kind: "stream", epoch, nextSeq: next_seq };
      }
      const event = JSON.parse(ev.data) as E;
      const m = ev.id !== undefined ? /^(\d+):(\d+)$/.exec(ev.id) : null;
      return m
        ? { kind: "event", epoch: Number(m[1]), seq: Number(m[2]), event }
        : { kind: "event", epoch: null, seq: null, event };
    },
    catch: bodyFailure,
  });
}

/** One credentialed fetch, before any status interpretation. `baseUrl`
 * carries no trailing slash and `path` starts with one: the two are
 * concatenated as sent. */
export interface FetchRequest {
  readonly baseUrl: string;
  readonly path: string;
  /** Any method name. A method outside GET, HEAD and OPTIONS carries the
   * CSRF marker, the same set the services call state-changing. */
  readonly method: string;
  /** Raw request body, already encoded: this layer never serializes. */
  readonly body?: BodyInit | null;
  /** Extra request headers. Caller headers are additive: the CSRF marker
   * comes from this discipline and a caller cannot turn it off. */
  readonly headers?: Readonly<Record<string, string>>;
  /** Credential mode for the exchange: the cookie planes send 'include', the
   * bearer-only tagma direct path sends 'omit'. Required: a plane states which
   * world it lives in, and no seam makes that choice by leaving a field out. */
  readonly credentials: RequestCredentials;
  /** Whole-exchange window in milliseconds: `null` sends no window, and a
   * value of zero or less aborts before the request leaves, which fails as a
   * `TransportError`. Required: this primitive carries no default, so the
   * policy is written where the plane picks it and no seam inherits it by
   * leaving the field out. */
  readonly timeoutMs: number | null;
}

/**
 * The credentialed fetch every plane shares: the request names its credential
 * mode (`include` for the cookie planes, `omit` for the bearer-only tagma
 * direct path); the `X-Requested-With` CSRF marker rides state-changing methods
 * and the window bounds the whole exchange.
 *
 * It answers one question -- did an answer arrive? A response comes back
 * whatever its status, because what a status means belongs to the plane: a
 * JSON client fails a non-2xx as a refusal, a metadata probe reads the same
 * answer as "not there". Only an exchange that produced no answer at all
 * (offline, DNS, TLS, a reset, the window closing) fails, as
 * `TransportError`. Interrupting the fiber aborts the request in flight.
 */
export function fetchRequest(
  request: FetchRequest,
): Effect.Effect<Response, TransportError> {
  const {
    baseUrl,
    path,
    method,
    body,
    headers: extra,
    credentials,
    timeoutMs,
  } = request;
  return Effect.gen(function* () {
    const headers: Record<string, string> = { ...extra };
    if (isStateChanging(method)) headers[CSRF_HEADER] = CSRF_HEADER_VALUE;
    // Zero or less means "already over": abort before the request goes out
    // rather than hand `AbortSignal.timeout` a delay it throws on.
    const windowSignal =
      timeoutMs === null
        ? undefined
        : timeoutMs > 0
          ? AbortSignal.timeout(timeoutMs)
          : AbortSignal.abort();
    return yield* Effect.tryPromise({
      // Declaring the parameter hands this fiber's interruption to `fetch`:
      // abandoning a call aborts its socket instead of leaving it running.
      try: (interrupted) => {
        const signal = mergeSignals(windowSignal, interrupted);
        return fetch(baseUrl + path, {
          method,
          headers,
          credentials,
          ...(signal ? { signal } : {}),
          ...(body !== undefined ? { body } : {}),
        });
      },
      catch: transportError,
    });
  });
}

/** The fetch signal for one exchange: the caller's window when it asked for
 * one, joined with the fiber's interruption, so either teardown aborts the
 * connection.
 *
 * The join is written out rather than handed to `AbortSignal.any` because
 * that helper sets an engine floor (Chrome 116, Safari 17.4, Firefox 124)
 * and this code ships inside a desktop shell, whose webview follows the
 * user's system rather than a declared browser baseline. The handwritten
 * form has no floor and the same behavior: either source aborting aborts
 * the join with that source's reason, and both listeners come off as soon
 * as one fires. */
function mergeSignals(
  windowSignal: AbortSignal | undefined,
  interrupt: AbortSignal,
): AbortSignal {
  if (windowSignal === undefined) return interrupt;
  const joined = new AbortController();
  const listeners: Array<[AbortSignal, () => void]> = [];
  const detach = () => {
    for (const [signal, listener] of listeners) {
      signal.removeEventListener("abort", listener);
    }
    listeners.length = 0;
  };
  for (const signal of [windowSignal, interrupt]) {
    if (signal.aborted) {
      detach();
      joined.abort(signal.reason);
      return joined.signal;
    }
    const listener = () => {
      detach();
      joined.abort(signal.reason);
    };
    listeners.push([signal, listener]);
    signal.addEventListener("abort", listener, { once: true });
    // A source that aborted between the check above and this registration
    // fires no event, so without a second look the join would stay live and
    // the exchange would outlive the window it was given.
    if (signal.aborted) {
      detach();
      joined.abort(signal.reason);
      return joined.signal;
    }
  }
  return joined.signal;
}

/** The methods the services treat as state-changing: everything outside GET,
 * HEAD and OPTIONS (`is_state_changing` in their middleware). Only these
 * carry the CSRF marker. */
function isStateChanging(method: string): boolean {
  return method !== "GET" && method !== "HEAD" && method !== "OPTIONS";
}

/** The no-answer failure: the reason the exchange gave, as the class the
 * planes share. */
function transportError(cause: unknown): TransportError {
  const reason = cause instanceof Error ? cause.message : String(cause);
  return new TransportError(reason, { cause });
}

/** The failure channel of a JSON request as plain data: the service refused
 * the call, or the exchange failed before an answer arrived. It carries no
 * route; `message` is what user-facing copy renders.
 * The plane packages map this onto their own error classes. */
export type RequestFailure =
  | {
      readonly kind: "transport";
      readonly message: string;
      readonly cause?: unknown;
    }
  | {
      readonly kind: "api";
      readonly status: number;
      readonly message: string;
      readonly code?: string;
      /** The tagma 409's stranded profile-set bindings, when the envelope
       * carried them: the direct client rebuilds its own ApiError from this
       * rather than losing the list at the seam. */
      readonly dangling?: readonly string[];
    };

/** Whole-exchange window for a JSON request: long enough for a cold TLS
 * handshake through a slow proxy, short enough that a stalled relay fails the
 * call instead of hanging the page. */
export const JSON_REQUEST_TIMEOUT_MS = 30_000;

/** One JSON request. `baseUrl` carries no trailing slash and `path` starts
 * with one: the two are concatenated as sent. */
export interface JsonRequest {
  readonly baseUrl: string;
  readonly path: string;
  /** Any method name. A method outside GET, HEAD and OPTIONS carries the
   * CSRF marker, the same set the services call state-changing. */
  readonly method: string;
  readonly body?: unknown;
  /** Extra request headers, e.g. the bearer a token-authenticated route
   * wants. Caller headers are additive: the CSRF marker and the JSON
   * `content-type` come from this discipline and a caller cannot turn them
   * off. */
  readonly headers?: Readonly<Record<string, string>>;
  /** Credential mode for the exchange; see {@linkcode FetchRequest}. */
  readonly credentials: RequestCredentials;
  /** Overrides {@linkcode JSON_REQUEST_TIMEOUT_MS}; `null` sends no window.
   * A window of zero or less aborts before the request leaves, which fails as
   * a `TransportError`. */
  readonly timeoutMs?: number | null;
}

/**
 * The JSON fetch discipline as an Effect: the credentialed JSON/CSRF rules
 * every plane service shares, single-homed so a plane split cannot fork them.
 * The session cookie (`credentials: "include"`) is the auth; the
 * `X-Requested-With` CSRF marker is required on cookie-bearing mutating
 * requests and harmless elsewhere.
 *
 * A non-2xx answer arrives as `{ kind: "api" }` carrying the parsed envelope;
 * a timeout, a refused connection or an unreadable body arrives as
 * `{ kind: "transport" }` with the underlying error kept as `cause`. A 204 or
 * a `content-length: 0` body is a success carrying `undefined`.
 * Cancellation is not a failure value: no caller-supplied signal enters this
 * primitive, and the window it does set is its own. A later caller-supplied
 * signal would arrive as interruption, not in `E`.
 *
 * Which primitive a plane picks: a JSON plane takes this one and maps the
 * failure onto its own classes with one `mapError`. A plane that moves raw
 * bytes (a streamed download, an upload body) needs the response itself, so
 * it calls `fetchRequest` and reads the status line in its own verbs.
 */
export function jsonRequest<T>(
  request: JsonRequest,
): Effect.Effect<T, RequestFailure> {
  const { baseUrl, path, method, body, credentials, timeoutMs } = request;
  return Effect.gen(function* () {
    const headers: Record<string, string> = {
      accept: "application/json",
      ...request.headers,
    };
    if (isStateChanging(method) && body !== undefined) {
      headers["content-type"] = "application/json";
    }
    const resp = yield* fetchRequest({
      baseUrl,
      path,
      method,
      headers,
      credentials,
      body: body === undefined ? undefined : JSON.stringify(body),
      // A JSON exchange's window policy lives here and nowhere else: an
      // omitted window takes the shared default, `null` stays "no window".
      timeoutMs: timeoutMs === undefined ? JSON_REQUEST_TIMEOUT_MS : timeoutMs,
    });
    if (!resp.ok) {
      // `readApiError` reads the body once and never throws: the status line
      // alone always describes the refusal.
      const api = yield* Effect.promise(() => readApiError(resp));
      return yield* Effect.fail(apiFailure(api));
    }
    // 204 No Content -- nothing to parse.
    if (resp.status === 204 || resp.headers.get("content-length") === "0") {
      return undefined as T;
    }
    return yield* Effect.tryPromise({
      try: async () => (await resp.json()) as T,
      catch: decodeFailure,
    });
  }).pipe(
    // A dead exchange arrives from the fetch layer as the class the planes
    // share; this function's public failure form is the data above.
    Effect.catchTag("TransportError", (failure) =>
      Effect.fail(requestTransportFailure(failure)),
    ),
  );
}

/** The failure channel of a data-plane request: a structured answer from the
 * service, or the transport failing before one arrived. */
export type DataPlaneFailure = DataPlaneApiError | TransportError;

/** {@linkcode jsonRequest} for the data plane: the same exchange, with the
 * plane's error classes in the failure channel. */
export function dataPlaneRequest<T>(
  request: Omit<JsonRequest, "credentials">,
): Effect.Effect<T, DataPlaneFailure> {
  // The relay planes all ride the session cookie, so the credential mode is
  // written once here: a verb cannot name another, and a cookie-less plane
  // calls `jsonRequest` with its own value instead of passing through here.
  return Effect.mapError(
    jsonRequest<T>({ ...request, credentials: "include" }),
    dataPlaneFailure,
  );
}

/** Map the plane-neutral failure onto the data plane's error classes. Kept in
 * one place so another plane adds its own mapping instead of a branch here. */
function dataPlaneFailure(failure: RequestFailure): DataPlaneFailure {
  return failure.kind === "api"
    ? new DataPlaneApiError(failure.status, failure.message, failure.code)
    : new TransportError(failure.message, { cause: failure.cause });
}

/** The `{ kind: "api" }` data for a non-2xx response, from `readApiError`.
 * Envelope parsing itself stays in errors.ts: one reader for every client. */
function apiFailure(api: ApiError): RequestFailure {
  return {
    kind: "api",
    status: api.status,
    message: api.message,
    code: api.code,
    // Present only when the envelope carried it: an error without the list
    // keeps the shape callers already branch on.
    ...(api.dangling ? { dangling: api.dangling } : {}),
  };
}

/** A dead exchange in the plane-neutral data form: callers that branch on
 * `kind` do not need the class. */
function requestTransportFailure(failure: TransportError): RequestFailure {
  return { kind: "transport", message: failure.message, cause: failure.cause };
}

/** A response arrived but its body could not be read as JSON: it was not JSON,
 * or the window closed mid-body. The sentence is shared with every plane that
 * decodes an answer itself. */
function decodeFailure(cause: unknown): RequestFailure {
  return {
    kind: "transport",
    message: UNREADABLE_BODY_MESSAGE,
    cause,
  };
}

/**
 * The one place an Effect becomes a Promise: a verb, or a request function
 * in this module, builds the effect and the caller hands it here to await.
 * Nothing above this line holds the effect itself.
 *
 * `signal` is the caller's lifetime: aborting it interrupts the effect and
 * rejects with the signal's own reason.
 */
export function runClientEffect<A, E>(
  effect: Effect.Effect<A, E>,
  signal?: AbortSignal,
): Promise<A> {
  if (signal === undefined) return Effect.runPromise(effect);
  if (signal.aborted) return Promise.reject(abortReason(signal));
  return Effect.runPromise(Effect.raceFirst(effect, abortEffect(signal)));
}

/**
 * The one place a stream becomes an AsyncGenerator: a stream verb builds
 * the stream and the caller hands it here to iterate. Nothing above this
 * line holds the stream itself.
 *
 * A typed failure reaches the iterator as that error itself, not a wrapper,
 * so `instanceof` branches in the consumers keep working. The caller's signal
 * ending the stream is a clean end: an abort mid-body ends the loop without
 * throwing.
 */
export async function* runClientStream<A, E>(
  stream: Stream.Stream<A, E>,
  signal?: AbortSignal,
): AsyncGenerator<A> {
  const iterator: AsyncIterator<A> =
    Stream.toAsyncIterable(stream)[Symbol.asyncIterator]();
  // Closing can reject out of the stream's own finalizers, and nothing here
  // awaits that promise: drop the rejection instead of leaving it unhandled.
  const close = () => {
    void iterator.return?.(undefined)?.catch(() => {});
  };
  if (signal?.aborted) close();
  signal?.addEventListener("abort", close, { once: true });
  try {
    while (true) {
      const next = await iterator.next();
      if (next.done) return;
      yield next.value;
    }
  } finally {
    signal?.removeEventListener("abort", close);
    // A consumer stopping early (a `break`, or `iterator.return()`) closes the
    // stream's scope here, which is what cancels the body reader.
    close();
  }
}

/** The error a caller's abort rejects with: the signal's own reason, matching
 * what an aborting fetch has always produced. */
function abortReason(signal: AbortSignal): unknown {
  return (
    signal.reason ?? new DOMException("the operation was aborted", "AbortError")
  );
}

/** An effect that never completes on its own and dies with the caller's abort
 * reason, so `raceFirst` can interrupt the request it races. */
function abortEffect(signal: AbortSignal): Effect.Effect<never> {
  return Effect.callback<never>((resume) => {
    const onAbort = () => {
      resume(Effect.die(abortReason(signal)));
    };
    signal.addEventListener("abort", onAbort, { once: true });
    return Effect.sync(() => {
      signal.removeEventListener("abort", onAbort);
    });
  });
}
