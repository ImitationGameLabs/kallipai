import { assertEquals } from "@std/assert";
import {
  collectionRowKey,
  derivedPullKey,
  gatewayTabPosture,
  pullSelectionDirty,
  saveButtonEnabled,
  selectionWire,
} from "./profilesGatewayTab.ts";
import type {
  UserCollectionRow,
  UserPlatformCollectionRow,
} from "../gateway/client.ts";

function own(name: string, sets: string[]): UserCollectionRow {
  return { name, description: "", sets };
}

function catalog(
  owner: string,
  name: string,
  sets: string[],
): UserPlatformCollectionRow {
  return { owner, name, description: "", sets };
}

Deno.test(
  "collectionRowKey: own rows by name, catalog rows by owner/name",
  () => {
    assertEquals(collectionRowKey(own("alpha", [])), "alpha");
    assertEquals(
      collectionRowKey(catalog("acc-x", "alpha", [])),
      "acc-x/alpha",
    );
  },
);

Deno.test(
  "derivedPullKey: the covering row with the fewest filtered members wins",
  () => {
    const rows = [
      own("both", ["a", "b"]),
      own("partial", ["a", "gone"]),
      own("narrow", ["a"]),
      own("empty", []),
    ];
    // The selected row covers: it carries every live set, and the
    // members beyond the live face are the ones the gateway filtered.
    assertEquals(derivedPullKey(rows, ["a", "b"]), "both");
    // An exact match derives with zero filtered members; the wider
    // covers lose to it.
    assertEquals(derivedPullKey(rows, ["a"]), "narrow");
    // A live face no row contains in full, an empty live snapshot,
    // and no rows at all derive nothing.
    assertEquals(derivedPullKey(rows, ["a", "b", "c"]), null);
    assertEquals(derivedPullKey(rows, []), null);
    assertEquals(derivedPullKey([], ["a"]), null);
    // A catalog row derives its owner/name key.
    assertEquals(
      derivedPullKey([catalog("acc-x", "beta", ["a"])], ["a"]),
      "acc-x/beta",
    );
  },
);

Deno.test(
  "derivedPullKey: a filtered-away member never disqualifies the cover",
  () => {
    // The gateway filtered "c" away: the row that still carries it
    // covers the live face and beats a narrower row that misses one.
    const selected = own("selected", ["a", "b", "c"]);
    const narrow = own("narrow", ["a"]);
    assertEquals(derivedPullKey([narrow, selected], ["a", "b"]), "selected");
    // Covers with equal filtered counts keep row order.
    const twin = own("twin", ["a", "b", "x"]);
    const other = own("other", ["a", "b", "y"]);
    assertEquals(derivedPullKey([twin, other], ["a", "b"]), "twin");
    assertEquals(derivedPullKey([other, twin], ["a", "b"]), "other");
  },
);

Deno.test("selectionWire: the picked row's wire member", () => {
  const rows = [own("alpha", ["a"]), catalog("acc-x", "beta", ["b"])];
  assertEquals(selectionWire(rows, "alpha"), { collection: "alpha" });
  assertEquals(selectionWire(rows, "acc-x/beta"), {
    owner: "acc-x",
    collection: "beta",
  });
  // A stale key names no row: nothing rides the wire.
  assertEquals(selectionWire(rows, "gone"), null);
});

Deno.test("gatewayTabPosture: the local three states", () => {
  const base = {
    sourceMode: "local",
    proxyAvailable: true,
    online: true,
    sessionReady: true,
    servingPolis: "https://polis.test",
    connectedPolis: null,
  };
  // Local on an enrolled serving platform: the switch entry with the
  // editable pre-switch choice.
  const enrolled = gatewayTabPosture({
    ...base,
    platforms: [{ origin: "https://polis.test", enrolled: true }],
  });
  assertEquals(enrolled.showGatewayTab, true);
  assertEquals(enrolled.servingPlatformEnrolled, true);
  assertEquals(enrolled.browseFace, true);
  assertEquals(enrolled.activeHere, false);
  assertEquals(enrolled.selectionEditable, true);

  // Enrolled somewhere else: the tab teaches this platform's
  // enrollment; nothing is actionable.
  const elsewhere = gatewayTabPosture({
    ...base,
    platforms: [{ origin: "https://other.test", enrolled: true }],
  });
  assertEquals(elsewhere.showGatewayTab, true);
  assertEquals(elsewhere.servingPlatformEnrolled, false);
  assertEquals(elsewhere.browseFace, false);
  assertEquals(elsewhere.selectionEditable, false);

  // Nothing enrolled anywhere: the local-only page.
  const bare = gatewayTabPosture({
    ...base,
    platforms: [],
    proxyAvailable: false,
  });
  assertEquals(bare.showGatewayTab, false);
  assertEquals(bare.selectionEditable, false);
});

