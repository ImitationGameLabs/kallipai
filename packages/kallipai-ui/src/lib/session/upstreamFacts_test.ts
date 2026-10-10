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
  endpoint: { family: "openai", base_url: "https://api.openai.com/v1" },
  account_hint: "3f9a1b2c",
};

const payment: FactView = {
  kind: "payment",
  detail: "billing hard limit reached",
  recorded_at: NOW - 30,
  account_hint: "aa11bb22",
};

Deno.test("one line per fact, in the wire's own order", () => {
  const lines = factLines([quota, payment], NOW);
  assertEquals(lines.length, 2);
  assertEquals(lines[0]?.kind, "quota");
  assertEquals(lines[1]?.kind, "payment");
});

Deno.test(
  "an endpoint fact names its family, an account fact is account-wide",
  () => {
    const lines = factLines([quota, payment], NOW);
    assertEquals(lines[0]?.scope, "openai");
    assertEquals(lines[1]?.scope, "account-wide");
  },
);

Deno.test("the account hint renders as the wire's eight hex", () => {
  assertEquals(factLines([quota], NOW)[0]?.account, "account 3f9a1b2c");
});

Deno.test("a reset hint renders as a countdown", () => {
  assertEquals(factLines([quota], NOW)[0]?.reopens, "reopens in 2h");
});

Deno.test("an account-wide fact carries no endpoint", () => {
  assertEquals(factLines([payment], NOW)[0]?.endpoint, undefined);
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

Deno.test("endpoint facts key apart by base URL, not only by family", () => {
  // One account, one family, two bases, the same second: the tagma keeps
  // these as separate endpoint entries, so the lines must stay separate —
  // equal keys throw in the keyed each.
  const other: FactView = {
    ...quota,
    endpoint: { family: "openai", base_url: "https://relay.example/v1" },
  };
  const lines = factLines([quota, other], NOW);
  assertEquals(lines[0]?.key === lines[1]?.key, false);
  assertEquals(lines[0]?.endpoint, "https://api.openai.com/v1");
  assertEquals(lines[1]?.endpoint, "https://relay.example/v1");
});

Deno.test(
  "lines stay keyed apart by kind, scope, account and record time",
  () => {
    const lines = factLines([quota, { ...quota, recorded_at: NOW - 120 }], NOW);
    assertEquals(lines[0]?.key === lines[1]?.key, false);
    assertEquals(factLines([quota], NOW)[0]?.key, lines[0]?.key);
  },
);
