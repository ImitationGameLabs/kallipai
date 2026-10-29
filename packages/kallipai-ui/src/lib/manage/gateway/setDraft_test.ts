import { assert, assertEquals, assertThrows } from "@std/assert";
import type { AdminProfileRow, AdminSetRow } from "./client.ts";
import {
  adminMemberBody,
  changedUpdates,
  contextProblem,
  editProblem,
  firstOffending,
  memberEditOf,
  memberIds,
  modelProblem,
  moveMember,
  orderChanged,
  parseContextWindow,
  reseed,
  setsAlsoHolding,
  type MemberEdit,
} from "./setDraft.ts";

// The set dialog's draft rules, driven directly: reordering at the edges,
// the id order the PATCH carries, which rows a save must PUT, and which
// context windows the catalog takes.

function row(
  id: string,
  overrides: Partial<AdminProfileRow> = {},
): AdminProfileRow {
  return {
    profile_id: id,
    collection_name: "baseline",
    provider_id: id,
    owner: "system",
    family: "deepseek",
    model: `${id}-model`,
    max_context_window: 128_000,
    effort: "high",
    modalities: ["text"],
    parked: false,
    max_budget: null,
    tpm_limit: null,
    rpm_limit: null,
    ...overrides,
  };
}

Deno.test("moveMember swaps within the list and refuses the edges", () => {
  const list = [row("p1"), row("p2"), row("p6")];
  assertEquals(memberIds(moveMember(list, 1, -1)), ["p2", "p1", "p6"]);
  assertEquals(memberIds(moveMember(list, 0, 1)), ["p2", "p1", "p6"]);
  assertEquals(memberIds(moveMember(list, 0, 2)), ["p2", "p6", "p1"]);
  // A refused move answers the same reference: the caller keeps the list
  // it had, and a disabled step is not a no-op copy.
  assert(moveMember(list, 0, -1) === list, "the first member cannot move up");
  assert(moveMember(list, 2, 1) === list, "the last member cannot move down");
  assert(moveMember(list, 1, 0) === list, "a zero step is not a move");
  assert(
    moveMember(list, 5, -1) === list,
    "an index outside the list is refused",
  );
});

Deno.test("memberIds reports the draft order verbatim", () => {
  assertEquals(memberIds([]), []);
  assertEquals(memberIds([row("p6"), row("p1")]), ["p6", "p1"]);
});

Deno.test("memberEditOf projects the wire row onto the editable fields", () => {
  const projected = memberEditOf(
    row("p3", {
      max_context_window: null,
      effort: null,
      modalities: ["text", "image"],
    }),
  );
  assertEquals(projected, {
    model: "p3-model",
    context_window: "",
    effort: null,
    modalities: ["text", "image"],
  });
});

Deno.test("memberEditOf spells a stored window as the field's text", () => {
  assertEquals(
    memberEditOf(row("p8", { max_context_window: 128_000 })).context_window,
    "128000",
  );
});

Deno.test("contextProblem takes an empty field and whole counts", () => {
  const edit = (value: string): MemberEdit => ({
    ...memberEditOf(row("p1")),
    context_window: value,
  });
  // Empty is no limit (the wire takes null), not a typo.
  assertEquals(parseContextWindow(""), null);
  assertEquals(parseContextWindow("   "), null);
  assert(!contextProblem(edit("")));
  assert(!contextProblem(edit("128000")));
  assertEquals(parseContextWindow(" 64000 "), 64_000);
  // A sign, a fraction, text, and zero are all typos.
  for (const text of ["-5", "1.5", "abc", "0", "12px"]) {
    assertEquals(parseContextWindow(text), undefined);
    assert(contextProblem(edit(text)), `${text} is not a window`);
  }
});

Deno.test("changedUpdates sends only the rows whose content differs", () => {
  const original = [row("p1"), row("p2"), row("p6")];
  // Nothing edited: no PUT at all, even though the order may have moved.
  assertEquals(
    changedUpdates(original, [row("p6"), row("p1"), row("p2")], {}),
    [],
  );

  const edits: Record<string, MemberEdit> = {
    p1: memberEditOf(original[0]!),
    p2: { ...memberEditOf(original[1]!), model: "deepseek-next" },
    p6: { ...memberEditOf(original[2]!), modalities: ["text", "image"] },
  };
  const updates = changedUpdates(
    original,
    [row("p6"), row("p1"), row("p2")],
    edits,
  );
  // Display order, not the original order: p6 sits first in the draft.
  assertEquals(
    updates.map((u) => u.profileId),
    ["p6", "p2"],
  );
  assertEquals(updates[0]!.body.modalities, ["text", "image"]);
  assertEquals(updates[1]!.body.model, "deepseek-next");
  // The shared body carries the fields both faces own; the catalog adds
  // the served flag at its own port.
  assert(
    updates.every((u) => !("parked" in u.body)),
    "the shared PUT body has no parked key",
  );
});

