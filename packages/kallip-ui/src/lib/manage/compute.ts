// Pure computation helpers extracted from budget.svelte.ts, BudgetBar.svelte,
// and profiles.svelte.ts. These encode the behavioral rules (thresholds, edge
// cases, optimistic math) and are unit-tested in compute_test.ts. The stores
// and component delegate to these functions; the $state/$derived plumbing stays
// in the .svelte.ts / .svelte files.

import type { ProfileConfig } from "@kallipai/kallip-client";

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
export function addEndpoint(
  config: ProfileConfig,
  id: string,
): ProfileConfig {
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

/** Structural equality check (used for isDirty). */
export function profileConfigEqual(
  a: ProfileConfig,
  b: ProfileConfig,
): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
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
 * Returns null if valid, or an error message string.
 */
export function validateWarnMinutes(
  pre: number,
  final: number,
): string | null {
  if (isNaN(pre) || isNaN(final)) return "warn minutes must be valid numbers";
  if (pre <= 0 || final <= 0) return "warn minutes must be positive";
  if (pre < final) return "pre-warn must be >= final-warn";
  return null;
}
