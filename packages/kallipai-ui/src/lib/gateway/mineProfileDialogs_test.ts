// The profile delete confirm names the collection it removes from: a profile
// is addressed by collection and id, so the confirm's row has to carry both
// and the remove call has to spend them. The test goes red if the call drops
// the collection and addresses the profile by id alone.
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
const { createMineProfileDialogs } =
  await import("./mineProfileDialogs.svelte.ts");
import type { MineProfileDialogStore } from "./mineProfileDialogs.svelte.ts";
import type { UserProfileRow } from "./client.ts";

/** One profile row in a named collection. */
function profileRow(collection: string, id: string): UserProfileRow {
  return {
    profile_id: id,
    collection_name: collection,
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

Deno.test(
  "profileRemove: the confirm carries the collection to the server",
  async () => {
    const calls: [string, string][] = [];
    const store = {
      removeProfile: (collection: string, id: string) => {
        calls.push([collection, id]);
        return Promise.resolve(true);
      },
      error: null,
    } as unknown as MineProfileDialogStore;
    const dialogs = createMineProfileDialogs({ store });
    dialogs.remove.ask(profileRow("c2", "p9"));
    assertEquals(dialogs.remove.target?.collection_name, "c2");
    await dialogs.remove.confirm();
    assertEquals(calls, [["c2", "p9"]]);
    assertEquals(dialogs.remove.open, false);
  },
);