Deno.test("adminMemberBody adds the served flag at the catalog port", () => {
  const original = [row("p1")];
  const edits: Record<string, MemberEdit> = {
    p1: { ...memberEditOf(original[0]!), model: "next" },
  };
  const [update] = changedUpdates(original, original, edits);
  // The shared body carries only what both faces own; the catalog's
  // profile route requires the flag, so its port adds it there.
  assert(!("parked" in update!.body), "the shared body stays free of it");
  assertEquals(adminMemberBody(update!.body), {
    model: "next",
    max_context_window: 128_000,
    effort: "high",
    modalities: null,
    parked: false,
  });
});

Deno.test(
  "changedUpdates treats a reordered modality list as the same content",
  () => {
    const original = [row("p4", { modalities: ["text", "image"] })];
    const edits: Record<string, MemberEdit> = {
      p4: { ...memberEditOf(original[0]!), modalities: ["image", "text"] },
    };
    assertEquals(changedUpdates(original, original, edits), []);
  },
);

Deno.test("memberEditOf projects the stored tags into canonical order", () => {
  // The tag group renders the canonical sequence, so a stored order that
  // disagrees is not a selection difference the dialog could show.
  assertEquals(
    memberEditOf(row("p7", { modalities: ["audio", "text"] })).modalities,
    ["text", "audio"],
  );
});

Deno.test("changedUpdates spells a text-only selection as null", () => {
  const original = [row("p5", { modalities: ["text", "image"] })];
  const edits: Record<string, MemberEdit> = {
    p5: { ...memberEditOf(original[0]!), modalities: ["text"] },
  };
  const updates = changedUpdates(original, original, edits);
  assertEquals(updates.length, 1);
  // Text only is the server default and has no list spelling of its own.
  assertEquals(updates[0]!.body.modalities, null);
});

Deno.test("changedUpdates spells the window as the wire takes it", () => {
  const original = [row("p1")];
  const cleared: Record<string, MemberEdit> = {
    p1: { ...memberEditOf(original[0]!), context_window: "" },
  };
  assertEquals(
    changedUpdates(original, original, cleared)[0]!.body.max_context_window,
    null,
  );
  const raised: Record<string, MemberEdit> = {
    p1: { ...memberEditOf(original[0]!), context_window: "256000" },
  };
  assertEquals(
    changedUpdates(original, original, raised)[0]!.body.max_context_window,
    256_000,
  );
});

Deno.test("a draft that never passed the gate cannot become a PUT", () => {
  const original = [row("p1")];
  const bad: Record<string, MemberEdit> = {
    p1: { ...memberEditOf(original[0]!), model: "next", context_window: "-5" },
  };
  assertThrows(
    () => changedUpdates(original, original, bad),
    Error,
    "not a positive whole number",
  );
});

Deno.test("modelProblem wants a trimmed id the server can serve", () => {
  const edit = (model: string): MemberEdit => ({
    ...memberEditOf(row("p1")),
    model,
  });
  assert(modelProblem(edit("")));
  assert(modelProblem(edit("   ")));
  assert(!modelProblem(edit("deepseek-chat")));
  // A model problem and a window problem each block a save on their own.
  assert(editProblem(edit("")));
  assert(editProblem({ ...edit("deepseek-chat"), context_window: "-5" }));
  assert(!editProblem(edit("deepseek-chat")));
});

Deno.test("changedUpdates trims the model before it compares or sends", () => {
  const original = [row("p1", { model: "deepseek-chat" })];
  const padded: Record<string, MemberEdit> = {
    p1: { ...memberEditOf(original[0]!), model: "  deepseek-chat  " },
  };
  // Trailing spaces are typing, not a new id: nothing to PUT.
  assertEquals(changedUpdates(original, original, padded), []);
  const typed: Record<string, MemberEdit> = {
    p1: { ...memberEditOf(original[0]!), model: " deepseek-next " },
  };
  const updates = changedUpdates(original, original, typed);
  assertEquals(updates.length, 1);
  assertEquals(updates[0]!.body.model, "deepseek-next");
});

