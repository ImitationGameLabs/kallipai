// Pure computation helpers extracted from budget.svelte.ts, BudgetBar.svelte,
// and profiles.svelte.ts. These encode the behavioral rules (thresholds, edge
// cases, optimistic math) and are unit-tested in compute_test.ts. The stores
// and component delegate to these functions; the $state/$derived plumbing stays
// in the .svelte.ts / .svelte files.

import type {
  Modality,
  ProfileConfig,
  ProfileConfigPutRequest,
  ProfileProvider,
  ProfileModel,
  ProfileProbeRequest,
  ProfileSet,
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

/** Replace one set's value, keeping the rest (the shared tail of every
 * set-mutating helper below). */
function updateSet(
  config: ProfileConfig,
  name: string,
  set: ProfileSet,
): ProfileConfig {
  return { ...config, sets: { ...config.sets, [name]: set } };
}

/** First free `set-N` name (N counts from 1). Generated names are wire
 * identifiers (the server restricts set names to `[A-Za-z0-9_-]`), which
 * this pattern always satisfies; the set dialog can rename them. */
function generatedSetName(config: ProfileConfig): string {
  let n = 1;
  while (config.sets[`set-${n}`] !== undefined) n++;
  return `set-${n}`;
}

/** Append a new empty set under a generated name. The first set of an
 * empty config also becomes the default, mirroring the server's
 * single-set resolution so the draft never shows a default-less config. */
export function addSet(config: ProfileConfig): ProfileConfig {
  const name = generatedSetName(config);
  const next = updateSet(config, name, { description: null, profiles: [] });
  return Object.keys(config.sets).length === 0
    ? { ...next, default: name }
    : next;
}

/** Remove the named set. Removing the default set re-points the default at
 * the first remaining name (sorted — the wire order) or clears it when no
 * sets remain; removing any other set leaves the default untouched.
 * Unknown names are a no-op. */
export function removeSet(config: ProfileConfig, name: string): ProfileConfig {
  if (config.sets[name] === undefined) return config;
  const { [name]: _removed, ...sets } = config.sets;
  const { default: _oldDefault, ...rest } = config;
  // Only removing the default set re-points it; a surviving default stays.
  if (config.default !== name) {
    return config.default === undefined
      ? { ...rest, sets }
      : { ...rest, sets, default: config.default };
  }
  const next = Object.keys(sets).sort()[0];
  return next === undefined
    ? { ...rest, sets }
    : { ...rest, sets, default: next };
}

/** Rename a set (the set dialog Save path when the name changed). The
 * default reference follows the rename; renaming to an existing name or
 * an unknown source is a no-op. Agents bound to the old name are the
 * server's dangling-record concern, surfaced by its own flows. */
export function renameSet(
  config: ProfileConfig,
  from: string,
  to: string,
): ProfileConfig {
  const set = config.sets[from];
  if (!set || from === to || config.sets[to] !== undefined) return config;
  const { [from]: _old, ...rest } = config.sets;
  const sets = { ...rest, [to]: set };
  return config.default === from
    ? { ...config, sets, default: to }
    : { ...config, sets };
}

/** Set the set's description (the set dialog Save path). Unknown names
 * are a no-op. */
export function updateSetDescription(
  config: ProfileConfig,
  name: string,
  description: string | null,
): ProfileConfig {
  const set = config.sets[name];
  if (!set) return config;
  return updateSet(config, name, { ...set, description });
}

/** Mark the named set as the default (rides PUT as the `default`
 * field). Unknown names are a no-op. */
export function setDefaultSet(
  config: ProfileConfig,
  name: string,
): ProfileConfig {
  return config.sets[name] === undefined
    ? config
    : { ...config, default: name };
}

// ---------------------------------------------------------------------------
// Profile modality helpers
// ---------------------------------------------------------------------------

/** Canonical modality display order (mirrors kallip-common Modality::ALL). */
export const MODALITY_ORDER: readonly Modality[] = [
  "text",
  "image",
  "audio",
  "video",
];

/** Modalities a profile declares; absent = text-only (the server default). */
export function profileModalities(profile: ProfileModel): readonly Modality[] {
  return profile.modalities ?? ["text"];
}

/** Effective modalities of a set: the intersection across member
 * profiles. An empty set short-circuits to the empty intersection,
 * matching kallip-runtime ProfileSet::effective_modalities. */
export function setEffectiveModalities(set: ProfileSet): Modality[] {
  if (set.profiles.length === 0) {
    return [];
  }
  return MODALITY_ORDER.filter((m) =>
    set.profiles.every((p) => profileModalities(p).includes(m)),
  );
}

/** True when some member declares modalities beyond the set's effective
 * intersection; requests silently narrow to the intersection. */
export function setHasShadowedMembers(set: ProfileSet): boolean {
  const effective = setEffectiveModalities(set);
  return set.profiles.some((p) =>
    profileModalities(p).some((m) => !effective.includes(m)),
  );
}

/** Join modalities in canonical order for display ("text, image"). */
export function formatModalities(modalities: readonly Modality[]): string {
  return MODALITY_ORDER.filter((m) => modalities.includes(m)).join(", ");
}

/** Modalities for the wire: a text-only selection is the server
 * default and rides as absent (keeps isDirty honest); anything
 * else is carried as declared.
 */
export function normalizeModalities(
  modalities: readonly Modality[],
): readonly Modality[] | undefined {
  if (modalities.length === 1 && modalities[0] === "text") {
    return undefined;
  }
  return [...modalities];
}

/** Add a blank profile with default fields to the named set. Unknown set
 * names leave the config unchanged. */
export function addProfile(
  config: ProfileConfig,
  setName: string,
): ProfileConfig {
  const set = config.sets[setName];
  if (!set) return config;
  return updateSet(config, setName, {
    ...set,
    profiles: [
      ...set.profiles,
      {
        id: "",
        endpoint: "",
        model: "",
        max_context_window: DEFAULT_MAX_CONTEXT,
      },
    ],
  });
}

/** Remove the profile at (setName, profileIdx). Unknown set names or an
 * out-of-range index leave the config unchanged. */
export function removeProfile(
  config: ProfileConfig,
  setName: string,
  profileIdx: number,
): ProfileConfig {
  const set = config.sets[setName];
  if (!set) return config;
  return updateSet(config, setName, {
    ...set,
    profiles: set.profiles.filter((_, pi) => pi !== profileIdx),
  });
}

/** Add a new provider under the given id. */
export function addProvider(config: ProfileConfig, id: string): ProfileConfig {
  return {
    ...config,
    endpoints: {
      ...config.endpoints,
      [id]: { id, family: "deepseek", api_key: "", base_url: null },
    },
  };
}

/** Remove a provider by id. */
export function removeProvider(
  config: ProfileConfig,
  id: string,
): ProfileConfig {
  const { [id]: _, ...rest } = config.endpoints;
  return { ...config, endpoints: rest };
}

/** Insert or replace a provider under its id (id-keyed upsert).
 * Replacing is the Edit path — the dialog locks the id there, so a matching
 * id is always the same provider being updated; New-mode duplicate ids are
 * rejected by dialog validation before this runs.
 */
export function upsertProvider(
  config: ProfileConfig,
  provider: ProfileProvider,
): ProfileConfig {
  return {
    ...config,
    endpoints: { ...config.endpoints, [provider.id]: provider },
  };
}

/** Replace the profile list of the named set (dialog Save path). Unknown
 * set names leave the config unchanged.
 */
export function replaceSetProfiles(
  config: ProfileConfig,
  setName: string,
  profiles: readonly ProfileModel[],
): ProfileConfig {
  const set = config.sets[setName];
  if (!set) return config;
  return updateSet(config, setName, { ...set, profiles: [...profiles] });
}

/** Replace the profile at (setName, profileIdx) (the set-member
 * profile dialog Save path). Unknown set names or an out-of-range
 * index leave the config unchanged.
 */
export function replaceSetProfile(
  config: ProfileConfig,
  setName: string,
  profileIdx: number,
  profile: ProfileModel,
): ProfileConfig {
  const set = config.sets[setName];
  if (!set || set.profiles[profileIdx] === undefined) return config;
  return updateSet(config, setName, {
    ...set,
    profiles: set.profiles.map((p, pi) => (pi === profileIdx ? profile : p)),
  });
}

/** Move a profile from one set to another (drag-and-drop draft update).
 * The profile lands at the end of the target set; a move within the same
 * set reorders it to last. Unknown set names or an out-of-range index
 * leave the config unchanged.
 */
export function moveProfile(
  config: ProfileConfig,
  fromSet: string,
  fromIdx: number,
  toSet: string,
): ProfileConfig {
  const profile = config.sets[fromSet]?.profiles[fromIdx];
  if (!profile) return config;
  const without = removeProfile(config, fromSet, fromIdx);
  const target = without.sets[toSet];
  if (!target) return config;
  return updateSet(without, toSet, {
    ...target,
    profiles: [...target.profiles, profile],
  });
}

/** The parked list with `undefined` normalized back to absent when empty,
 * so a draft that parks and unparks everything compares equal (isDirty)
 * to a committed config that never carried the key.
 */
function withParking(
  config: ProfileConfig,
  parking: readonly ProfileModel[],
): ProfileConfig {
  return parking.length > 0 ? { ...config, parking } : omitParking(config);
}

/** A copy of the config with the `parking` key removed (absent = empty).
 * `structuredClone`-safe and JSON.stringify-friendly: an undefined-valued
 * optional property is dropped, matching the GET shape.
 */
function omitParking(config: ProfileConfig): ProfileConfig {
  const { parking: _unused, ...rest } = config;
  return rest;
}

/** Move a set profile into the parking area (drag-and-drop draft update).
 * The profile lands at the end of the parked list. Unknown set names or
 * an out-of-range index leave the config unchanged.
 */
export function moveToParking(
  config: ProfileConfig,
  fromSet: string,
  fromIdx: number,
): ProfileConfig {
  const profile = config.sets[fromSet]?.profiles[fromIdx];
  if (!profile) return config;
  const without = removeProfile(config, fromSet, fromIdx);
  return withParking(without, [...(without.parking ?? []), profile]);
}

/** Move a parked profile back into the named set (drag-and-drop draft
 * update). The profile lands at the end of the target set. Unknown set
 * names or an out-of-range index leave the config unchanged.
 */
export function moveFromParking(
  config: ProfileConfig,
  fromIdx: number,
  toSet: string,
): ProfileConfig {
  const profile = config.parking?.[fromIdx];
  const target = config.sets[toSet];
  if (!profile || !target) return config;
  const rest = (config.parking ?? []).filter((_, i) => i !== fromIdx);
  return withParking(
    updateSet(config, toSet, {
      ...target,
      profiles: [...target.profiles, profile],
    }),
    rest,
  );
}

/** Replace the parked list wholesale (the parking dialog Save path).
 * An empty list normalizes to the absent key (draft-equality rule above).
 */
export function replaceParkingProfiles(
  config: ProfileConfig,
  profiles: readonly ProfileModel[],
): ProfileConfig {
  return withParking(config, [...profiles]);
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
 * Translate the editable draft into a PUT wire body: sets flatten to a
 * name-carrying array, an empty key means "keep the live key" (null on
 * the wire), and a masked echo passes through — the server also treats
 * it as "keep".
 */
export function profileConfigToWire(
  draft: ProfileConfig,
): ProfileConfigPutRequest {
  const endpoints = Object.fromEntries(
    Object.entries(draft.endpoints).map(([id, ep]) => [
      id,
      { ...ep, api_key: ep.api_key === "" ? null : ep.api_key },
    ]),
  );
  const sets = Object.entries(draft.sets).map(([name, set]) => ({
    name,
    description: set.description,
    profiles: set.profiles,
  }));
  const wire: ProfileConfigPutRequest = {
    sets,
    endpoints,
    // Always send the parking key: an explicit empty list is the only way
    // to clear the parked area (the server keeps live parking when the
    // key is absent, so an omitted key would silently mean "keep").
    parking: draft.parking ?? [],
  };
  // `default` rides the draft as-is: absent when no sets exist, a
  // resolvable name otherwise.
  return draft.default === undefined
    ? wire
    : { ...wire, default: draft.default };
}

/**
 * Build a probe request from the draft: endpoints not carrying a freshly
 * typed key probe with the live key (`api_key: null`), so the masked value
 * from GET is never sent as a credential. `setName` probes a single set;
 * omit for all.
 */
export function buildProbeRequest(
  committed: ProfileConfig | null,
  draft: ProfileConfig,
  setName?: string,
): ProfileProbeRequest {
  const endpoints = Object.values(draft.endpoints).map((ep) => ({
    id: ep.id,
    family: ep.family,
    base_url: ep.base_url,
    api_key: probeWireKey(ep.api_key, committed?.endpoints[ep.id]?.api_key),
  }));
  const only = setName === undefined ? undefined : draft.sets[setName];
  const entries =
    setName === undefined
      ? Object.entries(draft.sets)
      : only
        ? [[setName, only] as const]
        : [];
  const sets = entries.map(([name, set]) => ({
    name,
    profiles: set.profiles.map((p) => ({
      id: p.id,
      endpoint: p.endpoint,
      model: p.model,
    })),
  }));
  return { endpoints, sets };
}

/**
 * Build a single-provider probe request (no set checks); null when the
 * provider id is not in the draft.
 */
export function singleProviderProbeRequest(
  committed: ProfileConfig | null,
  draft: ProfileConfig,
  id: string,
): ProfileProbeRequest | null {
  const ep = draft.endpoints[id];
  if (!ep) return null;
  return {
    endpoints: [
      {
        id: ep.id,
        family: ep.family,
        base_url: ep.base_url,
        api_key: probeWireKey(ep.api_key, committed?.endpoints[id]?.api_key),
      },
    ],
    sets: [],
  };
}

/**
 * Build a single-profile probe request (the profile Test button): the set
 * carries only that profile, and its provider rides inline under the
 * shared key rule. A dangling provider reference still probes — the server
 * reports the missing reference as invalid_config, which is the honest
 * verdict for that profile. Null when the set name or profile index is
 * out of range.
 */
export function singleProfileProbeRequest(
  committed: ProfileConfig | null,
  draft: ProfileConfig,
  setName: string,
  profileIdx: number,
): ProfileProbeRequest | null {
  const profile = draft.sets[setName]?.profiles[profileIdx];
  if (!profile) return null;
  const ep = draft.endpoints[profile.endpoint];
  const endpoints = ep
    ? [
        {
          id: ep.id,
          family: ep.family,
          base_url: ep.base_url,
          api_key: probeWireKey(
            ep.api_key,
            committed?.endpoints[ep.id]?.api_key,
          ),
        },
      ]
    : [];
  return {
    endpoints,
    sets: [
      {
        name: setName,
        profiles: [
          {
            id: profile.id,
            endpoint: profile.endpoint,
            model: profile.model,
          },
        ],
      },
    ],
  };
}
/** Probe-set name for parked profiles: `:` is invalid in a real set name
 * (the server restricts names to `[A-Za-z0-9_-]`), so this placeholder
 * can never collide with a named set's reports. */
const PARKING_PROBE_SET = ":parking";

/**
 * Build a single-profile probe request for a parked profile (the parking
 * card Test button): same shape as the set variant — the profile rides
 * in a one-profile probe set, its provider inline under the shared key
 * rule. A dangling provider reference still probes (server verdict:
 * invalid_config), and an undefined parked list (GET omitted the key) is
 * simply out of range. Null when idx is out of range.
 */
export function singleParkingProfileProbeRequest(
  committed: ProfileConfig | null,
  draft: ProfileConfig,
  idx: number,
): ProfileProbeRequest | null {
  const profile = draft.parking?.[idx];
  if (!profile) return null;
  const ep = draft.endpoints[profile.endpoint];
  const endpoints = ep
    ? [
        {
          id: ep.id,
          family: ep.family,
          base_url: ep.base_url,
          api_key: probeWireKey(
            ep.api_key,
            committed?.endpoints[ep.id]?.api_key,
          ),
        },
      ]
    : [];
  return {
    endpoints,
    sets: [
      {
        name: PARKING_PROBE_SET,
        profiles: [
          {
            id: profile.id,
            endpoint: profile.endpoint,
            model: profile.model,
          },
        ],
      },
    ],
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
