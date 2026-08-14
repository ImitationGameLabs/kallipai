// Unit tests for pure computation helpers extracted from management stores
// and BudgetBar. Covers budget bar edge cases (zero-budget regression),
// burn rate / ETA calculations, and profile draft mutators.

import { assertEquals } from "@std/assert";
import type { ProfileConfig } from "@kallipai/kallip-client";
import {
  addEndpoint,
  addProfile,
  addTier,
  barColorClass,
  barFillPct,
  type BudgetSample,
  burnRate,
  consumedPct,
  cronHasFiveFields,
  etaMinutes,
  isBudgetPaused,
  profileConfigEqual,
  removeEndpoint,
  removeLastTier,
  removeProfile,
  validateWarnMinutes,
} from "./compute.ts";

// --- consumedPct ---

Deno.test("consumedPct: budget 0 returns 0 (not 100)", () => {
  assertEquals(consumedPct(0, 0), 0);
  assertEquals(consumedPct(500, 0), 0);
});
Deno.test("consumedPct: exact 50%", () => {
  assertEquals(consumedPct(50, 100), 50);
});
Deno.test("consumedPct: rounds to nearest integer", () => {
  assertEquals(consumedPct(1, 3), 33);
  assertEquals(consumedPct(2, 3), 67);
});
Deno.test("consumedPct: clamps to 100 when consumed exceeds budget", () => {
  assertEquals(consumedPct(150, 100), 100);
});

// --- barFillPct ---

Deno.test("barFillPct: budget 0 returns 0 (neutral empty bar)", () => {
  assertEquals(barFillPct(0, 0), 0);
  assertEquals(barFillPct(500, 0), 0);
});
Deno.test("barFillPct: normal case", () => {
  assertEquals(barFillPct(25, 100), 25);
  assertEquals(barFillPct(100, 100), 100);
});

// --- barColorClass ---

Deno.test("barColorClass: budget 0 returns empty string (neutral)", () => {
  assertEquals(barColorClass(0, 0), "");
  assertEquals(barColorClass(500, 0), "");
});
Deno.test("barColorClass: green when >60% remaining", () => {
  assertEquals(barColorClass(30, 100), "bg-success-500");
  assertEquals(barColorClass(1, 100), "bg-success-500");
});
Deno.test("barColorClass: amber when 15-60% remaining", () => {
  assertEquals(barColorClass(40, 100), "bg-warning-500");
  assertEquals(barColorClass(85, 100), "bg-warning-500");
});
Deno.test("barColorClass: red when <15% remaining", () => {
  assertEquals(barColorClass(86, 100), "bg-error-500");
  assertEquals(barColorClass(100, 100), "bg-error-500");
});

// --- isBudgetPaused ---

Deno.test("isBudgetPaused: true when remaining 0 but budget exists", () => {
  assertEquals(isBudgetPaused(0, 1_000_000), true);
});
Deno.test("isBudgetPaused: false when budget is 0 (never set)", () => {
  assertEquals(isBudgetPaused(0, 0), false);
});
Deno.test("isBudgetPaused: false when remaining > 0", () => {
  assertEquals(isBudgetPaused(500, 1_000_000), false);
});

// --- burnRate ---

Deno.test("burnRate: null when fewer than 2 samples", () => {
  assertEquals(burnRate([]), null);
  assertEquals(burnRate([{ consumed: 100, timestamp: 1000 }]), null);
});
Deno.test("burnRate: null when timestamps equal (zero elapsed)", () => {
  assertEquals(
    burnRate([{ consumed: 100, timestamp: 1000 }, {
      consumed: 200,
      timestamp: 1000,
    }]),
    null,
  );
});
Deno.test("burnRate: 0 when consumption is flat (idle)", () => {
  assertEquals(
    burnRate([{ consumed: 100, timestamp: 1000 }, {
      consumed: 100,
      timestamp: 2000,
    }]),
    0,
  );
});
Deno.test("burnRate: correct tokens/min over 1 minute", () => {
  assertEquals(
    burnRate([{ consumed: 400, timestamp: 0 }, {
      consumed: 1000,
      timestamp: 60_000,
    }]),
    600,
  );
});
Deno.test("burnRate: correct tokens/min over sub-minute window", () => {
  assertEquals(
    burnRate([{ consumed: 0, timestamp: 0 }, {
      consumed: 100,
      timestamp: 10_000,
    }]),
    600,
  );
});
Deno.test("burnRate: uses first and last sample (not adjacent pair)", () => {
  const samples: BudgetSample[] = [
    { consumed: 0, timestamp: 0 },
    { consumed: 5000, timestamp: 10_000 },
    { consumed: 100, timestamp: 60_000 },
  ];
  assertEquals(burnRate(samples), 100);
});

// --- etaMinutes ---

Deno.test("etaMinutes: null when rate is null", () => {
  assertEquals(etaMinutes(1_000_000, null), null);
});
Deno.test("etaMinutes: null when rate is 0 (idle)", () => {
  assertEquals(etaMinutes(1_000_000, 0), null);
});
Deno.test("etaMinutes: correct value", () => {
  assertEquals(etaMinutes(1_000_000, 5000), 200);
});
Deno.test("etaMinutes: rounds to nearest minute", () => {
  assertEquals(etaMinutes(100, 30), 3);
});

// --- Profile draft mutators ---

const emptyConfig: ProfileConfig = { tiers: [], endpoints: {} };

