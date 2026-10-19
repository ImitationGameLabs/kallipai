import { assertEquals } from "@std/assert";
import { joinDeviceRows } from "./tagmata.svelte.ts";
import type { TagmaProcessLike, TagmaStatusSummary } from "./tagmata.svelte.ts";
import {
  activeAgents,
  formatRemaining,
  formatTagmaStatusLine,
  totalAgents,
} from "./tagmata.svelte.ts";

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

// Every status surface renders this one aggregation: the root holds a slot
// in the roster whether or not it is busy, and counts toward the active
// numerator only while it runs.
function snap(over: Partial<TagmaStatusSummary>): TagmaStatusSummary {
  return {
    rootState: "idle",
    subagentsTotal: 0,
    subagentsActive: 0,
    tokenBudget: 0,
    tokenConsumed: 0,
    tokenBudgetUnlimited: false,
    ...over,
  };
}

Deno.test("activeAgents: a busy root joins the numerator and the total", () => {
  const s = snap({ rootState: "busy", subagentsTotal: 3, subagentsActive: 1 });
  assertEquals(activeAgents(s), 2);
  assertEquals(totalAgents(s), 4);
});

Deno.test(
  "activeAgents: an idle root holds a slot but adds no activity",
  () => {
    const s = snap({
      rootState: "idle",
      subagentsTotal: 3,
      subagentsActive: 1,
    });
    assertEquals(activeAgents(s), 1);
    assertEquals(totalAgents(s), 4);
  },
);

Deno.test("activeAgents: a faulted root still holds its slot", () => {
  const s = snap({
    rootState: "faulted",
    subagentsTotal: 3,
    subagentsActive: 3,
  });
  assertEquals(activeAgents(s), 3);
  assertEquals(totalAgents(s), 4);
});

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

// The panel join is a pure projection; these tests pin the key preference (an
// identity entry the process reports for this page's polis first, the one-click
// slug convention as fallback) and the unmatched-halves shapes the card list
// renders.
const presence = () => "checking" as const;
const status = () => undefined;
const ports = {};
const slugFor = (id: string) => `tagma-${id.slice(0, 8)}`;
const origin = "https://api.kallipai.com";
const otherOrigin = "https://api.kallipai.lan";

function proc(
  p: Partial<TagmaProcessLike> & { slug: string },
): TagmaProcessLike {
  return { workspace: "/w", running: true, ...p };
}

Deno.test(
  "joinDeviceRows merges on the identity reported for this polis",
  () => {
    const rows = joinDeviceRows(
      [
        {
          tagmaId: "tid-full-uuid",
          label: null,
          createdAt: "2026-08-26T00:00:00Z",
        },
      ],
      [
        proc({
          slug: "team",
          identities: [{ polis: origin, tagma_id: "tid-full-uuid" }],
        }),
      ],
      ports,
      origin,
      slugFor,
      presence,
      status,
    );
    assertEquals(rows.length, 1);
    assertEquals(rows[0].tagma?.tagmaId, "tid-full-uuid");
    assertEquals(rows[0].process?.slug, "team");
  },
);

Deno.test(
  "joinDeviceRows ignores identities reported for another polis",
  () => {
    // The process is enrolled, just not here: it keeps its own hosted-process
    // card and the identity card stays unmerged.
    const rows = joinDeviceRows(
      [
        {
          tagmaId: "tid-remote",
          label: null,
          createdAt: "2026-08-26T00:00:00Z",
        },
      ],
      [
        proc({
          slug: "team",
          identities: [{ polis: otherOrigin, tagma_id: "tid-remote" }],
        }),
      ],
      ports,
      origin,
      slugFor,
      presence,
      status,
    );
    assertEquals(rows.length, 2);
    assertEquals(rows[0].tagma?.tagmaId, "tid-remote");
    assertEquals(rows[0].process, undefined);
    assertEquals(rows[1].key, "team");
    assertEquals(rows[1].process?.slug, "team");
  },
);

Deno.test("joinDeviceRows picks this polis' entry among several", () => {
  const rows = joinDeviceRows(
    [
      {
        tagmaId: "tid-mine",
        label: null,
        createdAt: "2026-08-26T00:00:00Z",
      },
    ],
    [
      proc({
        slug: "team",
        identities: [
          { polis: otherOrigin, tagma_id: "tid-theirs" },
          { polis: origin, tagma_id: "tid-mine" },
        ],
      }),
    ],
    ports,
    origin,
    slugFor,
    presence,
    status,
  );
  assertEquals(rows.length, 1);
  assertEquals(rows[0].tagma?.tagmaId, "tid-mine");
  assertEquals(rows[0].process?.slug, "team");
});

Deno.test(
  "joinDeviceRows matches polis origins modulo trailing slashes",
  () => {
    const rows = joinDeviceRows(
      [
        {
          tagmaId: "tid-slash",
          label: null,
          createdAt: "2026-08-26T00:00:00Z",
        },
      ],
      [
        proc({
          slug: "team",
          identities: [{ polis: `${origin}/`, tagma_id: "tid-slash" }],
        }),
      ],
      ports,
      `${origin}/`,
      slugFor,
      presence,
      status,
    );
    assertEquals(rows.length, 1);
    assertEquals(rows[0].process?.slug, "team");
  },
);

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
    origin,
    slugFor,
    presence,
    status,
  );
  assertEquals(rows.length, 1);
  assertEquals(rows[0].process?.slug, "tagma-abcdefgh");
});