Deno.test("gatewayTabPosture: the connected mirror states", () => {
  const base = {
    sourceMode: "model-gateway",
    proxyAvailable: true,
    online: true,
    sessionReady: true,
    servingPolis: "https://polis.test",
    platforms: [{ origin: "https://polis.test", enrolled: true }],
  };
  // Connected here: the narrowing edits act on the live face.
  const here = gatewayTabPosture({
    ...base,
    connectedPolis: "https://polis.test",
  });
  assertEquals(here.activeHere, true);
  assertEquals(here.selectionEditable, true);
  assertEquals(here.browseFace, true);

  // Connected to another platform: a plain browse; the selection only
  // makes sense as part of a rebind.
  const away = gatewayTabPosture({
    ...base,
    connectedPolis: "https://other.test",
  });
  assertEquals(away.activeHere, false);
  assertEquals(away.selectionEditable, false);
  assertEquals(away.browseFace, true);
});

Deno.test("gatewayTabPosture: the offline direct shell", () => {
  // The direct shell has no platform context: the mirror stays (a
  // gateway-sourced daemon still shows it) but nothing is actionable,
  // and a local daemon keeps the local-only page.
  const mirror = gatewayTabPosture({
    sourceMode: "model-gateway",
    proxyAvailable: true,
    online: false,
    sessionReady: false,
    servingPolis: null,
    connectedPolis: "https://polis.test",
    platforms: [],
  });
  assertEquals(mirror.showGatewayTab, true);
  assertEquals(mirror.browseFace, false);
  assertEquals(mirror.activeHere, false);

  const local = gatewayTabPosture({
    sourceMode: "local",
    proxyAvailable: true,
    online: false,
    sessionReady: false,
    servingPolis: null,
    connectedPolis: null,
    platforms: [],
  });
  assertEquals(local.showGatewayTab, false);
});

Deno.test(
  "pullSelectionDirty: a touched pick departs from any baseline, including none",
  () => {
    // An empty live snapshot (no selection yet) is a baseline: the
    // first choice is saveable.
    assertEquals(pullSelectionDirty(true, "system/baseline", null), true);
    // No touched pick, already on the baseline, or not connected
    // here: nothing to save.
    assertEquals(pullSelectionDirty(true, null, null), false);
    assertEquals(pullSelectionDirty(true, null, "system/baseline"), false);
    assertEquals(
      pullSelectionDirty(true, "system/baseline", "system/baseline"),
      false,
    );
    assertEquals(pullSelectionDirty(false, "system/baseline", null), false);
    // A pick against a live baseline: dirty.
    assertEquals(pullSelectionDirty(true, "own", "system/baseline"), true);
  },
);

Deno.test("saveButtonEnabled: the diagonal of mode-exclusive pendings", () => {
  // Read-only mirror (gateway source): the pick is the only
  // saveable change, and a first choice on an empty baseline
  // enables save (the unified flow keeps it reachable).
  assertEquals(saveButtonEnabled(true, false, true), true);
  assertEquals(saveButtonEnabled(true, false, false), false);
  assertEquals(saveButtonEnabled(true, true, false), false);
  assertEquals(saveButtonEnabled(true, true, true), true);
  // Editable page (local source): the form is the saveable
  // change; the pick cannot be dirty here (never active
  // here), so its flag must not leak into enablement.
  assertEquals(saveButtonEnabled(false, true, false), true);
  assertEquals(saveButtonEnabled(false, false, false), false);
  assertEquals(saveButtonEnabled(false, true, true), true);
  assertEquals(saveButtonEnabled(false, false, true), false);
});