Deno.test("addTier: appends an empty tier", () => {
  const r = addTier(emptyConfig);
  assertEquals(r.tiers.length, 1);
  assertEquals(r.tiers[0].profiles, []);
});
Deno.test("addTier: preserves existing tiers and endpoints", () => {
  const base: ProfileConfig = {
    tiers: [{ profiles: [] }],
    endpoints: {
      ep1: { id: "ep1", family: "deepseek", api_key: "", base_url: null },
    },
  };
  const r = addTier(base);
  assertEquals(r.tiers.length, 2);
  assertEquals(r.endpoints, base.endpoints);
  assertEquals(r.tiers[0], base.tiers[0]);
});
Deno.test("removeLastTier: removes the last tier", () => {
  const base = addTier(addTier(emptyConfig));
  const r = removeLastTier(base);
  assertEquals(r.tiers.length, 1);
});
Deno.test("removeLastTier: no-op when already empty", () => {
  assertEquals(removeLastTier(emptyConfig), emptyConfig);
});
Deno.test("addProfile: adds blank profile to correct tier", () => {
  const base = addTier(addTier(emptyConfig));
  const r = addProfile(base, 0);
  assertEquals(r.tiers[0].profiles.length, 1);
  assertEquals(r.tiers[0].profiles[0].id, "");
  assertEquals(r.tiers[0].profiles[0].max_context_window, 128_000);
  assertEquals(r.tiers[1].profiles.length, 0);
});
Deno.test("removeProfile: removes correct profile from correct tier", () => {
  let c = addTier(emptyConfig);
  c = addProfile(c, 0);
  c = addProfile(c, 0);
  assertEquals(c.tiers[0].profiles.length, 2);
  const r = removeProfile(c, 0, 0);
  assertEquals(r.tiers[0].profiles.length, 1);
  assertEquals(r.tiers[0].profiles[0], c.tiers[0].profiles[1]);
});
Deno.test("addEndpoint: adds endpoint under given id", () => {
  const r = addEndpoint(emptyConfig, "new-ep");
  assertEquals(r.endpoints["new-ep"].family, "deepseek");
  assertEquals(r.endpoints["new-ep"].api_key, "");
  assertEquals(r.endpoints["new-ep"].base_url, null);
});
Deno.test("removeEndpoint: removes endpoint by id", () => {
  const base = addEndpoint(emptyConfig, "ep1");
  assertEquals(base.endpoints["ep1"] !== undefined, true);
  assertEquals(removeEndpoint(base, "ep1").endpoints["ep1"], undefined);
});
Deno.test("profileConfigEqual: true for identical configs", () => {
  const a = addTier(emptyConfig);
  assertEquals(profileConfigEqual(a, a), true);
});
Deno.test("profileConfigEqual: false when draft diverges", () => {
  const committed = addTier(emptyConfig);
  const dirty = addTier(committed);
  assertEquals(profileConfigEqual(committed, dirty), false);
});

// --- Edge cases from code review ---

Deno.test("burnRate: decreasing consumption returns 0 (idle)", () => {
  assertEquals(
    burnRate([{ consumed: 200, timestamp: 0 }, {
      consumed: 100,
      timestamp: 60_000,
    }]),
    0,
  );
});

Deno.test("barColorClass: consumed exceeding budget still clamps correctly", () => {
  // 150% consumed → pct clamps to 100 → 0% remaining → red
  assertEquals(barColorClass(1500, 1000), "bg-error-500");
});

Deno.test("addProfile: out-of-range tierIdx is a no-op (no tier created)", () => {
  const base = addTier(emptyConfig);
  const result = addProfile(base, 99);
  assertEquals(result.tiers[0].profiles.length, 0);
});

Deno.test("etaMinutes: negative remaining returns negative (over-budget scenario)", () => {
  // Edge: remaining can go negative briefly during optimistic updates.
  // The function does NOT clamp — caller handles display.
  assertEquals(etaMinutes(-500, 100), -5);
});

// --- Schedule validation ---

Deno.test("cronHasFiveFields: valid 5-field cron", () => {
  assertEquals(cronHasFiveFields("0 9 * * 1-5"), true);
  assertEquals(cronHasFiveFields("*/5 0 * * *"), true);
});

Deno.test("cronHasFiveFields: rejects non-5-field input", () => {
  assertEquals(cronHasFiveFields("0 9 *"), false);
  assertEquals(cronHasFiveFields("0 9 * * 1-5 extra"), false);
  assertEquals(cronHasFiveFields(""), false);
});

Deno.test("cronHasFiveFields: tolerates extra whitespace", () => {
  assertEquals(cronHasFiveFields("  0   9   *   *   1-5  "), true);
});

Deno.test("validateWarnMinutes: valid values return null", () => {
  assertEquals(validateWarnMinutes(10, 5), null);
  assertEquals(validateWarnMinutes(5, 5), null);
});

Deno.test("validateWarnMinutes: pre < final returns error", () => {
  assertEquals(validateWarnMinutes(3, 5) !== null, true);
});

Deno.test("validateWarnMinutes: non-positive returns error", () => {
  assertEquals(validateWarnMinutes(0, 5) !== null, true);
  assertEquals(validateWarnMinutes(10, 0) !== null, true);
});

Deno.test("validateWarnMinutes: NaN returns error", () => {
  assertEquals(validateWarnMinutes(NaN, 5) !== null, true);
});
