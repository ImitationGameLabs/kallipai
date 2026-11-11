// The Profiles page's drag lifecycle: which card is dragged, which drop area is
// hovered, and what a drop does to the draft and to the report keys. A drop
// clears the drag state before it touches the draft, so a refused move cannot
// leave the page armed with a payload it already consumed.
//
// The module is rune-bearing, so a passthrough $state shim with a snapshot
// member lets it run uncompiled (the profiles-store_test pattern).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  namespace $state {
    function snapshot<T>(v: T): T;
  }
}

(globalThis as Record<string, unknown>)["$state"] = Object.assign(
  (v: unknown) => v,
  {
    snapshot: (v: unknown) => v,
  },
);

const { assertEquals } = await import("@std/assert");
const { createProfilesDrag } = await import("./profilesDrag.svelte.ts");
import type { ProfilesProbeCalls } from "./profilesProbes.svelte.ts";
import type { ProfilesDragStore } from "./profilesDrag.svelte.ts";

import type { ProfileConfig } from "@kallipai/kallipai-client";

function row(id: string): Record<string, unknown> {
  return {
    id,
    endpoint: "openai",
    model: id,
    max_context_window: 1000,
  };
}

/** Two sets of one row each, plus one parked row. */
function draftWith(): ProfileConfig {
  return {
    endpoints: {},
    sets: {
      s1: { description: null, profiles: [row("m1")] },
      s2: { description: null, profiles: [row("m2")] },
    },
    parking: [row("u1")],
    default: "s1",
  } as unknown as ProfileConfig;
}

function harness(initial: ProfileConfig | null) {
  const clearedSetProfiles: string[] = [];
  const clearedUnassigned: string[] = [];
  const state = { draft: initial, refreshed: 0 };
  const store = {
    get draft() {
      return state.draft;
    },
    set draft(next: ProfileConfig | null) {
      state.draft = next;
    },
  } as unknown as ProfilesDragStore;
  const probes = {
    clearSetProfile: (setName: string, id: string) => {
      clearedSetProfiles.push(`${setName}:${id}`);
    },
    clearUnassignedProfile: (id: string) => {
      clearedUnassigned.push(id);
    },
  } as unknown as ProfilesProbeCalls;
  const drag = createProfilesDrag({
    store,
    probes,
    refreshUnassignedLive: () => {
      state.refreshed += 1;
      return Promise.resolve();
    },
  });
  return {
    drag,
    clearedSetProfiles,
    clearedUnassigned,
    draft: () => state.draft,
    refreshed: () => state.refreshed,
    setDraft: (next: ProfileConfig | null) => {
      state.draft = next;
    },
  };
}

// --- hover state ---

Deno.test(
  "hover: leaving a set that is not the hovered one keeps the hover",
  () => {
    const h = harness(draftWith());
    h.drag.enterSet("s1");
    h.drag.leaveSet("s2");
    assertEquals(h.drag.overSet, "s1");
    h.drag.leaveSet("s1");
    assertEquals(h.drag.overSet, null);
  },
);

Deno.test("hover: the unassigned area toggles on enter and leave", () => {
  const h = harness(draftWith());
  h.drag.enterUnassigned();
  assertEquals(h.drag.overUnassigned, true);
  h.drag.leaveUnassigned();
  assertEquals(h.drag.overUnassigned, false);
});

// --- drops ---

Deno.test("dropToSet: the drag state clears even with no draft to move", () => {
  const h = harness(null);
  h.drag.start({ area: "set", fromSet: "s1", fromIdx: 0 });
  h.drag.enterSet("s2");
  h.drag.dropToSet("s2");
  assertEquals(h.drag.current, null);
  assertEquals(h.drag.overSet, null);
});

Deno.test(
  "dropToSet: a cross-set move clears the source report key and moves the row",
  () => {
    const h = harness(draftWith());
    h.drag.start({ area: "set", fromSet: "s1", fromIdx: 0 });
    h.drag.dropToSet("s2");
    const draft = h.draft()!;
    assertEquals(draft.sets.s1.profiles, []);
    assertEquals(
      draft.sets.s2.profiles.map((p) => p.id),
      ["m2", "m1"],
    );
    assertEquals(h.clearedSetProfiles, ["s1:m1"]);
  },
);

Deno.test("dropToSet: a reorder inside one set keeps its report key", () => {
  const h = harness(draftWith());
  h.drag.start({ area: "set", fromSet: "s1", fromIdx: 0 });
  h.drag.dropToSet("s1");
  assertEquals(h.clearedSetProfiles, []);
});

Deno.test(
  "dropToSet: an unassigned row clears its p: key and lands in the set",
  () => {
    const h = harness(draftWith());
    h.drag.start({ area: "unassigned", fromSet: null, fromIdx: 0 });
    h.drag.dropToSet("s2");
    const draft = h.draft()!;
    assertEquals(draft.parking, undefined);
    assertEquals(
      draft.sets.s2.profiles.map((p) => p.id),
      ["m2", "u1"],
    );
    assertEquals(h.clearedUnassigned, ["u1"]);
    assertEquals(h.refreshed(), 1);
  },
);

Deno.test(
  "dropToUnassigned: a set member clears its set key and lands parked",
  () => {
    const h = harness(draftWith());
    h.drag.start({ area: "set", fromSet: "s1", fromIdx: 0 });
    h.drag.dropToUnassigned();
    const draft = h.draft()!;
    assertEquals(draft.sets.s1.profiles, []);
    assertEquals(
      draft.parking?.map((p) => p.id),
      ["u1", "m1"],
    );
    assertEquals(h.clearedSetProfiles, ["s1:m1"]);
    assertEquals(h.refreshed(), 1);
  },
);

Deno.test(
  "dropToUnassigned: an unassigned payload leaves the draft alone",
  () => {
    const initial = draftWith();
    const h = harness(initial);
    h.drag.start({ area: "unassigned", fromSet: null, fromIdx: 0 });
    h.drag.dropToUnassigned();
    assertEquals(h.draft(), initial);
    assertEquals(h.clearedUnassigned, []);
  },
);

Deno.test("a drop with no payload moves nothing", () => {
  const initial = draftWith();
  const h = harness(initial);
  h.drag.dropToSet("s2");
  assertEquals(h.draft(), initial);
  assertEquals(h.clearedSetProfiles, []);
});
