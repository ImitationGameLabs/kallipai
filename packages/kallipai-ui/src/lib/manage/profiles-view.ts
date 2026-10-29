// Pure view helpers extracted from ProfilesPage: probe-report merging into
// keyed maps, draft-derived id lists, probe status labels/colors (i18n via
// paraglide — same-layer precedent as compute.ts), and the parked-live
// snapshot core. The page keeps the $state/$derived/store plumbing; these
// take the maps/values as parameters so tests can drive them directly
// (profiles-view_test.ts).

import type {
  AgentStatusResponse,
  ProfileConfig,
  ProfileModelProbeReport,
  ProfileProbeResponse,
  ProfileProbeStatus,
  ProfileProviderProbeReport,
} from "@kallipai/kallipai-client";
import { KallipaiError } from "@kallipai/kallipai-common";
import {
  manage_profiles_probe_advice_invalid,
  manage_profiles_probe_advice_ok_endpoint,
  manage_profiles_probe_advice_ok_profile,
  manage_profiles_probe_advice_partial,
  manage_profiles_probe_advice_unreachable,
  manage_profiles_probe_advice_unauthorized,
  manage_profiles_probe_advice_upstream_error,
  manage_profiles_probe_advice_upstream_not_found,
  manage_profiles_probe_advice_upstream_rate_limited,
  manage_profiles_probe_advice_upstream_unexpected,
  manage_profiles_probe_models_one,
  manage_profiles_probe_models_other,
  manage_profiles_probe_status_invalid,
  manage_profiles_probe_status_ok,
  manage_profiles_probe_status_partial,
  manage_profiles_probe_status_unauthorized,
  manage_profiles_probe_status_unreachable,
  manage_profiles_probe_status_upstream,
} from "../../paraglide/messages.js";

/** `${setName}:${profileId}` — profile ids can repeat across sets. */
export function profileKey(setName: string, profileId: string): string {
  return `${setName}:${profileId}`;
}

/** Merge a probe response's provider-scope results into the page-held map. */
export function mergeProviderScope(
  reports: Map<string, ProfileProviderProbeReport>,
  resp: ProfileProbeResponse,
): void {
  for (const r of resp.results) reports.set(r.endpoint_id, r);
}

/** Merge a set-scoped response: sets[0] is the requested set. */
export function mergeProfileScope(
  setName: string,
  reports: Map<string, ProfileModelProbeReport>,
  resp: ProfileProbeResponse,
): void {
  const s = resp.sets[0];
  if (!s) return;
  for (const p of s.profiles) {
    reports.set(profileKey(setName, p.profile_id), p);
  }
}

/**
 * Merge an all-scope response: each reported set carries its name, so the
 * reports map keys straight off it — set name + profile id.
 */
export function mergeProfileScopeAll(
  reports: Map<string, ProfileModelProbeReport>,
  resp: ProfileProbeResponse,
): void {
  for (const s of resp.sets) {
    for (const p of s.profiles) {
      reports.set(profileKey(s.name, p.profile_id), p);
    }
  }
}

/** Drop the stored report for one profile (set-scoped key). */
export function clearProfileResult(
  reports: Map<string, ProfileModelProbeReport>,
  setName: string,
  profileId: string,
): void {
  reports.delete(profileKey(setName, profileId));
}

/** What a probe call leaves its caller: a reply to render, a failure the
 * store already turned into probeError, or nothing at all -- a newer probe
 * or a dialog opener superseded this call, or the draft offered no request
 * to send. */
export type ProbeOutcome =
  | { status: "landed"; response: ProfileProbeResponse }
  | { status: "failed" }
  | { status: "abandoned" };

/** The inline report a profile dialog renders under its form. */
export type InlineProbeReport = {
  status: ProfileProbeStatus;
  detail: string | null;
  modelsHint?: "not_listed" | null;
};

/** What a dialog keeps after one probe call: a landed reply replaces
 * the report, a failure clears it, and an abandoned call leaves the
 * report alone -- that dialog may already show a newer verdict. */
export function reportAfterProbe(
  outcome: ProbeOutcome,
  current: InlineProbeReport | null,
): InlineProbeReport | null {
  switch (outcome.status) {
    case "landed": {
      const p = outcome.response.sets[0]?.profiles[0];
      if (!p) return null;
      return {
        status: p.status,
        detail: p.detail ?? null,
        modelsHint: p.models_hint ?? null,
      };
    }
    case "failed":
      return null;
    case "abandoned":
      return current;
  }
}

/** Endpoint (provider) ids of the draft config. */
export function providerIdsOf(
  draft: ProfileConfig | null | undefined,
): string[] {
  return Object.keys(draft?.endpoints ?? {});
}

/**
 * Every profile id visible in the draft — sets ∪ unassigned (the unassigned
 * dialog's new-mode duplicate check; advisory only, PUT is authoritative).
 */
export function occupiedIdsOf(
  draft: ProfileConfig | null | undefined,
): string[] {
  const ids = Object.values(draft?.sets ?? {}).flatMap((s) =>
    s.profiles.map((p) => p.id),
  );
  return [...ids, ...(draft?.parking ?? []).map((p) => p.id)];
}

/** The six keys the color table is keyed by: the object arm carries no
 * string value of its own, so it normalizes to its wire tag. */
export type ProbeStatusKey =
  | "ok"
  | "unreachable"
  | "unauthorized"
  | "invalid_config"
  | "partial"
  | "upstream_status";

