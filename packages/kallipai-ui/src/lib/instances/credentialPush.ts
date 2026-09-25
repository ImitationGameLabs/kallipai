// One-click provider-credential push: after a successful cloud spawn, send
// the vault-selected API key to the new tagma over a management session and
// verify it connects. The pure decision logic lives here so the polling
// window and wire assembly stay unit-testable; the page owns the transport
// (channel construction) and passes it in as ports.
//
// Failure contract (three terminal states, nothing silent):
//  - "pushed":      PUT succeeded; probeOk reports the connect check.
//  - "unreachable": the tagma never became manageable within the window --
//                   nothing was pushed; the user configures the profiles page.
//  - "failed":      the tagma rejected the credential (4xx class) -- no point
//                   retrying; the raw message is carried for the banner.

import { KallipaiError, TransportError } from "@kallipai/kallipai-common";
import type {
  ProfileApplyResponse,
  ProfileConfig,
  ProfileConfigPutRequest,
  ProfileProbeRequest,
  ProfileProbeResponse,
} from "@kallipai/kallipai-client";

/** How long we keep trying after spawn returns. A fresh instance must boot,
 * enroll via its minted code, and appear at lesche before any manage request
 * can connect; the default is deliberately generous (the operator-recorded
 * enroll-time distribution lives in the batch-3 e2e notes) and only bounds
 * this background push, not the UI. */
export const ENROLL_PUSH_WINDOW_MS = 120_000;

/** Fixed retry cadence while waiting for enrollment. */
export const ENROLL_PUSH_INTERVAL_MS = 2_000;

/**
 * The profiles-endpoint key a one-click instance's credentials are stored
 * under. The slug is our own "tagma-<8hex>" form, so the composite stays
 * within [a-z0-9:-] -- safe for the TOML round trip and never colliding with
 * the reserved "unconfigured" sentinel id.
 */
export function providerEndpointKey(instanceSlug: string): string {
  return `provider:${instanceSlug}`;
}

/** The subset of an archeion ProviderSummary the selection and push need. */
export interface PushCandidate {
  readonly id: string;
  readonly name: string;
  /** Vault family ("deepseek", "openai-compatible", ...). */
  readonly family: string;
  readonly baseUrl: string | null;
  readonly mode: "plaintext" | "encrypted";
}

/**
 * An encrypted vault row is selectable only in a passkey-arrived session:
 * on another device's passkey the blob is undecryptable here anyway, and on
 * an OAuth-only session we fail closed rather than dragging device-key
 * material into the spawn path (mirrors the vault's canFlipKeys gate).
 */
export function isLocked(
  entry: Pick<PushCandidate, "mode">,
  sessionViaPasskey: boolean,
): boolean {
  return entry.mode === "encrypted" && !sessionViaPasskey;
}

/**
 * The PUT response echoes every endpoint with its api_key masked. Asserting
 * OUR key arrived means finding a mask shape rather than a null keep -- null
 * means our endpoint was dropped, not stored. mask_key masks short keys as
 * eight bare stars (no tail), so both shapes count as stored.
 */
export function putEchoedOurKey(
  returned: ProfileConfig,
  endpointKey: string,
  apiKey: string,
): boolean {
  const echoed = returned.endpoints[endpointKey]?.api_key;
  if (typeof echoed !== "string" || echoed.length === 0) return false;
  if (echoed === "********") return true;
  const tail = apiKey.slice(-4);
  return echoed.endsWith(tail) && echoed.includes("*");
}

/** True when apply reached at least one live agent (the delivery bar). */
export function applyReachedAgent(response: ProfileApplyResponse): boolean {
  return response.applied >= 1;
}

export type PushOutcome =
  | {
      state: "pushed";
      endpointKey: string;
      probeOk: boolean;
      applied: number;
    }
  | { state: "unreachable"; endpointKey: string }
  | { state: "failed"; endpointKey: string; message: string };

export interface PushPorts {
  fetchLive(): Promise<ProfileConfig>;
  put(body: ProfileConfigPutRequest): Promise<unknown>;
  probe(body: ProfileProbeRequest): Promise<ProfileProbeResponse>;
  apply(): Promise<ProfileApplyResponse>;
  now(): number;
  sleep(ms: number): Promise<void>;
  /** Release transport resources once the outcome is decided. */
  close?(): void | Promise<void>;
}

/** The fields handed to {@link pushCredentials}; the key has already been
 * decrypted by the caller and lives only for this call. */
export interface PushTarget {
  readonly endpointKey: string;
  readonly family: string;
  readonly apiKey: string;
  readonly baseUrl: string | null;
  /** The model the picked credential powers (operator-entered; the vault
   *  does not carry model names). */
  readonly model: string;
  /** Set-profile context window, kept at the backend's placeholder constant. */
  readonly maxContextWindow?: number;
}

/**
 * Assemble the additive PUT body: every live set/parking row round-trips
 * unchanged, existing endpoints come back with tri-state nulls (keep), and
 * exactly one endpoint carries the real credential plus a binding in the
 * first set. On a fresh instance that set is the only one, so it resolves
 * as the default and the next apply reaches the root agent; on a live
 * config the binding lands in the wire-first set, and the agents bound to
 * that set are the ones the credential reaches.
 */
