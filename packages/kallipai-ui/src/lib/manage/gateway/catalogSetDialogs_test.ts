// The set-edit request token: the dialog must seed from the click the user
// made last, never from one they moved on from. Each test below goes red if
// the compare after the load is dropped.
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
const { createCatalogSetDialogs } =
  await import("./catalogSetDialogs.svelte.ts");
import type { CatalogSetDialogStore } from "./catalogSetDialogs.svelte.ts";
import type { AdminSetRow } from "./client.ts";

/** One set row; only the name matters to these reads. */
function setRow(name: string): AdminSetRow {
  return { collection_name: "c1", name, description: "", profiles: [] };
}

/** A load whose answer the test lands by hand, per set name. */
function deferred<T>() {
  let land!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    land = resolve;
  });
  return { promise, land };
}

/** A set-dialog controller whose loadSet answers when the test says so. */
function dialogsWith() {
  const loads = new Map<
    string,
    ReturnType<typeof deferred<AdminSetRow | undefined>>
  >();
  const store = {
    loadSet: (_collection: string, name: string) => {
      const load = deferred<AdminSetRow | undefined>();
      loads.set(name, load);
      return load.promise;
    },
    setsIn: () => [],
    error: null,
  } as unknown as CatalogSetDialogStore;
  const dialogs = createCatalogSetDialogs({
    store,
    collection: () => "c1",
    reloadUnassigned: () => Promise.resolve(),
  });
  return { dialogs, loads };
}

Deno.test("setEdit: the last click seeds the dialog", async () => {
  const { dialogs, loads } = dialogsWith();
  const a = setRow("a");
  const b = setRow("b");

  const first = dialogs.edit.edit(a);
  const second = dialogs.edit.edit(b);
  loads.get("b")!.land(b);
  await second;
  loads.get("a")!.land(a);
  await first;
  assertEquals(dialogs.edit.row, b, "the superseded answer is dropped");
});

Deno.test("setEdit: a superseded answer does not open the dialog", async () => {
  const { dialogs, loads } = dialogsWith();
  const a = setRow("a");
  const b = setRow("b");

  const first = dialogs.edit.edit(a);
  const second = dialogs.edit.edit(b);
  loads.get("a")!.land(a);
  await first;
  assertEquals(dialogs.edit.open, false, "the dialog waits for the newest");
  assertEquals(dialogs.edit.row, null);
  loads.get("b")!.land(b);
  await second;
  assertEquals(dialogs.edit.open, true);
  assertEquals(dialogs.edit.row, b);
});

Deno.test(
  "setEdit: the open dialog keeps its target while the next load runs",
  async () => {
    const { dialogs, loads } = dialogsWith();
    const a = setRow("a");
    const b = setRow("b");

    const first = dialogs.edit.edit(a);
    loads.get("a")!.land(a);
    await first;
    const second = dialogs.edit.edit(b);
    assertEquals(
      dialogs.edit.open,
      true,
      "the dialog stays open while loading",
    );
    assertEquals(dialogs.edit.row, a, "the target is not blanked mid-load");
    loads.get("b")!.land(b);
    await second;
    assertEquals(dialogs.edit.row, b);
  },
);

Deno.test(
  "setCreate: the create dialog is not seeded by an edit in flight",
  async () => {
    const { dialogs, loads } = dialogsWith();
    const a = setRow("a");

    const edit = dialogs.edit.edit(a);
    dialogs.create.show();
    loads.get("a")!.land(a);
    await edit;
    assertEquals(dialogs.create.open, true, "the create dialog stays open");
    assertEquals(dialogs.edit.row, null, "the late row stays out of the form");
    assertEquals(dialogs.edit.open, false);
  },
);
