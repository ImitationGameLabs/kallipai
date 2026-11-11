// The placement drop's reload condition: the panel re-reads only when the
// membership write left the store clean. The reload's own read nulls the
// store error first, so an unconditional reload would wipe the failure banner
// before it paints. This test goes red if the condition is dropped.
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
const { createCatalogProfileDialogs } =
  await import("./catalogProfileDialogs.svelte.ts");
import type { CatalogProfileDialogStore } from "./catalogProfileDialogs.svelte.ts";
import type { AdminProfileRow } from "./client.ts";

/** One unassigned row; only the set matters to a drop. */
function row(id: string): AdminProfileRow {
  return {
    profile_id: id,
    collection_name: "c1",
    provider_id: `up-${id}`,
    owner: "admin",
    family: "deepseek",
    model: `${id}-model`,
    max_context_window: 128_000,
    effort: null,
    modalities: ["text"],
    set: null,
    position: 0,
    max_budget: null,
    tpm_limit: null,
    rpm_limit: null,
  };
}

Deno.test(
  "placement: a dropped row reloads the panel only on a clean verdict",
  async () => {
    let fail = false;
    let reloads = 0;
    const store: {
      addUnassignedToSet: () => Promise<void>;
      error: string | null;
    } = {
      addUnassignedToSet: () => {
        if (fail) store.error = "a set cannot be its own member";
        return Promise.resolve();
      },
      error: null,
    };
    const dialogs = createCatalogProfileDialogs({
      store: store as unknown as CatalogProfileDialogStore,
      reloadUnassigned: () => {
        reloads += 1;
        return Promise.resolve();
      },
    });

    dialogs.placement.startDrag(row("one"));
    await dialogs.placement.dropToSet("s1");
    assertEquals(reloads, 1);

    fail = true;
    dialogs.placement.startDrag(row("two"));
    await dialogs.placement.dropToSet("s1");
    assertEquals(reloads, 1, "a failed drop leaves the panel alone");
    assertEquals(dialogs.placement.dragRow, null, "the drag clears either way");
  },
);
