// The collection-edit request token, on the user face: a slow load that
// answers after a later click must not repaint the dialog the user opened.
// Each test goes red if the compare after the load is dropped.
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
const { createMineCollectionDialogs } =
  await import("./mineCollectionDialogs.svelte.ts");
import type { MineCollectionDialogStore } from "./mineCollectionDialogs.svelte.ts";
import type { UserCollectionRow } from "./client.ts";

/** One collection row; only the name matters to these reads. */
function collectionRow(name: string): UserCollectionRow {
  return {
    name,
    description: "",
    sets: [],
    profile_count: 0,
    set_count: 0,
  };
}

/** A load whose answer the test lands by hand, per collection name. */
function deferred<T>() {
  let land!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    land = resolve;
  });
  return { promise, land };
}

function dialogsWith() {
  const loads = new Map<
    string,
    ReturnType<typeof deferred<UserCollectionRow | undefined>>
  >();
  const store = {
    loadCollection: (name: string) => {
      const load = deferred<UserCollectionRow | undefined>();
      loads.set(name, load);
      return load.promise;
    },
    error: null,
  } as unknown as MineCollectionDialogStore;
  return { dialogs: createMineCollectionDialogs({ store }), loads };
}

Deno.test("collectionEdit: the last click seeds the dialog", async () => {
  const { dialogs, loads } = dialogsWith();
  const a = collectionRow("a");
  const b = collectionRow("b");

  const first = dialogs.edit.show(a);
  const second = dialogs.edit.show(b);
  loads.get("b")!.land(b);
  await second;
  loads.get("a")!.land(a);
  await first;
  assertEquals(dialogs.edit.row?.name, "b", "the superseded answer is dropped");
});

Deno.test(
  "collectionEdit: a superseded answer does not open the dialog",
  async () => {
    const { dialogs, loads } = dialogsWith();
    const a = collectionRow("a");
    const b = collectionRow("b");

    const first = dialogs.edit.show(a);
    const second = dialogs.edit.show(b);
    loads.get("a")!.land(a);
    await first;
    assertEquals(dialogs.edit.open, false, "the dialog waits for the newest");
    assertEquals(dialogs.edit.row, null);
    loads.get("b")!.land(b);
    await second;
    assertEquals(dialogs.edit.open, true);
    assertEquals(dialogs.edit.row?.name, "b");
  },
);

Deno.test(
  "collectionCreate: an edit in flight does not seed the create form",
  async () => {
    const { dialogs, loads } = dialogsWith();
    const a = collectionRow("a");

    const edit = dialogs.edit.show(a);
    await dialogs.edit.show(null);
    loads.get("a")!.land(a);
    await edit;
    assertEquals(dialogs.edit.row, null, "the late row stays out of the form");
    assertEquals(dialogs.edit.open, true);
  },
);
