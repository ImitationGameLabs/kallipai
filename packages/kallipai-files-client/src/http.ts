// Files data-plane plumbing: the shared credentialed fetch from
// `kallipai-common` plus this plane's reading of a status line. The files
// service authorizes bearer-first with a `kallipai_session` cookie fallback
// (kallipai-files/src/auth.rs), so the same credentialed fetch the relay
// clients use works unchanged: every request carries `credentials:
// "include"`, and the shared fetch attaches the CSRF marker
// (`X-Requested-With: kallipai`) on state-changing methods. The files service
// checks no marker of its own -- it has no such middleware -- so the header
// rides along for consistency with the planes that do.

import {
  type FetchRequest,
  fetchRequest,
  readApiError,
  TransportError,
  UNREADABLE_BODY_MESSAGE,
} from "@kallipai/kallipai-common";
import { Data, Effect } from "effect";

/**
 * A non-2xx files answer. The shape mirrors the relay data plane's
 * `DataPlaneApiError` -- the same envelope parser fills it -- and the tag
 * stays its own so a logged failure names the plane that refused.
 *
 * `code` carries the envelope's machine-readable reason when the service sent
 * one; it is there for troubleshooting, and no branch reads it.
 */
export class FilesApiError extends Data.TaggedError("FilesApiError")<{
  readonly status: number;
  readonly message: string;
  readonly code?: string;
}> {
  constructor(status: number, message: string, code?: string) {
    super({ status, message, code });
  }
}

/** The failure channel of a files call: the service refused it, or no answer
 * arrived at all. */
export type FilesFailure = FilesApiError | TransportError;

/** A files request: the shared fetch's parts, minus the base and path the
 * verb supplies. */
export type FilesRequest = Omit<FetchRequest, "baseUrl" | "path">;

/**
 * One files call. The shared fetch reports only whether an answer arrived; a
 * non-2xx answer here is a refusal, which this plane carries as data in the
 * failure channel.
 *
 * No window is set unless the caller passes `timeoutMs`: a large upload or
 * download is bounded by its own size rather than a fixed number of seconds,
 * and a call that stalls for good still ends as a dead exchange once the
 * browser gives up on it.
 *
 * `base` is a parameter, not state: the verbs hold no client instance, so an
 * effect value can be built and handed around before anything runs.
 */
export function filesFetch(
  base: string,
  path: string,
  request: FilesRequest,
): Effect.Effect<Response, FilesFailure> {
  return Effect.gen(function* () {
    const response = yield* fetchRequest({
      ...request,
      baseUrl: base,
      path,
      // This line is the whole of this plane's no-window policy: the shared
      // fetch defaults an omitted window to `JSON_REQUEST_TIMEOUT_MS`.
      timeoutMs: request.timeoutMs ?? null,
    });
    if (response.ok) return response;
    // `readApiError` reads the body once and never throws: the status line
    // alone always describes the refusal.
    const api = yield* Effect.promise(() => readApiError(response));
    return yield* Effect.fail(
      new FilesApiError(api.status, api.message, api.code),
    );
  });
}

/**
 * The answer as JSON. A body that is not JSON is a dead exchange rather than a
 * refusal: nothing in the status line explains it.
 */
export function readJson<T>(
  response: Response,
): Effect.Effect<T, TransportError> {
  return Effect.tryPromise({
    try: async () => (await response.json()) as T,
    catch: unreadableBody,
  });
}

/** The answer as bytes, with the same reading of an unreadable body. */
export function readBytes(
  response: Response,
): Effect.Effect<ArrayBuffer, TransportError> {
  return Effect.tryPromise({
    try: () => response.arrayBuffer(),
    catch: unreadableBody,
  });
}

/** A body that arrived but could not be read: not JSON, or truncated. The
 * sentence is the shared one, so every plane words it the same. */
function unreadableBody(cause: unknown): TransportError {
  return new TransportError(UNREADABLE_BODY_MESSAGE, { cause });
}
