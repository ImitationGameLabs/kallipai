// Shared browser fetch discipline for the kallipai plane services (the lesche
// rooms relay, the keryx bilateral relay, and the files transfer plane): the
// credentialed JSON/CSRF fetch every plane builds on, the connect-window SSE
// fetch, and the `/me/events` frame machine. Single-homed here so a plane
// split cannot fork the wire disciplines -- the CSRF marker pair, the 30s
// hard timeouts, and the frame cursor parsing stay identical across clients.

import { Effect } from "effect";

import { parseSseStream } from "./sse.ts";
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
 * covered -- see {@linkcode sseFetch}. */
export const SSE_CONNECT_TIMEOUT_MS = 30_000;

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

/**
 * Fetch a data-plane SSE endpoint with a connect/first-byte timeout. The
 * window covers connection establishment and response headers only: once the
 * response arrives the timer is cleared, because a body-scoped timer would
 * kill every healthy long-lived stream at the window edge and drive a
 * periodic reconnect storm (amplified by idle-sensitive intermediaries that
 * drop quiet streams). The external `signal` -- the caller's stream lifetime,
 * also passed to `parseSseStream` -- is bridged into the internal controller
 * for the fetch itself and unhooked once headers arrive; an already-aborted
 * external signal short-circuits before any request is made.
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
 * The shared `/me/events` frame machine: open the stream, parse the marker,
 * then yield payload frames with their `<epoch>:<seq>` cursor (or nulls when
 * the server sent no id). Both plane clients' event generators delegate
 * here, so the cursor discipline is parsed in exactly one place.
 */
export async function* readEventFrames<E>(
  url: string,
  signal?: AbortSignal,
): AsyncGenerator<EventFrame<E>> {
  const resp = await sseFetch(url, signal);
  if (!resp.ok) {
    throw dataPlaneFailure(apiFailure(await readApiError(resp)));
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
    const event = JSON.parse(ev.data) as E;
    const m = ev.id !== undefined ? /^(\d+):(\d+)$/.exec(ev.id) : null;
    yield m
      ? { kind: "event", epoch: Number(m[1]), seq: Number(m[2]), event }
      : { kind: "event", epoch: null, seq: null, event };
  }
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
  /** Whole-exchange window in milliseconds. Omitted means
   * {@linkcode JSON_REQUEST_TIMEOUT_MS}; `null` sends no window; a value of
   * zero or less aborts before the request leaves, which fails as a
   * `TransportError`. */
  readonly timeoutMs?: number | null;
}

/**
 * The credentialed fetch every plane shares: the session cookie
 * (`credentials: "include"`) is the auth, the `X-Requested-With` CSRF marker
 * rides state-changing methods (anything outside GET, HEAD and OPTIONS), and
 * the window bounds the whole exchange.
 *
 * It answers one question -- did an answer arrive? A response comes back
 * whatever its status, because what a status means belongs to the plane: a
 * JSON client fails a non-2xx as a refusal, a metadata probe reads the same
 * answer as "not there". Only an exchange that produced no answer at all
 * (offline, DNS, TLS, a reset, the window closing) fails, as
 * `TransportError`.
 */
export function fetchRequest(
  request: FetchRequest,
): Effect.Effect<Response, TransportError> {
  const { baseUrl, path, method, body, headers: extra, timeoutMs } = request;
  return Effect.gen(function* () {
    const headers: Record<string, string> = { ...extra };
    if (isStateChanging(method)) headers[CSRF_HEADER] = CSRF_HEADER_VALUE;
    const window =
      timeoutMs === undefined ? JSON_REQUEST_TIMEOUT_MS : timeoutMs;
    // Zero or less means "already over": abort before the request goes out
    // rather than hand `AbortSignal.timeout` a delay it throws on.
    const signal =
      window === null
        ? undefined
        : window > 0
          ? AbortSignal.timeout(window)
          : AbortSignal.abort();
    return yield* Effect.tryPromise({
      try: () =>
        fetch(baseUrl + path, {
          method,
          headers,
          credentials: "include",
          ...(signal ? { signal } : {}),
          ...(body !== undefined ? { body } : {}),
        }),
      catch: transportError,
    });
  });
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
  const { baseUrl, path, method, body, timeoutMs } = request;
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
      body: body === undefined ? undefined : JSON.stringify(body),
      timeoutMs,
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
  request: JsonRequest,
): Effect.Effect<T, DataPlaneFailure> {
  return Effect.mapError(jsonRequest<T>(request), dataPlaneFailure);
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
 * The single Promise boundary. The effect-typed request functions are the
 * clients; this exists because the consumers still `await` plain promises.
 * Every call site is transitional: a consumer that moves to the effect API
 * calls it directly, and the last call site deletes this function. Do not
 * add call sites in new code.
 */
export function runClientEffect<A, E>(effect: Effect.Effect<A, E>): Promise<A> {
  return Effect.runPromise(effect);
}

/**
 * Shared base for the data-plane browser clients (the lesche and keryx
 * clients): a base URL + the JSON/CSRF fetch helper. The session cookie
 * (`credentials: "include"`) is the auth; the `X-Requested-With` CSRF marker
 * is required on cookie-bearing mutating requests. Non-2xx responses become
 * `DataPlaneApiError` (`{ status, message }`).
 *
 * Two production subclasses remain (`KeryxClient`, `LescheClient`); this class
 * and `json` below go when the last of them moves to `dataPlaneRequest`. A
 * test-only probe also extends it, and goes with this class.
 */
export abstract class BaseClient {
  constructor(protected readonly baseUrl: string) {}

  /** JSON fetch with the CSRF marker on state-changing methods; `DataPlaneApiError` on
   * non-2xx, `TransportError` when no response arrived.
   *
   * Transitional: the subclasses still `await` it, so it crosses
   * `runClientEffect`, this package's Promise boundary. A subclass that moves
   * to the effect API calls `dataPlaneRequest` and drops this method; the one
   * that converts last leaves it with no callers, and it goes then. Do not
   * call it from new code. */
  protected json<T>(path: string, method: string, body?: unknown): Promise<T> {
    return runClientEffect(
      dataPlaneRequest<T>({ baseUrl: this.baseUrl, path, method, body }),
    );
  }
}
