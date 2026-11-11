// The named-set request token, on the user face: the dialog must seed from
// the click the user made last, never from one they moved on from, and the
// token has a second checkpoint because the first answer can trigger the
// rows read the projection needs. Each test below goes red if a compare is
// dropped.
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
const { createMineSetDialogs } = await import("./mineSetDialogs.svelte.ts");
import type { MineSetDialogStore } from "./mineSetDialogs.svelte.ts";
import type { UserProfileRow, UserSetRow } from "./client.ts";

/** One set row; the member resolves against the fixture's profile rows. */
function setRow(name: string): UserSetRow {
  return {
    collection_name: "c1",
    name,
    description: "",
    members: [`${name}-member`],
  };
}

/** One profile row in the collection every fixture set lives in. */
function profileRow(id: string): UserProfileRow {
  return {
    profile_id: id,
    collection_name: "c1",
    provider_id: "p1",
    family: "openai",
    model: "m",
    max_context_window: null,
    effort: null,
    modalities: null,
    max_budget: null,
    tpm_limit: null,
    rpm_limit: null,
    set: null,
    position: 0,
  };
}

/** A promise whose answer the test lands by hand. */
function deferred<T>() {
  let land!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    land = resolve;
  });
  return { promise, land };
}

/** A set-dialog controller whose loadSet answers when the test says so. */
function dialogsWith(
  rows: () => readonly UserProfileRow[] | null,
  refresh: () => Promise<void> = () => Promise.resolve(),
) {
  const loads = new Map<
    string,
    ReturnType<typeof deferred<UserSetRow | undefined>>
  >();
  const store = {
    loadSet: (_collection: string, name: string) => {
      const load = deferred<UserSetRow | undefined>();
      loads.set(name, load);
      return load.promise;
    },
    refreshAll: refresh,
    // The rows port is the store's field now; a getter keeps the fixture's
    // rows live across a test that re-reads them.
    get profiles() {
      return rows();
    },
    error: null,
  } as unknown as MineSetDialogStore;
  const dialogs = createMineSetDialogs({ store });
  return { dialogs, loads };
}

Deno.test("setEdit: the last click seeds the dialog", async () => {
  const rows = [profileRow("a-member"), profileRow("b-member")];
  const { dialogs, loads } = dialogsWith(() => rows);
  const a = setRow("a");
  const b = setRow("b");

  const first = dialogs.edit(a);
  const second = dialogs.edit(b);
  loads.get("b")!.land(b);
  await second;
  loads.get("a")!.land(a);
  await first;
  assertEquals(dialogs.set?.name, "b", "the superseded answer is dropped");
});

Deno.test("setEdit: a superseded answer does not open the dialog", async () => {
  const rows = [profileRow("a-member"), profileRow("b-member")];
  const { dialogs, loads } = dialogsWith(() => rows);
  const a = setRow("a");
  const b = setRow("b");

  const first = dialogs.edit(a);
  const second = dialogs.edit(b);
  loads.get("a")!.land(a);
  await first;
  assertEquals(dialogs.open, false, "the dialog waits for the newest");
  assertEquals(dialogs.set, null);
  loads.get("b")!.land(b);
  await second;
  assertEquals(dialogs.open, true);
  assertEquals(dialogs.set?.name, "b");
});

Deno.test(
  "setEdit: an answer that comes back after the rows read is dropped",
  async () => {
    // The first click's answer finds the rows unread and awaits the read;
    // the second click answers inside that gap. The first must not open the
    // dialog once the read lands, and it must not open it before either.
    let rows: readonly UserProfileRow[] | null = null;
    const read = deferred<void>();
    const { dialogs, loads } = dialogsWith(
      () => rows,
      () => {
        rows = [profileRow("a-member"), profileRow("b-member")];
        return read.promise;
      },
    );
    const a = setRow("a");
    const b = setRow("b");

    const first = dialogs.edit(a);
    loads.get("a")!.land(a);
    await Promise.resolve();
    const second = dialogs.edit(b);
    read.land(undefined);
    await first;
    assertEquals(dialogs.open, false, "the read's answer is not the seed");
    loads.get("b")!.land(b);
    await second;
    assertEquals(dialogs.open, true);
    assertEquals(dialogs.set?.name, "b");
  },
);

Deno.test(
  "setCreate: the set lands in the collection it was opened from",
  async () => {
    const rows = [profileRow("a-member")];
    let created: { collection: string; name: string } | null = null;
    const store = {
      addSet: (collection: string, body: { name: string }) => {
        created = { collection, name: body.name };
        return Promise.resolve({
          collection_name: collection,
          name: body.name,
          description: "",
          members: [],
        });
      },
      refreshAll: () => Promise.resolve(),
      updateMember: () => Promise.resolve(true),
      patchDescription: () => Promise.resolve(true),
      reorderSet: () => Promise.resolve(true),
      loadSet: () =>
        Promise.resolve({
          collection_name: "c1",
          name: "new",
          description: "",
          members: ["a-member"],
        }),
      profiles: rows,
      error: null,
    } as unknown as MineSetDialogStore;
    const dialogs = createMineSetDialogs({ store });
    dialogs.create("c1");
    await dialogs.save({
      name: "new",
      description: "",
      members: ["a-member"],
      updates: [],
      joins: [],
      leaves: [],
    });
    assertEquals(created, { collection: "c1", name: "new" });
  },
);

Deno.test(
  "setCreate: the create form is not seeded by an edit in flight",
  async () => {
    const rows = [profileRow("a-member")];
    const { dialogs, loads } = dialogsWith(() => rows);
    const a = setRow("a");

    const edit = dialogs.edit(a);
    dialogs.create("c1");
    loads.get("a")!.land(a);
    await edit;
    assertEquals(
      dialogs.set,
      null,
      "the late set stays out of the create form",
    );
    assertEquals(dialogs.collection, "c1", "the form keeps its collection");
    assertEquals(dialogs.open, true);
  },
);
