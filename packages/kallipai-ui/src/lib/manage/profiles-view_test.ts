// Unit tests for the pure view helpers extracted from ProfilesPage: probe
// report merging (keyed maps), draft-derived id lists, probe status labels
// (i18n via paraglide), and the parked-live snapshot core. These pin current
// semantics — the page keeps the $state/$derived/store plumbing.

import { assertEquals } from "@std/assert";
import type {
  AgentStatusResponse,
  ProfileConfig,
  ProfileModelProbeReport,
  ProfileProbeResponse,
  ProfileProbeStatus,
  ProfileProviderProbeReport,
} from "@kallipai/kallipai-client";
import {
  clearProfileResult,
  mergeProfileScope,
  mergeProfileScopeAll,
  mergeProviderScope,
  modelsCountLabel,
  occupiedIdsOf,
  parkedLiveSnapshot,
  probeStatusColor,
  probeStatusLabel,
  profileKey,
  providerIdsOf,
} from "./profiles-view.ts";

// --- fixtures ---

function providerReport(endpointId: string): ProfileProviderProbeReport {
  // Shape-only stub: merge* only reads keys, never report bodies.
  return { endpoint_id: endpointId } as unknown as ProfileProviderProbeReport;
}
function modelReport(): ProfileModelProbeReport {
  return {} as unknown as ProfileModelProbeReport;
}

function probeResponse(
  sets: { name: string; profiles: { profile_id: string }[] }[],
  results: { endpoint_id: string }[] = [],
): ProfileProbeResponse {
  return {
    sets: sets.map((s) => ({ ...s })),
    results: results.map((r) => ({ ...r, status: "ok" })),
  } as unknown as ProfileProbeResponse;
}

function draft(
  endpoints: Record<string, unknown>,
  sets: Record<string, { profiles: { id: string }[] }>,
  parking: { id: string }[] | undefined,
): ProfileConfig {
  return {
    endpoints,
    sets,
    parking,
  } as unknown as ProfileConfig;
}

// --- profileKey ---

Deno.test("profileKey: joins set name and profile id with a colon", () => {
  assertEquals(profileKey("s1", "gpt"), "s1:gpt");
  assertEquals(profileKey("prod", "claude"), "prod:claude");
});

Deno.test("profileKey: plain concatenation, no escaping (pinned)", () => {
  assertEquals(profileKey("s", "x:y"), "s:x:y");
});

// --- mergeProviderScope ---

Deno.test("mergeProviderScope: sets each result by endpoint id", () => {
  const m = new Map<string, ProfileProviderProbeReport>();
  mergeProviderScope(
    m,
    probeResponse(
      [],
      [
        { endpoint_id: "openai" },
        {
          endpoint_id: "anthropic",
        },
      ],
    ),
  );
  assertEquals(m.size, 2);
  assertEquals(m.get("openai")?.endpoint_id, "openai");
  assertEquals(m.get("anthropic")?.endpoint_id, "anthropic");
});

Deno.test("mergeProviderScope: empty results leave the map untouched", () => {
  const m = new Map<string, ProfileProviderProbeReport>([
    ["keep", providerReport("keep")],
  ]);
  mergeProviderScope(m, probeResponse([]));
  assertEquals([...m.keys()], ["keep"]);
});

Deno.test(
  "mergeProviderScope: overwrites a stale entry for the same endpoint",
  () => {
    const m = new Map<string, ProfileProviderProbeReport>();
    mergeProviderScope(m, probeResponse([], [{ endpoint_id: "e" }]));
    mergeProviderScope(m, probeResponse([], [{ endpoint_id: "e" }]));
    assertEquals(m.size, 1);
  },
);

// --- mergeProfileScope ---

Deno.test("mergeProfileScope: keys reports with the setName prefix", () => {
  const m = new Map<string, ProfileModelProbeReport>();
  mergeProfileScope(
    "s2",
    m,
    probeResponse([
      { name: "s2", profiles: [{ profile_id: "a" }, { profile_id: "b" }] },
    ]),
  );
  assertEquals([...m.keys()], ["s2:a", "s2:b"]);
});

Deno.test("mergeProfileScope: missing sets[0] leaves the map untouched", () => {
  const m = new Map<string, ProfileModelProbeReport>();
  mergeProfileScope("s1", m, probeResponse([]));
  assertEquals(m.size, 0);
});

Deno.test(
  "mergeProfileScope: same profile id in different sets keeps both",
  () => {
    const m = new Map<string, ProfileModelProbeReport>();
    mergeProfileScope(
      "s1",
      m,
      probeResponse([{ name: "s1", profiles: [{ profile_id: "dup" }] }]),
    );
    mergeProfileScope(
      "s2",
      m,
      probeResponse([{ name: "s2", profiles: [{ profile_id: "dup" }] }]),
    );
    assertEquals([...m.keys()], ["s1:dup", "s2:dup"]);
  },
);

// --- mergeProfileScopeAll ---

