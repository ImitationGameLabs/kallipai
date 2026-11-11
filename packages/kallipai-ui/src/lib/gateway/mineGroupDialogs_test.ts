// The group dialog's one open path: a create opened while an edit's read is
// still in flight must not be seeded by that read, and a superseded answer
// must not swap the target under an open dialog. Each test goes red if the
// token bump or the compare is dropped.
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
const { createMineGroupDialogs } = await import("./mineGroupDialogs.svelte.ts");
import type { MineGroupDialogStore } from "./mineGroupDialogs.svelte.ts";
import type { UserGroupRow } from "./client.ts";

/** One group row; only the id and name matter to these reads. */
function groupRow(id: string, name: string): UserGroupRow {
  return { group_id: id, name, members: [] };
}

/** A load whose answer the test lands by hand, per group id. */
function deferred<T>() {
  let land!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    land = resolve;
  });
  return { promise, land };
}

/** A group controller whose loadGroup answers when the test says so. */
function dialogsWith() {
  const loads = new Map<
    string,
    ReturnType<typeof deferred<UserGroupRow | undefined>>
  >();
  const store = {
    loadGroup: (id: string) => {
      const load = deferred<UserGroupRow | undefined>();
      loads.set(id, load);
      return load.promise;
    },
    error: null,
  } as unknown as MineGroupDialogStore;
  return { dialogs: createMineGroupDialogs({ store }), loads };
}

Deno.test(
  "groupEdit: the create form is not seeded by an edit in flight",
  async () => {
    const { dialogs, loads } = dialogsWith();
    const one = groupRow("g1", "one");

    const edit = dialogs.edit.show(one);
    await dialogs.edit.show(null);
    loads.get("g1")!.land(one);
    await edit;
    assertEquals(
      dialogs.edit.row,
      null,
      "the late group stays out of the form",
    );
    assertEquals(dialogs.edit.open, true);
  },
);

Deno.test(
  "groupEdit: a superseded answer does not open the dialog",
  async () => {
    const { dialogs, loads } = dialogsWith();
    const one = groupRow("g1", "one");
    const two = groupRow("g2", "two");

    const first = dialogs.edit.show(one);
    const second = dialogs.edit.show(two);
    loads.get("g1")!.land(one);
    await first;
    assertEquals(dialogs.edit.open, false, "the dialog waits for the newest");
    assertEquals(dialogs.edit.row, null);
    loads.get("g2")!.land(two);
    await second;
    assertEquals(dialogs.edit.row?.group_id, "g2");
  },
);
