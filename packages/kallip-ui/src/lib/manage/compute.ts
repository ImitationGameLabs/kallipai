// Pure computation helpers extracted from budget.svelte.ts, BudgetBar.svelte,
// and profiles.svelte.ts. These encode the behavioral rules (thresholds, edge
// cases, optimistic math) and are unit-tested in compute_test.ts. The stores
// and component delegate to these functions; the $state/$derived plumbing stays
// in the .svelte.ts / .svelte files.

import type {
  ProfileConfig,
  ProfileEndpoint,
  ProfileModel,
  ProfileProbeRequest,
} from "@kallipai/kallip-client";
import {
  manage_schedules_warn_invalid,
  manage_schedules_warn_order,
  manage_schedules_warn_positive,
} from "../../paraglide/messages.js";

// ---------------------------------------------------------------------------
// Budget helpers
// ---------------------------------------------------------------------------

/** True when the tagma-wide budget is paused (remaining is zero but a budget exists). */
export function isBudgetPaused(remaining: number, budget: number): boolean {
  return remaining === 0 && budget > 0;
}

/**
 * Percentage of budget consumed (0–100). Returns 0 when budget is unset
 * (0) to avoid a misleading "100% consumed" on a neutral state.
 */
export function consumedPct(consumed: number, budget: number): number {
  if (budget === 0) return 0;
  return Math.min(100, Math.round((consumed / budget) * 100));
}

/**
 * The fill percentage for the BudgetBar (0–100). Same computation as
 * consumedPct — kept as a separate export for semantic clarity at the
 * call site (bar fill vs data-layer percentage).
 */
export function barFillPct(consumed: number, budget: number): number {
  return consumedPct(consumed, budget);
}

/**
 * CSS class for the budget bar fill. Green when >60% remaining, amber when
 * 15–60% remaining, red when <15% remaining. Empty string when budget is 0
 * (neutral — no color fill).
 */
export function barColorClass(consumed: number, budget: number): string {
  if (budget === 0) return "";
  const remainingPct = 100 - consumedPct(consumed, budget);
  if (remainingPct > 60) return "bg-success-500";
  if (remainingPct >= 15) return "bg-warning-500";
  return "bg-error-500";
}

export interface BudgetSample {
  consumed: number;
  timestamp: number;
}

/**
 * Burn rate in tokens/min computed from a series of samples. Returns null if
 * fewer than 2 samples or if timestamps are non-increasing. Returns 0 if
 * consumption is flat or decreasing (idle).
 */
export function burnRate(samples: BudgetSample[]): number | null {
  if (samples.length < 2) return null;
  const first = samples[0]!;
  const last = samples[samples.length - 1]!;
  const dtSec = (last.timestamp - first.timestamp) / 1000;
  if (dtSec <= 0) return null;
  const dTokens = last.consumed - first.consumed;
  if (dTokens <= 0) return 0;
  return Math.round((dTokens / dtSec) * 60);
}

/**
 * Estimated minutes until budget exhaustion at the given burn rate. Returns
 * null when the rate is null (insufficient data) or zero (idle — no ETA).
 */
export function etaMinutes(
  remaining: number,
  rate: number | null,
): number | null {
  if (rate === null || rate === 0) return null;
  return Math.round(remaining / rate);
}

// ---------------------------------------------------------------------------
// Profile draft helpers
// ---------------------------------------------------------------------------

/** Default max_context_window for a newly added profile. */
const DEFAULT_MAX_CONTEXT = 128_000;

/** Append a new empty tier to the end of the tiers array. */
export function addTier(config: ProfileConfig): ProfileConfig {
  return { ...config, tiers: [...config.tiers, { profiles: [] }] };
}

/** Remove the last tier (truncate-tail). No-op if already empty. */
export function removeLastTier(config: ProfileConfig): ProfileConfig {
  if (config.tiers.length === 0) return config;
  return { ...config, tiers: config.tiers.slice(0, -1) };
}

/** Add a blank profile with default fields to the tier at tierIdx. */
export function addProfile(
  config: ProfileConfig,
  tierIdx: number,
): ProfileConfig {
  const tiers = config.tiers.map((t, i) =>
    i === tierIdx
      ? {
        profiles: [
          ...t.profiles,
          {
            id: "",
            endpoint: "",
            model: "",
            max_context_window: DEFAULT_MAX_CONTEXT,
          },
        ],
      }
      : t
  );
  return { ...config, tiers };
}

/** Remove the profile at (tierIdx, profileIdx). */
export function removeProfile(
  config: ProfileConfig,
  tierIdx: number,
  profileIdx: number,
): ProfileConfig {
  const tiers = config.tiers.map((t, i) =>
    i === tierIdx
      ? { profiles: t.profiles.filter((_, pi) => pi !== profileIdx) }
      : t
  );
  return { ...config, tiers };
}

/** Add a new endpoint under the given id. */
export function addEndpoint(config: ProfileConfig, id: string): ProfileConfig {
  return {
    ...config,
    endpoints: {
      ...config.endpoints,
      [id]: { id, family: "deepseek", api_key: "", base_url: null },
    },
  };
}

/** Remove an endpoint by id. */
export function removeEndpoint(
  config: ProfileConfig,
  id: string,
): ProfileConfig {
  const { [id]: _, ...rest } = config.endpoints;
  return { ...config, endpoints: rest };
}

/** Insert or replace an endpoint under its id (id-keyed upsert).
 * Replacing is the Edit path — the dialog locks the id there, so a matching
 * id is always the same endpoint being updated; New-mode duplicate ids are
 * rejected by dialog validation before this runs.
 */
export function upsertEndpoint(
  config: ProfileConfig,
  endpoint: ProfileEndpoint,
): ProfileConfig {
  return {
    ...config,
    endpoints: { ...config.endpoints, [endpoint.id]: endpoint },
  };
}

