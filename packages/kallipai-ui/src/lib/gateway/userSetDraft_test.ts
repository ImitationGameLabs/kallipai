import { assertEquals } from "@std/assert";
import {
  memberEditOf,
  membershipWrites,
  type MemberEdit,
} from "../manage/gateway/setDraft.ts";
import type { UserProfileRow, UserSetRow } from "./client.ts";
import {
  draftRowOf,
  pickedRows,
  poolOf,
  setDetailFor,
} from "./userSetDraft.ts";

// The user face's projection of its own rows: the shared draft arithmetic
// receives these rows, so what is pinned here is the projection itself (a
// row with no modality list, a set's member order) and the writes the
// projection makes possible.

function row(
  id: string,
  overrides: Partial<UserProfileRow> = {},
): UserProfileRow {
  return {
    profile_id: id,
    collection_name: "mine",
    provider_id: "prov",
    family: "deepseek",
    model: `${id}-model`,
    max_context_window: 128_000,
    effort: "high",
    modalities: ["text", "image"],
    max_budget: null,
    tpm_limit: null,
    rpm_limit: null,
    set: "primary",
    position: 0,
    ...overrides,
  };
}

function set(members: string[], description = "rotation"): UserSetRow {
  return {
    collection_name: "mine",
    name: "primary",
    description,
    members,
  };
}

Deno.test("a row with no modality list reads as text alone", () => {
  // The server spells text only as a null list; the tag group cannot show
  // null, so the form reads it as an empty selection.
  const members = [draftRowOf(row("p1", { modalities: null }))];
  const edit = { ...memberEditOf(members[0]!), model: "next" };
  assertEquals(edit.modalities, []);
  const writes = membershipWrites(members, members, { p1: edit }, "primary");
  // An empty tag selection is the server default and rides as absent.
  assertEquals(writes.updates[0]!.body.modalities, null);
});

Deno.test(
  "the member PUT body carries the shared fields and the set that holds it",
  () => {
    const original = [draftRowOf(row("p1"))];
    const edits: Record<string, MemberEdit> = {
      p1: { ...memberEditOf(original[0]!), model: "next" },
    };
    const writes = membershipWrites(original, original, edits, "primary");
    const body = writes.updates[0]!.body;
    assertEquals(body, {
      model: "next",
      max_context_window: 128_000,
      effort: "high",
      modalities: ["text", "image"],
      set: "primary",
    });
    // Nothing beyond these fields: the row carries no budget, and the set
    // is the only membership fact the PUT states.
    assertEquals(Object.keys(body).sort(), [
      "effort",
      "max_context_window",
      "modalities",
      "model",
      "set",
    ]);
  },
);

Deno.test("setDetailFor keeps the member order the wire names", () => {
  const detail = setDetailFor(set(["p2", "p1"]), [row("p1"), row("p2")]);
  assertEquals(detail!.collection_name, "mine");
  assertEquals(detail!.name, "primary");
  assertEquals(detail!.description, "rotation");
  assertEquals(
    detail!.profiles.map((profile) => profile.profile_id),
    ["p2", "p1"],
  );
  // Both rows keep the facts the cards read.
  assertEquals(detail!.profiles[0]!.family, "deepseek");
  assertEquals(detail!.profiles[0]!.set, "primary");
});

Deno.test("setDetailFor drops a member id its rows no longer answer", () => {
  const detail = setDetailFor(set(["p1", "gone"]), [row("p1")]);
  assertEquals(
    detail!.profiles.map((profile) => profile.profile_id),
    ["p1"],
  );
});

Deno.test("setDetailFor answers null while the rows are unread", () => {
  // Rows never read are not rows known to be absent: a detail built from an
  // unknown row set would drop every member, and the save that followed
  // would clear the set. The type keeps the two states apart.
  assertEquals(setDetailFor(set(["p1", "p2"]), null), null);
});

Deno.test("setDetailFor reads no rows as an empty membership", () => {
  // A set holding no members is a legal set: its detail keeps the set's own
  // facts and carries no rows.
  assertEquals(setDetailFor(set([], "solo"), []), {
    collection_name: "mine",
    name: "primary",
    description: "solo",
    profiles: [],
  });
});

Deno.test("a created set writes each picked row as a join on it", () => {
  // The dialog is creating the set: the place it resolves picks into holds
  // no registry rows, so every picked row is a join and nothing else.
  const rows = [draftRowOf(row("p1")), draftRowOf(row("p9"))];
  const writes = membershipWrites([], rows, {}, "primary");
  assertEquals(
    writes.joins.map((write) => write.profileId),
    ["p1", "p9"],
  );
  assertEquals(
    writes.joins.map((write) => write.body.set),
    ["primary", "primary"],
  );
  assertEquals(writes.updates, []);
  assertEquals(writes.leaves, []);
});

Deno.test(
  "pickedRows keeps the picks' order and leaves out a vanished row",
  () => {
    const source = [row("p1"), row("p2"), row("p3")];
    // The picks are the failover order the dialog showed, not the collection's.
    assertEquals(
      pickedRows(["p3", "p1"], source).map((picked) => picked.profile_id),
      ["p3", "p1"],
    );
    assertEquals(pickedRows(["p2"], source)[0]!.set, "primary");
    // A pick the rows no longer answer is left out: its PUT would 404 and sink
    // the save the create already started.
    assertEquals(
      pickedRows(["p1", "gone"], source).map((picked) => picked.profile_id),
      ["p1"],
    );
    assertEquals(pickedRows([], source), []);
  },
);

Deno.test("the picker's pool leaves out the rows the set already holds", () => {
  const profiles = [row("p1"), row("p2"), row("p3")];
  const members = [draftRowOf(row("p2"))];
  // A set holds a profile once, and the pool keeps the collection's
  // order: the picker reads like the list it came from.
  assertEquals(
    poolOf(profiles, members).map((r) => r.profile_id),
    ["p1", "p3"],
  );
  assertEquals(
    poolOf(profiles, profiles.map(draftRowOf)).map((r) => r.profile_id),
    [],
  );
  assertEquals(poolOf([], members), []);
  assertEquals(poolOf(profiles, []).length, 3);
});
