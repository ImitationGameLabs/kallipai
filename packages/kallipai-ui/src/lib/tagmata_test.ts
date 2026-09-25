import { assertEquals } from "@std/assert";
import { joinDeviceRows } from "./tagmata.svelte.ts";
import type { TagmaProcessLike } from "./tagmata.svelte.ts";
import { formatRemaining, formatTagmaStatusLine } from "./tagmata.svelte.ts";

Deno.test("formatRemaining: zero or negative -> expired", () => {
  assertEquals(formatRemaining(0), "expired");
  assertEquals(formatRemaining(-1), "expired");
});

Deno.test("formatRemaining: sub-minute -> <1min", () => {
  assertEquals(formatRemaining(1), "<1min");
  assertEquals(formatRemaining(59_999), "<1min");
});

Deno.test(
  "formatTagmaStatusLine: en renders active/total and token counts",
  () => {
    // char-exact: the message migration must not change the en readout.
    assertEquals(
      formatTagmaStatusLine({
        rootState: "busy",
        subagentsTotal: 3,
        subagentsActive: 1,
        tokenBudget: 50_000,
        tokenConsumed: 12_345,
        tokenBudgetUnlimited: false,
      }),
      "2/4 agents · 12.3k/50k tokens",
    );
  },
);

Deno.test(
  "formatTagmaStatusLine: unlimited budget renders the localized label",
  () => {
    assertEquals(
      formatTagmaStatusLine({
        rootState: "idle",
        subagentsTotal: 0,
        subagentsActive: 0,
        tokenBudget: 0,
        tokenConsumed: 12_345,
        tokenBudgetUnlimited: true,
      }),
      "0/1 agent · 12.3k/Unlimited tokens",
    );
  },
);

Deno.test("formatRemaining: drops leading zero units", () => {
  // 3 minutes exactly.
  assertEquals(formatRemaining(3 * 60_000), "3min");
  // 2h 3min (no days).
  assertEquals(formatRemaining(2 * 3_600_000 + 3 * 60_000), "2h 3min");
});

Deno.test("formatRemaining: full days/hours/minutes", () => {
  const ms = 1 * 86_400_000 + 2 * 3_600_000 + 3 * 60_000;
  assertEquals(formatRemaining(ms), "1d 2h 3min");
});

Deno.test("formatRemaining: days and minutes with zero hours", () => {
  // 1d 0h 3min -> hours omitted.
  const ms = 1 * 86_400_000 + 3 * 60_000;
  assertEquals(formatRemaining(ms), "1d 3min");
});

// The panel join is a pure projection; these tests pin the key preference
// (process-reported tagma_id first, one-click slug convention as fallback)
// and the unmatched-halves shapes the card list renders.
const presence = () => "checking" as const;
const status = () => undefined;
const ports = {};
const slugFor = (id: string) => `tagma-${id.slice(0, 8)}`;

function proc(
  p: Partial<TagmaProcessLike> & { slug: string },
): TagmaProcessLike {
  return { workspace: "/w", running: true, ...p };
}

Deno.test("joinDeviceRows merges on the process-reported tagma_id", () => {
  const rows = joinDeviceRows(
    [
      {
        tagmaId: "tid-full-uuid",
        label: null,
        createdAt: "2026-08-26T00:00:00Z",
      },
    ],
    [proc({ slug: "team", tagma_id: "tid-full-uuid" })],
    ports,
    slugFor,
    presence,
    status,
  );
  assertEquals(rows.length, 1);
  assertEquals(rows[0].tagma?.tagmaId, "tid-full-uuid");
  assertEquals(rows[0].process?.slug, "team");
});

Deno.test("joinDeviceRows falls back to the slug convention", () => {
  // A one-click-era process (pre-field daemon) whose slug encodes the id.
  const rows = joinDeviceRows(
    [
      {
        tagmaId: "abcdefgh-1234",
        label: null,
        createdAt: "2026-08-26T00:00:00Z",
      },
    ],
    [proc({ slug: "tagma-abcdefgh" })],
    ports,
    slugFor,
    presence,
    status,
  );
  assertEquals(rows.length, 1);
  assertEquals(rows[0].process?.slug, "tagma-abcdefgh");
});

Deno.test("joinDeviceRows keeps unmatched halves as their own rows", () => {
  const rows = joinDeviceRows(
    [
      {
        tagmaId: "enrolled-only",
        label: null,
        createdAt: "2026-08-26T00:00:00Z",
      },
    ],
    [proc({ slug: "local-only" })],
    ports,
    slugFor,
    presence,
    status,
  );
  assertEquals(rows.length, 2);
  assertEquals(rows[0].tagma?.tagmaId, "enrolled-only");
  assertEquals(rows[0].process, undefined);
  assertEquals(rows[1].key, "local-only");
  assertEquals(rows[1].process?.slug, "local-only");
});

Deno.test(
  "joinDeviceRows claims a process once even on colliding identities",
  () => {
    const rows = joinDeviceRows(
      [
        { tagmaId: "tid-a", label: null, createdAt: "2026-08-26T00:00:00Z" },
        { tagmaId: "tid-b", label: null, createdAt: "2026-08-26T00:00:00Z" },
      ],
      [proc({ slug: "tagma-tid-b", tagma_id: "tid-a" })],
      ports,
      slugFor,
      presence,
      status,
    );
    assertEquals(rows.length, 2);
    // tid-b's slug-convention fallback resolves to the same process
    // (slugFor("tid-b") = "tagma-tid-b"): the claim-once guard is what
    // keeps it identity-only — delete the guard and this fails.
    assertEquals(rows[0].process?.slug, "tagma-tid-b");
    assertEquals(rows[1].tagma?.tagmaId, "tid-b");
    assertEquals(rows[1].process, undefined);
  },
);

Deno.test("joinDeviceRows takes the wire port over session memory", () => {
  const rows = joinDeviceRows(
    [{ tagmaId: "tid-x", label: null, createdAt: "2026-08-26T00:00:00Z" }],
    [proc({ slug: "team", tagma_id: "tid-x", port: 7301 })],
    { team: 9999 },
    slugFor,
    presence,
    status,
  );
  assertEquals(rows[0].process?.port, 7301);
});

Deno.test(
  "joinDeviceRows falls back to session memory without a wire port",
  () => {
    const rows = joinDeviceRows(
      [{ tagmaId: "tid-y", label: null, createdAt: "2026-08-26T00:00:00Z" }],
      [proc({ slug: "fresh", tagma_id: "tid-y", port: null })],
      { fresh: 4242 },
      slugFor,
      presence,
      status,
    );
    assertEquals(rows[0].process?.port, 4242);
  },
);