export function buildPushConfig(
  live: ProfileConfig,
  add: PushTarget,
): ProfileConfigPutRequest {
  const binding = {
    id: `profile:${add.endpointKey}`,
    endpoint: add.endpointKey,
    model: add.model,
    max_context_window: add.maxContextWindow ?? 128_000,
  };
  // A re-push replaces its prior binding instead of appending a duplicate
  // (the registry rejects duplicate profile ids wholesale).
  const sets = Object.entries(live.sets).map(([name, set]) => ({
    name,
    description: set.description,
    profiles: set.profiles
      .filter((p) => p.endpoint !== add.endpointKey)
      .map((p) => ({ ...p })),
  }));
  if (sets.length === 0) {
    // Fresh instance: live store is empty; ours becomes the sole set
    // (single-set configs resolve to it as the default server-side).
    sets.push({ name: "default", description: null, profiles: [binding] });
  } else {
    // Append as failover to the first set (sorted order = the wire
    // order); never dethrone the operator-tuned active[0].
    sets[0]!.profiles.push(binding);
  }
  return {
    sets,
    endpoints: Object.fromEntries([
      ...Object.entries(live.endpoints).map(([key, ep]) => [
        key,
        // null/null = "keep" per the wire tri-state; masked round-tripping
        // would also work but forces us to echo secrets-shaped strings.
        { ...ep, api_key: null, base_url: null },
      ]),
      [
        add.endpointKey,
        {
          id: add.endpointKey,
          family: add.family,
          api_key: add.apiKey,
          base_url: add.baseUrl,
        },
      ],
    ]),
    parking: [...(live.parking ?? [])],
  };
}

/**
 * The verification request: probe just our endpoint inline (api_key null
 * resolves to the definition we just PUT). Set refs stay empty -- probe
 * validates counts only, so an endpoint without referencing profiles is
 * fine.
 */
export function probeRequestFor(add: PushTarget): ProfileProbeRequest {
  return {
    endpoints: [
      {
        id: add.endpointKey,
        family: add.family,
        api_key: null,
        base_url: add.baseUrl,
      },
    ],
    sets: [],
  };
}

/** True when the probe report marks our endpoint ok. */
export function probeVerdict(
  response: ProfileProbeResponse,
  endpointKey: string,
): boolean {
  return (
    response.results.find((r) => r.endpoint_id === endpointKey)?.status === "ok"
  );
}

/**
 * Classify a push failure: transport-class noise and server-side trouble
 * are worth re-attempting inside the window (enrollment may simply not be
 * done yet); a structured 4xx means the credential or request itself was
 * rejected and retrying verbatim cannot succeed.
 */
export function pushErrorKind(e: unknown): "retry" | "terminal" {
  if (e instanceof TransportError) return "retry";
  if (e instanceof KallipaiError) {
    const s = e.api.status;
    return s >= 500 || s === 409 || s === 429 ? "retry" : "terminal";
  }
  // Unknown throw shape (crypto, channel teardown): treat as transient.
  return "retry";
}

function failureMessage(e: unknown): string {
  if (e instanceof KallipaiError && e.api.message) return e.api.message;
  if (e instanceof Error && e.message) return e.message;
  return String(e);
}

/**
 * Run the push loop to a terminal state. Attempt one fires immediately
 * (a fast enroll beats polling); retries wait out the fixed interval and
 * the whole loop folds at the deadline into "unreachable". The single
 * post-push probe never fails the push (no rollback).
 */
export async function pushCredentials(
  target: PushTarget,
  ports: PushPorts,
): Promise<PushOutcome> {
  const deadline = ports.now() + ENROLL_PUSH_WINDOW_MS;
  try {
    for (;;) {
      try {
        const live = await ports.fetchLive();
        const returned = await ports.put(buildPushConfig(live, target));
        // Masked echo with our key's tail = the tagma actually stored it
        // (a null keep here would mean the endpoint never landed). A PUT
        // whose echo lost our endpoint is folded to unreachable via retry:
        // it repeats once, then the deadline ends the loop honestly.
        const stored =
          returned &&
          putEchoedOurKey(
            returned as ProfileConfig,
            target.endpointKey,
            target.apiKey,
          );
        if (!stored) {
          throw new TransportError("PUT echo did not confirm the endpoint");
        }
        let probeOk = false;
        try {
          const response = await ports.probe(probeRequestFor(target));
          probeOk = probeVerdict(response, target.endpointKey);
        } catch {
          // Connect check failed after a good push: surface probeOk=false,
          // keep the credential in place (rollback could strand the tagma
          // profile-less over what might be a provider-side blip).
        }
        let applied = 0;
        try {
          const applyResp = await ports.apply();
          applied = applyResp.applied;
        } catch {
          // Apply hiccup does not unsend the stored credential; the UI
          // reads applied===0 as 'stored but not live -- hand-apply'.
        }
        return {
          state: "pushed",
          endpointKey: target.endpointKey,
          probeOk,
          applied,
        };
      } catch (e) {
        if (pushErrorKind(e) === "terminal") {
          return {
            state: "failed",
            endpointKey: target.endpointKey,
            message: failureMessage(e),
          };
        }
      }
      if (ports.now() + ENROLL_PUSH_INTERVAL_MS >= deadline) {
        return { state: "unreachable", endpointKey: target.endpointKey };
      }
      await ports.sleep(ENROLL_PUSH_INTERVAL_MS);
    }
  } finally {
    await ports.close?.();
  }
}
