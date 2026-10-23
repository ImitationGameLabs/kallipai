// factLines: the shared fact table's rendering, pinned line for line. The
// table is tagma-wide, so these are the lines the fleet panel shows in one
// place; the fixtures mirror the wire bytes (snake_case fields, `until`
// already filtered by the tagma's own display bound).
import { assertEquals } from "@std/assert";
import type { FactView } from "@kallipai/kallipai-client";
import { factLines } from "./upstreamFacts.ts";

/** The clock the countdown is pinned against (unix seconds). */
const NOW = 1_700_000_000;

const quota: FactView = {
  kind: "quota",
  until: NOW + 2 * 3_600,
  detail: "insufficient_quota",
  recorded_at: NOW - 60,
  profile: "openai-main",
};

const payment: FactView = {
  kind: "payment",
  detail: "billing hard limit reached",
  recorded_at: NOW - 30,
  profile: "billing-main",
};

Deno.test("one line per fact, in the wire's own order", () => {
  const lines = factLines([quota, payment], NOW);
  assertEquals(lines.length, 2);
  assertEquals(lines[0]?.kind, "quota");
  assertEquals(lines[1]?.kind, "payment");
});

Deno.test("each line names the profile the fact is about", () => {
  const lines = factLines([quota, payment], NOW);
  assertEquals(lines[0]?.profile, "openai-main");
  assertEquals(lines[1]?.profile, "billing-main");
});

Deno.test("a reset hint renders as a countdown", () => {
  assertEquals(factLines([quota], NOW)[0]?.reopens, "reopens in 2h");
});

Deno.test("no reset hint, or one already past, renders no countdown", () => {
  assertEquals(factLines([payment], NOW)[0]?.reopens, undefined);
  assertEquals(
    factLines([{ ...quota, until: NOW - 1 }], NOW)[0]?.reopens,
    undefined,
  );
});

Deno.test("the upstream's own words ride through verbatim", () => {
  assertEquals(
    factLines([payment], NOW)[0]?.detail,
    "billing hard limit reached",
  );
});

Deno.test("facts key apart by profile, not only by kind", () => {
  // The table keeps one record per profile, so two profiles can carry the
  // same kind: the lines must stay separate -- equal keys throw in the
  // keyed each.
  const other: FactView = { ...quota, profile: "openai-backup" };
  const lines = factLines([quota, other], NOW);
  assertEquals(lines[0]?.key === lines[1]?.key, false);
  assertEquals(lines[0]?.profile, "openai-main");
  assertEquals(lines[1]?.profile, "openai-backup");
});

Deno.test("lines stay keyed apart by kind, profile and record time", () => {
  const lines = factLines([quota, { ...quota, recorded_at: NOW - 120 }], NOW);
  assertEquals(lines[0]?.key === lines[1]?.key, false);
  assertEquals(factLines([quota], NOW)[0]?.key, lines[0]?.key);
});