Deno.test(
  "joinDeviceRows without a polis origin keeps to the slug convention",
  () => {
    // The offline shell has no polis: reported identities never match, so the
    // page behaves exactly as it does without them.
    const rows = joinDeviceRows(
      [
        {
          tagmaId: "abcdefgh-1234",
          label: null,
          createdAt: "2026-08-26T00:00:00Z",
        },
      ],
      [
        proc({
          slug: "tagma-abcdefgh",
          identities: [{ polis: origin, tagma_id: "abcdefgh-1234" }],
        }),
      ],
      ports,
      "",
      slugFor,
      presence,
      status,
    );
    assertEquals(rows.length, 1);
    assertEquals(rows[0].process?.slug, "tagma-abcdefgh");
  },
);

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
    origin,
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
      [
        proc({
          slug: "tagma-tid-b",
          identities: [{ polis: origin, tagma_id: "tid-a" }],
        }),
      ],
      ports,
      origin,
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
    [
      proc({
        slug: "team",
        identities: [{ polis: origin, tagma_id: "tid-x" }],
        port: 7301,
      }),
    ],
    { team: 9999 },
    origin,
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
      [
        proc({
          slug: "fresh",
          identities: [{ polis: origin, tagma_id: "tid-y" }],
          port: null,
        }),
      ],
      { fresh: 4242 },
      origin,
      slugFor,
      presence,
      status,
    );
    assertEquals(rows[0].process?.port, 4242);
  },
);

Deno.test(
  "joinDeviceRows without an origin claims nothing even for a blank polis",
  () => {
    // A blank origin must not match a blank polis: nothing is claimed, so the
    // process keeps its slug-convention card instead of merging by id.
    const rows = joinDeviceRows(
      [
        {
          tagmaId: "tid-blank",
          label: null,
          createdAt: "2026-08-26T00:00:00Z",
        },
      ],
      [
        proc({
          slug: "team",
          identities: [{ polis: "  ", tagma_id: "tid-blank" }],
        }),
      ],
      ports,
      "",
      slugFor,
      presence,
      status,
    );
    assertEquals(rows.length, 2);
    assertEquals(rows[0].process, undefined);
  },
);

Deno.test("joinDeviceRows reads identities in the wire shape", () => {
  // The daemon serializes InstanceIdentity as {"polis","tagma_id"}: a rename
  // on either side leaves this joining nothing.
  const instance = JSON.parse(
    '{"slug":"team","workspace":"/w","running":true,' +
      '"identities":[{"polis":"https://api.kallipai.com","tagma_id":"tid-wire"}]}',
  );
  const rows = joinDeviceRows(
    [{ tagmaId: "tid-wire", label: null, createdAt: "2026-08-26T00:00:00Z" }],
    [instance],
    ports,
    origin,
    slugFor,
    presence,
    status,
  );
  assertEquals(rows.length, 1);
  assertEquals(rows[0].process?.slug, "team");
});

Deno.test(
  "joinDeviceRows folds host case, default ports and paths like the tagma",
  () => {
    // The enrollment side compares `url::Url::origin` values, so these three
    // spellings all name this page's polis and must merge.
    for (const polis of [
      "https://API.Kallipai.com",
      "https://api.kallipai.com:443",
      "https://api.kallipai.com/some/path",
    ]) {
      const rows = joinDeviceRows(
        [
          {
            tagmaId: "tid-same",
            label: null,
            createdAt: "2026-08-26T00:00:00Z",
          },
        ],
        [proc({ slug: "team", identities: [{ polis, tagma_id: "tid-same" }] })],
        ports,
        origin,
        slugFor,
        presence,
        status,
      );
      assertEquals(rows.length, 1, polis);
      assertEquals(rows[0].process?.slug, "team", polis);
    }
  },
);

Deno.test("joinDeviceRows does not merge an origin that does not parse", () => {
  // A malformed value cannot be compared, and a wrong merge would show the
  // wrong identity: the process keeps its own card.
  const rows = joinDeviceRows(
    [{ tagmaId: "tid-bad", label: null, createdAt: "2026-08-26T00:00:00Z" }],
    [
      proc({
        slug: "team",
        identities: [{ polis: "not a url", tagma_id: "tid-bad" }],
      }),
    ],
    ports,
    origin,
    slugFor,
    presence,
    status,
  );
  assertEquals(rows.length, 2);
  assertEquals(rows[0].process, undefined);
});

Deno.test("joinDeviceRows compares a value it cannot parse as text", () => {
  // The fallback keeps the pre-parse reading: an unparseable page origin and
  // an identity that normalizes to the same text still merge, so a fallback
  // that returns nothing (or an empty value) instead of the value would leave
  // that identity on its own card.
  const rows = joinDeviceRows(
    [
      {
        tagmaId: "tid-fallback",
        label: null,
        createdAt: "2026-08-26T00:00:00Z",
      },
    ],
    [
      proc({
        slug: "team",
        identities: [{ polis: "not a url/", tagma_id: "tid-fallback" }],
      }),
    ],
    ports,
    "not a url",
    slugFor,
    presence,
    status,
  );
  assertEquals(rows.length, 1);
  assertEquals(rows[0].tagma?.tagmaId, "tid-fallback");
  assertEquals(rows[0].process?.slug, "team");
});
