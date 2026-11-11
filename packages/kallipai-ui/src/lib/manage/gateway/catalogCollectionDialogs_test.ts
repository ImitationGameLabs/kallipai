// The collection row's action menu and the group dialog's live row: two
// pieces of bookkeeping that belong beside the dialogs. The menu hands its
// string over whole, and the group row is resolved by id on every read so a
// member op's re-read cannot leave the dialog on a stale row.
//
// The module is rune-bearing but deno test runs it uncompiled, so a
// passthrough $state shim lets it run with plain fields (store_test's
// pattern).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { createCatalogCollectionDialogs } =
  await import("./catalogCollectionDialogs.svelte.ts");
import type { CatalogCollectionDialogStore } from "./catalogCollectionDialogs.svelte.ts";
import type { AdminCollectionRow, GroupRow } from "./client.ts";

/** One collection row; the counts feed the delete confirm's copy. */
function collectionRow(name: string): AdminCollectionRow {
  return {
    name,
    description: "",
    sets: [],
    default_set: null,
    publications: [],
    profile_count: 0,
    set_count: 0,
  };
}

/** A controller whose store records the calls its menu dispatch makes. */
function dialogsWith(groups: () => GroupRow[]) {
  const calls: string[] = [];
  const store = {
    get groups() {
      return groups();
    },
    publishToGroup: (name: string, groupId: string) => {
      calls.push(`publish ${name} ${groupId}`);
      return Promise.resolve(undefined);
    },
    unpublishFromGroup: (name: string, groupId: string) => {
      calls.push(`unpublish ${name} ${groupId}`);
      return Promise.resolve(undefined);
    },
    error: null,
  } as unknown as CatalogCollectionDialogStore;
  const dialogs = createCatalogCollectionDialogs({
    store,
    afterRemove: () => {},
  });
  return { dialogs, calls };
}

Deno.test(
  "collectionAction: the menu string reaches the store and the forms",
  () => {
    const { dialogs, calls } = dialogsWith(() => []);
    const row = collectionRow("c1");

    dialogs.collection.applyAction(row, "publish:g1");
    dialogs.collection.applyAction(row, "unpublish:g2");
    assertEquals(calls, ["publish c1 g1", "unpublish c1 g2"]);

    dialogs.collection.applyAction(row, "edit");
    assertEquals(dialogs.collection.open, true, "edit opens the form");
    assertEquals(dialogs.collection.row?.name, "c1");
    dialogs.collection.cancel();
    assertEquals(dialogs.collection.open, false);

    dialogs.collection.applyAction(row, "remove");
    assertEquals(
      dialogs.collection.remove.open,
      true,
      "remove asks the confirm",
    );
    assertEquals(dialogs.collection.remove.target?.name, "c1");
  },
);

Deno.test("collectionAction: an unknown string is not an action", () => {
  const { dialogs, calls } = dialogsWith(() => []);
  const row = collectionRow("c1");

  dialogs.collection.applyAction(row, "publishx:g1");
  assertEquals(calls, [], "no prefix match, no call");
  assertEquals(dialogs.collection.open, false);
  assertEquals(dialogs.collection.remove.open, false);
});

Deno.test(
  "groupEdit: the row is resolved by id, not held from the click",
  () => {
    const groups: GroupRow[] = [{ group_id: "g1", name: "one", members: [] }];
    const { dialogs } = dialogsWith(() => groups);

    dialogs.group.show(groups[0]);
    assertEquals(dialogs.group.row?.name, "one");
    // A member op re-reads the list and the store swaps the array: the row the
    // click captured is gone, the id still resolves.
    groups[0] = { group_id: "g1", name: "renamed", members: ["a"] };
    assertEquals(dialogs.group.row?.name, "renamed", "the live row is re-read");
    assertEquals(dialogs.group.row?.members, ["a"]);
  },
);