Deno.test("setsAlsoHolding names the other sets of the collection", () => {
  const target: AdminSetRow = {
    collection_name: "acme",
    name: "primary",
    description: "",
    profiles: [row("p1"), row("p2")],
  };
  const siblings: AdminSetRow[] = [
    target,
    {
      collection_name: "acme",
      name: "backup",
      description: "",
      profiles: [row("p1"), row("p6")],
    },
    {
      collection_name: "acme",
      name: "spare",
      description: "",
      profiles: [row("p1")],
    },
  ];
  assertEquals(setsAlsoHolding(target, siblings), {
    p1: ["backup", "spare"],
  });
});

Deno.test(
  "an untouched order comes back whole with the registry's rows",
  () => {
    const registry = [row("p2"), row("p1", { model: "p1-fresh" })];
    const next = reseed({
      registry,
      members: [row("p1"), row("p2")],
      edits: {
        p1: { ...memberEditOf(row("p1")), model: "p1-draft" },
        p2: { ...memberEditOf(row("p2")), model: "p2-draft" },
      },
      editingId: "p1",
      landed: ["p2"],
      // The seed carried p1 then p2: the draft order is still the seed's.
      seededIds: ["p1", "p2"],
    });
    // A sequence nobody moved is the registry's, not the draft's.
    assertEquals(memberIds(next.members), ["p2", "p1"]);
    assertEquals(next.members[1]!.model, "p1-fresh");
    // The row whose PUT landed has nothing left to defend.
    assertEquals(Object.keys(next.edits), ["p1"]);
    assertEquals(next.edits.p1!.model, "p1-draft");
    assertEquals(next.editingId, "p1");
  },
);

Deno.test("a touched order keeps the draft sequence over fresh values", () => {
  const registry = [row("p1"), row("p2", { model: "p2-fresh" }), row("p3")];
  const next = reseed({
    registry,
    // p3 left and p2 moved up, so the sequence is the user's.
    members: [row("p2", { model: "p2-draft" }), row("p1")],
    edits: { p2: { ...memberEditOf(row("p2")), model: "p2-draft" } },
    editingId: "p2",
    landed: [],
    // The seed carried p3 too, so the ids already differ from it.
    seededIds: ["p1", "p2", "p3"],
  });
  assertEquals(memberIds(next.members), ["p2", "p1"]);
  // A row that stayed carries the registry's values, not the draft's.
  assertEquals(next.members[0]!.model, "p2-fresh");
  // Its draft stays a draft until a save lands it.
  assertEquals(next.edits.p2!.model, "p2-draft");
  assertEquals(next.editingId, "p2");
});

Deno.test("a save that landed every row leaves nothing drafted", () => {
  const registry = [row("p1"), row("p2")];
  const next = reseed({
    registry,
    members: [row("p2"), row("p1")],
    edits: {
      p1: { ...memberEditOf(row("p1")), model: "p1-draft" },
      p2: { ...memberEditOf(row("p2")), model: "p2-draft" },
    },
    editingId: "p2",
    landed: ["p1", "p2"],
    seededIds: ["p1", "p2"],
  });
  assertEquals(memberIds(next.members), ["p2", "p1"]);
  assertEquals(next.edits, {});
  assertEquals(next.editingId, null);
});

Deno.test("a row the registry dropped leaves no member and no form", () => {
  const next = reseed({
    registry: [row("p1")],
    members: [row("p1"), row("p9")],
    edits: { p9: memberEditOf(row("p9")) },
    editingId: "p9",
    landed: [],
    // p9 was in the seed: the registry no longer holds it, so the server
    // deleted it.
    seededIds: ["p1", "p9"],
  });
  assertEquals(memberIds(next.members), ["p1"]);
  assertEquals(next.edits, {});
  assertEquals(next.editingId, null);
});