Deno.test("mergeProfileScopeAll: keys by the response set name", () => {
  const m = new Map<string, ProfileModelProbeReport>();
  mergeProfileScopeAll(
    m,
    probeResponse([
      { name: "a", profiles: [{ profile_id: "a" }] },
      { name: "b", profiles: [{ profile_id: "a" }] },
    ]),
  );
  assertEquals([...m.keys()], ["a:a", "b:a"]);
});

Deno.test("mergeProfileScopeAll: empty sets leave the map untouched", () => {
  const m = new Map<string, ProfileModelProbeReport>();
  mergeProfileScopeAll(m, probeResponse([]));
  assertEquals(m.size, 0);
});

// --- clearProfileResult ---

Deno.test("clearProfileResult: deletes an existing entry", () => {
  const m = new Map<string, ProfileModelProbeReport>([
    [profileKey("s1", "a"), modelReport()],
  ]);
  clearProfileResult(m, "s1", "a");
  assertEquals(m.size, 0);
});

Deno.test("clearProfileResult: missing entry is a no-op", () => {
  const m = new Map<string, ProfileModelProbeReport>();
  clearProfileResult(m, "ghost", "nope");
  assertEquals(m.size, 0);
});

// --- providerIdsOf / occupiedIdsOf ---

Deno.test("providerIdsOf: endpoint keys of the draft", () => {
  assertEquals(providerIdsOf(draft({ a: {}, b: {} }, {}, undefined)), [
    "a",
    "b",
  ]);
});

Deno.test("providerIdsOf: missing draft yields []", () => {
  assertEquals(providerIdsOf(undefined), []);
});

Deno.test("occupiedIdsOf: sets then parking, in order", () => {
  const ids = occupiedIdsOf(
    draft(
      {},
      {
        s1: { profiles: [{ id: "t1" }, { id: "t2" }] },
        s2: { profiles: [{ id: "t3" }] },
      },
      [{ id: "p1" }],
    ),
  );
  assertEquals(ids, ["t1", "t2", "t3", "p1"]);
});

Deno.test("occupiedIdsOf: duplicate ids are kept (current semantics)", () => {
  const ids = occupiedIdsOf(
    draft({}, { s1: { profiles: [{ id: "dup" }] } }, [{ id: "dup" }]),
  );
  assertEquals(ids, ["dup", "dup"]);
});

Deno.test("occupiedIdsOf: empty draft yields []", () => {
  assertEquals(occupiedIdsOf(draft({}, {}, undefined)), []);
});

Deno.test("occupiedIdsOf: missing draft yields []", () => {
  assertEquals(occupiedIdsOf(undefined), []);
});

// --- probeStatusLabel / probeStatusColor / modelsCountLabel ---

Deno.test(
  "probeStatusLabel: every status maps to a distinct non-empty label",
  () => {
    const labels = (
      [
        "ok",
        "partial",
        "unreachable",
        "unauthorized",
        "invalid_config",
      ] as const
    ).map((s) => probeStatusLabel(s));
    assertEquals(new Set(labels).size, 5);
  },
);

Deno.test(
  "probeStatusColor: mapping pinned — the three failures share the error color",
  () => {
    assertEquals(probeStatusColor.ok, "text-success-500 dark:text-success-400");
    assertEquals(
      probeStatusColor.partial,
      "text-warning-500 dark:text-warning-400",
    );
    assertEquals(
      probeStatusColor.unreachable,
      "text-error-500 dark:text-error-400",
    );
    assertEquals(probeStatusColor.unauthorized, probeStatusColor.unreachable);
    assertEquals(probeStatusColor.invalid_config, probeStatusColor.unreachable);
  },
);

Deno.test("modelsCountLabel: 1 is singular, 2 is plural", () => {
  assertEquals(modelsCountLabel(1) !== modelsCountLabel(2), true);
});

// --- parkedLiveSnapshot ---

function settled(
  profileId: string | undefined,
): PromiseSettledResult<AgentStatusResponse> {
  return {
    status: "fulfilled",
    value: {
      profile: profileId ? { profile_id: profileId } : undefined,
    } as AgentStatusResponse,
  };
}

const rejected = { status: "rejected" as const, reason: new Error("x") };

Deno.test(
  "parkedLiveSnapshot: counts agents and dedupes ids at the intersection",
  () => {
    const snap = parkedLiveSnapshot(
      ["p1", "p2"],
      [
        settled("p1"),
        settled("p1"),
        settled("p2"),
        settled("other"),
        settled(undefined),
      ],
    );
    assertEquals(snap, { agentCount: 3, profileIds: ["p1", "p2"] });
  },
);

Deno.test(
  "parkedLiveSnapshot: rejected statuses are skipped, not fatal",
  () => {
    const snap = parkedLiveSnapshot(["p1"], [rejected, settled("p1")]);
    assertEquals(snap, { agentCount: 1, profileIds: ["p1"] });
  },
);

Deno.test("parkedLiveSnapshot: no intersection yields null", () => {
  assertEquals(parkedLiveSnapshot(["p1"], [settled("other")]), null);
  assertEquals(parkedLiveSnapshot(["p1"], [rejected]), null);
  assertEquals(parkedLiveSnapshot([], [settled("p1")]), null);
});