/** Replace the profile list of the tier at tierIdx (dialog Save path).
 * Out-of-range tierIdx leaves the config unchanged.
 */
export function replaceTierProfiles(
  config: ProfileConfig,
  tierIdx: number,
  profiles: readonly ProfileModel[],
): ProfileConfig {
  const tiers = config.tiers.map((t, i) => i === tierIdx ? { profiles } : t);
  return { ...config, tiers };
}

/** Move a profile from one tier to another (drag-and-drop draft update).
 * The profile lands at the end of the target tier; a move within the same
 * tier reorders it to last. Invalid coordinates leave the config unchanged.
 */
export function moveProfile(
  config: ProfileConfig,
  fromTier: number,
  fromIdx: number,
  toTier: number,
): ProfileConfig {
  const profile = config.tiers[fromTier]?.profiles[fromIdx];
  if (!profile || toTier < 0 || toTier >= config.tiers.length) return config;
  const without = removeProfile(config, fromTier, fromIdx);
  const tiers = without.tiers.map((t, i) =>
    i === toTier ? { profiles: [...t.profiles, profile] } : t
  );
  return { ...without, tiers };
}

/** Structural equality check (used for isDirty). */
export function profileConfigEqual(
  a: ProfileConfig,
  b: ProfileConfig,
): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * The probe key rule shared by every probe path: only a freshly typed key —
 * one that differs from the committed (masked) value — is sent inline;
 * anything else probes with the live key (`null`), so the masked value from
 * GET never travels back up as a credential.
 */
function probeWireKey(
  draftKey: string | null,
  committedKey: string | null | undefined,
): string | null {
  return draftKey && draftKey !== committedKey ? draftKey : null;
}

/**
 * Translate the editable draft into a PUT wire body: an empty key means "keep
 * the live key" (null on the wire); a masked echo is passed through — the
 * server also treats it as "keep".
 */
export function profileConfigToWire(draft: ProfileConfig): ProfileConfig {
  const endpoints = Object.fromEntries(
    Object.entries(draft.endpoints).map(([id, ep]) => [
      id,
      { ...ep, api_key: ep.api_key === "" ? null : ep.api_key },
    ]),
  );
  return { ...draft, endpoints };
}

/**
 * Build a probe request from the draft: endpoints not carrying a freshly
 * typed key probe with the live key (`api_key: null`), so the masked value
 * from GET is never sent as a credential. `tierIdx` probes a single tier;
 * omit for all.
 */
export function buildProbeRequest(
  committed: ProfileConfig | null,
  draft: ProfileConfig,
  tierIdx?: number,
): ProfileProbeRequest {
  const endpoints = Object.values(draft.endpoints).map((ep) => ({
    id: ep.id,
    family: ep.family,
    base_url: ep.base_url,
    api_key: probeWireKey(ep.api_key, committed?.endpoints[ep.id]?.api_key),
  }));
  const tiers = [
    ...(tierIdx === undefined
      ? draft.tiers
      : [draft.tiers[tierIdx]].filter((t) => t !== undefined)),
  ].map((t) => ({
    profiles: t.profiles.map((p) => ({
      id: p.id,
      endpoint: p.endpoint,
      model: p.model,
    })),
  }));
  return { endpoints, tiers };
}

/**
 * Build a single-endpoint probe request (no tier checks); null when the
 * endpoint id is not in the draft.
 */
export function singleEndpointProbeRequest(
  committed: ProfileConfig | null,
  draft: ProfileConfig,
  id: string,
): ProfileProbeRequest | null {
  const ep = draft.endpoints[id];
  if (!ep) return null;
  return {
    endpoints: [{
      id: ep.id,
      family: ep.family,
      base_url: ep.base_url,
      api_key: probeWireKey(ep.api_key, committed?.endpoints[id]?.api_key),
    }],
    tiers: [],
  };
}

/**
 * Build a single-profile probe request (the profile Test button): the
 * tier carries only that profile, and its endpoint rides inline under the
 * shared key rule. A dangling endpoint reference still probes — the server
 * reports the missing reference as invalid_config, which is the honest
 * verdict for that profile. Null when the coordinates are out of range.
 */
export function singleProfileProbeRequest(
  committed: ProfileConfig | null,
  draft: ProfileConfig,
  tierIdx: number,
  profileIdx: number,
): ProfileProbeRequest | null {
  const profile = draft.tiers[tierIdx]?.profiles[profileIdx];
  if (!profile) return null;
  const ep = draft.endpoints[profile.endpoint];
  const endpoints = ep
    ? [{
      id: ep.id,
      family: ep.family,
      base_url: ep.base_url,
      api_key: probeWireKey(ep.api_key, committed?.endpoints[ep.id]?.api_key),
    }]
    : [];
  return {
    endpoints,
    tiers: [{
      profiles: [{
        id: profile.id,
        endpoint: profile.endpoint,
        model: profile.model,
      }],
    }],
  };
}

// ---------------------------------------------------------------------------
// Schedule validation helpers
// ---------------------------------------------------------------------------

/** Cron field count check — returns false when not exactly 5 tokens. */
export function cronHasFiveFields(expr: string): boolean {
  return expr.trim().split(/\s+/).filter(Boolean).length === 5;
}

/**
 * Validate warn-minute fields. pre must be >= final and both must be positive.
 * Returns null if valid, or a localized error message string.
 */
export function validateWarnMinutes(pre: number, final: number): string | null {
  if (isNaN(pre) || isNaN(final)) return manage_schedules_warn_invalid();
  if (pre <= 0 || final <= 0) return manage_schedules_warn_positive();
  if (pre < final) return manage_schedules_warn_order();
  return null;
}