/** Which face a verdict came from: the provider card reads the model list
 * (zero-cost), the profile card and dialog send one billed call. The
 * advice differs, so the caller states the face. */
export type ProbeFace = "endpoint" | "profile";

export function probeStatusKey(s: ProfileProbeStatus): ProbeStatusKey {
  return typeof s === "string" ? s : "upstream_status";
}

export function probeStatusLabel(s: ProfileProbeStatus): string {
  if (typeof s !== "string") {
    return manage_profiles_probe_status_upstream({
      status: s.upstream_status.status,
    });
  }
  switch (s) {
    case "ok":
      return manage_profiles_probe_status_ok();
    case "partial":
      return manage_profiles_probe_status_partial();
    case "unreachable":
      return manage_profiles_probe_status_unreachable();
    case "unauthorized":
      return manage_profiles_probe_status_unauthorized();
    case "invalid_config":
      return manage_profiles_probe_status_invalid();
  }
}

const PROBE_STATUS_COLORS: Record<ProbeStatusKey, string> = {
  ok: "text-success-500 dark:text-success-400",
  partial: "text-warning-500 dark:text-warning-400",
  unreachable: "text-error-500 dark:text-error-400",
  unauthorized: "text-error-500 dark:text-error-400",
  invalid_config: "text-error-500 dark:text-error-400",
  upstream_status: "text-error-500 dark:text-error-400",
};

/** The color a verdict renders in: by key, so the object arm takes the
 * failure color. */
export function probeStatusColor(s: ProfileProbeStatus): string {
  return PROBE_STATUS_COLORS[probeStatusKey(s)];
}

/** One line of advice under a verdict, by verdict and by face: the endpoint
 * face reads the model list (nothing inference-shaped is sent), the profile
 * face sends one non-streaming 256-token call. */
export function probeStatusAdvice(
  s: ProfileProbeStatus,
  face: ProbeFace,
): string {
  if (typeof s !== "string") {
    const status = s.upstream_status.status;
    if (status === 404) {
      return manage_profiles_probe_advice_upstream_not_found();
    }
    if (status === 429) {
      return manage_profiles_probe_advice_upstream_rate_limited();
    }
    if (status >= 500) {
      return manage_profiles_probe_advice_upstream_error({ status });
    }
    return manage_profiles_probe_advice_upstream_unexpected({ status });
  }
  switch (s) {
    case "ok":
      return face === "endpoint"
        ? manage_profiles_probe_advice_ok_endpoint()
        : manage_profiles_probe_advice_ok_profile();
    case "partial":
      return manage_profiles_probe_advice_partial();
    case "unreachable":
      return manage_profiles_probe_advice_unreachable();
    case "unauthorized":
      return manage_profiles_probe_advice_unauthorized();
    case "invalid_config":
      return manage_profiles_probe_advice_invalid();
  }
}

export function modelsCountLabel(count: number): string {
  return count === 1
    ? manage_profiles_probe_models_one({ count })
    : manage_profiles_probe_models_other({ count });
}

/**
 * Unassigned-live snapshot core: which unassigned ids some live agent still runs.
 * Takes the settled per-agent statuses (the page fetches them; per-agent
 * failures arrive as rejections and are skipped — one 409/404 must not void
 * the advisory snapshot). Null = nothing unassigned-live.
 */
export function unassignedLiveSnapshot(
  unassignedIds: string[],
  statuses: PromiseSettledResult<AgentStatusResponse>[],
): { agentCount: number; profileIds: string[] } | null {
  const unassigned = new Set(unassignedIds);
  let agentCount = 0;
  const profileIds = new Set<string>();
  for (const s of statuses) {
    if (s.status !== "fulfilled") continue;
    const pid = s.value.profile?.profile_id;
    if (pid && unassigned.has(pid)) {
      agentCount++;
      profileIds.add(pid);
    }
  }
  return agentCount > 0 ? { agentCount, profileIds: [...profileIds] } : null;
}

// --- save-failure classification and leave-guard decisions ----
// The store's catch block and the host wrapper's guard are thin layers
// over these; the decision table lives here so tests drive it directly.

/** What the save flow should do with a thrown save failure. */
export type SaveFailureKind = "park-dangling" | "stale-backend" | "error";

export function classifySaveFailure(
  e: unknown,
  force: boolean,
): SaveFailureKind {
  if (e instanceof KallipaiError && e.api.dangling) return "park-dangling";
  // Old backend: force is ignored, so a dangling save keeps failing with
  // a bare 409 and no structured list.
  if (force && e instanceof KallipaiError && e.api.status === 409) {
    return "stale-backend";
  }
  return "error";
}

/** Should an in-flight navigation be intercepted by the unsaved guard? */
export function leaveGuardIntercept(
  isDirty: boolean,
  guardOpen: boolean,
): boolean {
  return isDirty && !guardOpen;
}

/** A parked dangling-409: the stranded set names plus which send
 * parked it (the confirm dialog re-sends by the kind). */
export type PendingDangling = {
  names: readonly string[];
  retry: "save" | "switch" | "apply";
};
/** The unsaved-changes dialog waits while the dangling confirm is up. */
export function leaveGuardDialogVisible(
  guardOpen: boolean,
  pendingDangling: PendingDangling | null,
): boolean {
  return guardOpen && pendingDangling === null;
}
