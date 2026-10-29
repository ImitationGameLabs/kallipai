import { assertEquals } from "@std/assert";
import {
  changedUpdates,
  memberEditOf,
  type MemberEdit,
} from "../manage/gateway/setDraft.ts";
import type { UserProfileRow, UserSetRow } from "./client.ts";
import {
  diffBase,
  draftRowOf,
  poolOf,
  setDetailFor,
  userMemberBody,
} from "./userSetDraft.ts";

// The user face's projection of its own rows: the shared draft arithmetic
// receives these rows, so what is pinned here is the projection itself (a
// row with no modality list, a set's member order) and the one place the
// two faces differ on the wire: the member PUT body.

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
    parked: true,
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
  const edit = memberEditOf(draftRowOf(row("p1", { modalities: null })));
  assertEquals(edit.modalities, []);
  const updates = changedUpdates(
    [draftRowOf(row("p1", { modalities: null }))],
    [draftRowOf(row("p1", { modalities: null }))],
    { p1: { ...edit, model: "next" } },
  );
  assertEquals(updates[0]!.body.modalities, null);
});

Deno.test(
  "the member PUT body carries the shared fields and nothing else",
  () => {
    const original = [draftRowOf(row("p1"))];
    const edits: Record<string, MemberEdit> = {
      p1: { ...memberEditOf(original[0]!), model: "next" },
    };
    const updates = changedUpdates(original, original, edits);
    const body = userMemberBody(updates[0]!.body);
    assertEquals(body, {
      model: "next",
      max_context_window: 128_000,
      effort: "high",
      modalities: ["text", "image"],
    });
    // The catalog's promote flag is not this face's to send: the user route's
    // profile body has no such field.
    assertEquals(Object.keys(body).sort(), [
      "effort",
      "max_context_window",
      "modalities",
      "model",
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
  assertEquals(detail!.profiles[0]!.parked, true);
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

Deno.test("diffBase adds the pool rows the draft picked up", () => {
  const original = [draftRowOf(row("p1"))];
  const added = draftRowOf(row("p9"));
  const base = diffBase(original, [original[0]!, added]);
  assertEquals(
    base.map((profile) => profile.profile_id),
    ["p1", "p9"],
  );
  // A create has no registry row to start from: the draft is its own base,
  // so an edit made before the first save still diffs.
  assertEquals(diffBase(null, [added]), [added]);
});

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

Deno.test("a create draft carries no row to put", () => {
  // A create diffs against itself: the dialog offers no form before the set
  // exists, so the edit map stays empty and no PUT body can come out.
  const members = [draftRowOf(row("p1")), draftRowOf(row("p2"))];
  assertEquals(changedUpdates(diffBase(members, members), members, {}), []);
});
