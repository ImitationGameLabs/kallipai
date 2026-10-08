// Shared browser fetch discipline for the kallipai data-plane services (the
// lesche rooms relay and the keryx bilateral relay): the credentialed
// JSON/CSRF base both plane clients extend, the connect-window SSE fetch, and
// the `/me/events` frame machine. Single-homed here so a plane split cannot
// fork the wire disciplines -- the CSRF marker pair, the 30s hard timeouts,
// and the frame cursor parsing stay identical across both clients.

import { parseSseStream } from "./sse.ts";
import { DataPlaneApiError, readApiError } from "./errors.ts";

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
    throw await dataPlaneApiError(resp);
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

/**
 * Shared base for the data-plane browser clients (the lesche and keryx
 * clients): a base URL + the JSON/CSRF fetch helper. The session cookie
 * (`credentials: "include"`) is the auth; the `X-Requested-With` CSRF marker
 * is required on cookie-bearing mutating requests. Non-2xx responses become
 * `DataPlaneApiError` (`{ status, message }`).
 */
export abstract class BaseClient {
  constructor(protected readonly baseUrl: string) {}

  /** JSON fetch with the CSRF marker on non-GETs; `DataPlaneApiError` on
   * non-2xx. */
  protected async json<T>(
    path: string,
    method: string,
    body?: unknown,
  ): Promise<T> {
    const headers: Record<string, string> = { accept: "application/json" };
    // Send the CSRF marker on every non-GET unconditionally: it is required
    // on cookie-bearing mutating requests and harmless otherwise.
    const isStateChanging = method !== "GET";
    if (isStateChanging) {
      headers[CSRF_HEADER] = CSRF_HEADER_VALUE;
      if (body !== undefined) headers["content-type"] = "application/json";
    }
    // Hard timeout: a stalled relay must reject the pending open (the chat
    // page then degrades) rather than hang forever. 30s matches the SSE
    // connect timeout already used by the streaming path.
    const resp = await fetch(this.baseUrl + path, {
      method,
      headers,
      credentials: "include",
      signal: AbortSignal.timeout(30_000),
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    if (!resp.ok) throw await dataPlaneApiError(resp);
    // 204 No Content -- nothing to parse.
    if (resp.status === 204 || resp.headers.get("content-length") === "0") {
      return undefined as T;
    }
    return (await resp.json()) as T;
  }
}

/** Build a `DataPlaneApiError` from a non-2xx response. Envelope parsing is
 * shared (`readApiError`) so the plane clients cannot drift from the
 * `{"error":{"message":...}}` shape the services emit. */
async function dataPlaneApiError(resp: Response): Promise<DataPlaneApiError> {
  const { status, message } = await readApiError(resp);
  return new DataPlaneApiError(status, message);
}