Deno.test("a member picked after the seed keeps its row and its draft", () => {
  const next = reseed({
    registry: [row("p1")],
    members: [row("p1"), row("p9", { model: "p9-draft" })],
    edits: { p9: { ...memberEditOf(row("p9")), model: "p9-draft" } },
    editingId: "p9",
    landed: [],
    // p9 was not in the seed: only the PATCH can land it, and the save
    // that would have sent that PATCH stopped short.
    seededIds: ["p1"],
  });
  assertEquals(memberIds(next.members), ["p1", "p9"]);
  assertEquals(next.edits.p9!.model, "p9-draft");
  assertEquals(next.editingId, "p9");
});

Deno.test("a re-seed hands back the seed it read from the registry", () => {
  const next = reseed({
    registry: [row("p2"), row("p1")],
    members: [row("p1"), row("p2")],
    edits: {},
    editingId: null,
    landed: [],
    seededIds: ["p1", "p2"],
  });
  assertEquals(next.seededIds, ["p2", "p1"]);
});

Deno.test("a second stop adopts an order that changed again", () => {
  // The first stop reads someone else's order and adopts it.
  const first = reseed({
    registry: [row("p2"), row("p1")],
    members: [row("p1"), row("p2")],
    edits: {},
    editingId: null,
    landed: [],
    seededIds: ["p1", "p2"],
  });
  assertEquals(memberIds(first.members), ["p2", "p1"]);
  // The seed it hands back is the registry's, so the next stop compares
  // against that one and hands a third order back whole too.
  const second = reseed({
    registry: [row("p1"), row("p2")],
    members: first.members,
    edits: first.edits,
    editingId: first.editingId,
    landed: [],
    seededIds: first.seededIds,
  });
  assertEquals(memberIds(second.members), ["p1", "p2"]);
});

Deno.test("a member the server deleted comes back as newly picked", () => {
  // p9 was in the seed and the registry no longer holds it: the first
  // re-seed drops it, and the seed it hands back no longer carries it.
  const dropped = reseed({
    registry: [row("p1")],
    members: [row("p1"), row("p9", { model: "p9-draft" })],
    edits: { p9: { ...memberEditOf(row("p9")), model: "p9-draft" } },
    editingId: "p9",
    landed: [],
    seededIds: ["p1", "p9"],
  });
  assertEquals(memberIds(dropped.members), ["p1"]);
  // The user picks it again; a later re-read must not drop it a second time.
  const again = reseed({
    registry: [row("p1")],
    members: [...dropped.members, row("p9", { model: "p9-new" })],
    edits: { p9: { ...memberEditOf(row("p9")), model: "p9-new" } },
    editingId: "p9",
    landed: [],
    seededIds: dropped.seededIds,
  });
  assertEquals(memberIds(again.members), ["p1", "p9"]);
  assertEquals(again.edits.p9!.model, "p9-new");
});

Deno.test("an empty seed treats every draft row as newly picked", () => {
  const next = reseed({
    registry: [row("p1")],
    members: [row("p1"), row("p9", { model: "p9-draft" })],
    edits: { p9: { ...memberEditOf(row("p9")), model: "p9-draft" } },
    editingId: "p9",
    landed: [],
    seededIds: [],
  });
  assertEquals(memberIds(next.members), ["p1", "p9"]);
  assertEquals(next.edits.p9!.model, "p9-draft");
});

Deno.test("orderChanged tells the user's sequence from the seed's", () => {
  assertEquals(orderChanged(["p1", "p2"], [row("p1"), row("p2")]), false);
  // A move, an add and a removal are all membership changes.
  assertEquals(orderChanged(["p1", "p2"], [row("p2"), row("p1")]), true);
  assertEquals(orderChanged(["p1"], [row("p1"), row("p9")]), true);
  assertEquals(orderChanged(["p1", "p9"], [row("p1")]), true);
});

Deno.test("firstOffending names the row whose draft the server refuses", () => {
  const good = { ...memberEditOf(row("p1")), model: "p1-next" };
  const badModel = { ...memberEditOf(row("p2")), model: " " };
  const badWindow = { ...memberEditOf(row("p3")), context_window: "0" };
  assertEquals(
    firstOffending([row("p1"), row("p2"), row("p3")], {
      p1: good,
      p2: badModel,
      p3: badWindow,
    })?.profile_id,
    "p2",
  );
  // Only drafted rows can offend: an untouched row sends nothing.
  assertEquals(firstOffending([row("p1")], { p1: good }), undefined);
  assertEquals(
    firstOffending([row("p3")], { p3: badWindow })?.profile_id,
    "p3",
  );
});
