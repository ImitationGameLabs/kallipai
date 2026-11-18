// Browser client for the archeion control-plane relay. The archeion (default :7100)
// is the control plane: passkey ceremonies, `/me`, and the tagma lifecycle. It
// shares a session cookie cross-subdomain (archeion/lesche/keryx), so every
// fetch carries `credentials: "include"` (the session cookie is the auth) and
// every state-changing request carries the CSRF marker
// (`X-Requested-With: kallipai`), which the archeion's `csrf_guard` requires on
// cookie-bearing mutating requests. Non-2xx responses become `ArcheionApiError`
// (`{ status, message }`). The data-plane client lives in
// @kallipai/kallipai-keryx-client (bilateral) / -lesche-client (rooms).

import {
  type JsonRequest,
  jsonRequest,
  type RequestFailure,
  TransportError,
} from "@kallipai/kallipai-common";
import { Effect } from "effect";

import { ArcheionApiError } from "./types.ts";

/** The failure channel of an archeion call: the service refused it, or no
 * answer arrived at all. */
export type ArcheionFailure = ArcheionApiError | TransportError;

/** An archeion request: the shared JSON request's parts, minus the base and
 * path the verb supplies. `timeoutMs` and `credentials` are the two fields
 * this plane writes once: omitted `timeoutMs` becomes no window (see
 * `archeionRequest`), and the credential mode is the session cookie's, so a
 * number is the only way to bound the exchange. */
export type ArcheionRequest = Omit<
  JsonRequest,
  "baseUrl" | "path" | "credentials"
>;

/**
 * One archeion call: the shared JSON/CSRF exchange, with this plane's error
 * classes in the failure channel. A non-2xx answer is a refusal carrying the
 * envelope's `status` and `message`; an exchange that produced no answer, or
 * whose body could not be read, is a dead exchange.
 *
 * `base` is a parameter, not state: the verbs hold no client instance, so an
 * effect value can be built and handed around before anything runs.
 */
export function archeionRequest<T>(
  base: string,
  path: string,
  request: ArcheionRequest,
): Effect.Effect<T, ArcheionFailure> {
  const window: number | null = request.timeoutMs ?? null;
  return Effect.mapError(
    jsonRequest<T>({
      ...request,
      baseUrl: base,
      path,
      // The archeion shares its session cookie cross-subdomain, so this plane
      // is a cookie plane; the field is written here, once, and the derived
      // request type keeps verbs from naming another.
      credentials: "include",
      // This line is the whole of this plane's no-window policy: `jsonRequest`
      // names its window at the seam, so an archeion call has to say `null`
      // explicitly instead of inheriting a default by omission. The typed
      // binding is what makes dropping the `?? null` a compile error.
      timeoutMs: window,
    }),
    archeionFailure,
  );
}

/** Map the plane-neutral failure onto this plane's error classes, the same
 * mapping `dataPlaneRequest` makes for the relay planes. */
function archeionFailure(failure: RequestFailure): ArcheionFailure {
  return failure.kind === "api"
    ? new ArcheionApiError(failure.status, failure.message, failure.code)
    : new TransportError(failure.message, { cause: failure.cause });
}
