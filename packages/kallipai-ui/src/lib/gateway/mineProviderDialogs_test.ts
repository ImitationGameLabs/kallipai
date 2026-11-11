// The provider confirm keeps its place when the server refuses: a referenced
// provider answers 409, and the confirm stays open carrying that reason, the
// way a refused save keeps the form open. The test goes red if the refusal
// path closes the confirm or drops the store's reason.
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
const { createMineProviderDialogs } =
  await import("./mineProviderDialogs.svelte.ts");
import type { MineProviderDialogStore } from "./mineProviderDialogs.svelte.ts";

const REFUSED = "provider still carries a credential";

/** A provider controller whose remove answers the way the server did. */
function dialogsWith(removal: { ok: boolean }) {
  let reason: string | null = null;
  const store = {
    removeProvider: (_id: string) => {
      reason = removal.ok ? null : REFUSED;
      return Promise.resolve(removal.ok);
    },
    get error() {
      return reason;
    },
  } as unknown as MineProviderDialogStore;
  return createMineProviderDialogs({ store });
}

Deno.test(
  "providerRemove: a refused delete keeps the confirm and its reason",
  async () => {
    const dialogs = dialogsWith({ ok: false });
    dialogs.provider.remove.ask("p1");
    await dialogs.provider.remove.confirm();
    assertEquals(dialogs.provider.remove.open, true, "the confirm stays open");
    assertEquals(dialogs.provider.remove.error, REFUSED);
    assertEquals(dialogs.provider.remove.busy, false);
  },
);

Deno.test("providerRemove: a landed delete clears the target", async () => {
  const dialogs = dialogsWith({ ok: true });
  dialogs.provider.remove.ask("p1");
  await dialogs.provider.remove.confirm();
  assertEquals(dialogs.provider.remove.open, false);
  assertEquals(dialogs.provider.remove.target, null);
  assertEquals(dialogs.provider.remove.error, null);
});
