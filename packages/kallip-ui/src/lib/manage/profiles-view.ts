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
} from "@kallipai/kallip-client";
import {
  manage_profiles_probe_models_one,
  manage_profiles_probe_models_other,
  manage_profiles_probe_status_invalid,
  manage_profiles_probe_status_ok,
  manage_profiles_probe_status_partial,
  manage_profiles_probe_status_unauthorized,
  manage_profiles_probe_status_unreachable,
} from "../../paraglide/messages.js";

/** `${tierIdx}:${profileId}` — profile ids can repeat across tiers. */
export function profileKey(tierIdx: number, profileId: string): string {
  return `${tierIdx}:${profileId}`;
}

/** Merge a probe response's provider-scope results into the page-held map. */
export function mergeProviderScope(
  reports: Map<string, ProfileProviderProbeReport>,
  resp: ProfileProbeResponse,
): void {
  for (const r of resp.results) reports.set(r.endpoint_id, r);
}

/** Merge a tier-scoped response: tiers[0] is the requested tier. */
export function mergeProfileScope(
  tierIdx: number,
  reports: Map<string, ProfileModelProbeReport>,
  resp: ProfileProbeResponse,
): void {
  const t = resp.tiers[0];
  if (!t) return;
  for (const p of t.profiles) {
    reports.set(profileKey(tierIdx, p.profile_id), p);
  }
}

/**
 * Merge an all-scope response: response tiers line up 1:1 with the draft
 * tiers by request order — tiers[i] reports on draft tier i.
 */
export function mergeProfileScopeAll(
  reports: Map<string, ProfileModelProbeReport>,
  resp: ProfileProbeResponse,
): void {
  for (const t of resp.tiers) {
    for (const p of t.profiles) {
      reports.set(profileKey(t.index, p.profile_id), p);
    }
  }
}

export function clearProfileResult(
  reports: Map<string, ProfileModelProbeReport>,
  tierIdx: number,
  profileId: string,
): void {
  reports.delete(profileKey(tierIdx, profileId));
}

/** Endpoint (provider) ids of the draft config. */
export function providerIdsOf(
  draft: ProfileConfig | null | undefined,
): string[] {
  return Object.keys(draft?.endpoints ?? {});
}

/**
 * Every profile id visible in the draft — tiers ∪ parking (the parking
 * dialog's new-mode duplicate check; advisory only, PUT is authoritative).
 */
export function occupiedIdsOf(
  draft: ProfileConfig | null | undefined,
): string[] {
  const ids = (draft?.tiers ?? []).flatMap((t) => t.profiles.map((p) => p.id));
  return [...ids, ...(draft?.parking ?? []).map((p) => p.id)];
}

export function probeStatusLabel(s: ProfileProbeStatus): string {
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

export const probeStatusColor: Record<ProfileProbeStatus, string> = {
  ok: "text-success-500 dark:text-success-400",
  partial: "text-warning-500 dark:text-warning-400",
  unreachable: "text-error-500 dark:text-error-400",
  unauthorized: "text-error-500 dark:text-error-400",
  invalid_config: "text-error-500 dark:text-error-400",
};

export function modelsCountLabel(count: number): string {
  return count === 1
    ? manage_profiles_probe_models_one({ count })
    : manage_profiles_probe_models_other({ count });
}

/**
 * Parked-live snapshot core: which parked ids some live agent still runs.
 * Takes the settled per-agent statuses (the page fetches them; per-agent
 * failures arrive as rejections and are skipped — one 409/404 must not void
 * the advisory snapshot). Null = nothing parked-live.
 */
export function parkedLiveSnapshot(
  parkedIds: string[],
  statuses: PromiseSettledResult<AgentStatusResponse>[],
): { agentCount: number; profileIds: string[] } | null {
  const parked = new Set(parkedIds);
  let agentCount = 0;
  const profileIds = new Set<string>();
  for (const s of statuses) {
    if (s.status !== "fulfilled") continue;
    const pid = s.value.profile?.profile_id;
    if (pid && parked.has(pid)) {
      agentCount++;
      profileIds.add(pid);
    }
  }
  return agentCount > 0 ? { agentCount, profileIds: [...profileIds] } : null;
}
