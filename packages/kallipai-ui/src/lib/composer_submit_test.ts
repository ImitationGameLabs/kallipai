// Behaviour tests for the composer's failure path: a send that rejects must
// put its text back where the reader typed it. The module is rune-bearing and
// `deno test` runs it uncompiled, so it loads behind a passthrough $state shim
// (the seam reveal_test uses) that answers the $state.raw form too.

import { assertEquals } from "@std/assert";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  namespace $state {
    function raw<T>(initial: T): T;
  }
}

(globalThis as Record<string, unknown>)["$state"] = Object.assign(
  (v: unknown) => v,
  { raw: (v: unknown) => v },
);

const { createComposer } = await import("./composer.svelte.ts");

function deferred() {
  let resolve!: () => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<void>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** Silence the controller's own console.error while a rejection is expected. */
async function quietly(run: () => Promise<void>): Promise<void> {
  const original = console.error;
  console.error = () => {};
  try {
    await run();
  } finally {
    console.error = original;
  }
}

Deno.test(
  "a failed send puts the text back under the sending binding",
  async () => {
    const gate = deferred();
    const composer = createComposer({
      send: () => gate.promise,
      canSubmit: () => true,
    });
    const restored: string[] = [];
    composer.binding = { key: "r:a", restore: (text) => restored.push(text) };
    composer.draft = "half typed";

    const submitting = composer.submit();
    assertEquals(composer.draft, "");

    // The reader switches conversation while the send is in flight: the binding
    // the composer holds is now the other conversation's.
    composer.binding = { key: "r:b", restore: (text) => restored.push(text) };
    composer.draft = "typed later";

    await quietly(async () => {
      gate.reject(new Error("offline"));
      await submitting;
    });

    assertEquals(restored, ["half typed"]);
    assertEquals(composer.draft, "typed later");
  },
);

Deno.test(
  "a failed send on the same conversation repaints the field",
  async () => {
    const composer = createComposer({
      send: () => Promise.reject(new Error("offline")),
      canSubmit: () => true,
    });
    composer.binding = {
      key: "r:a",
      restore: (text) => {
        composer.draft = text;
      },
    };
    composer.draft = "half typed";

    await quietly(() => composer.submit());

    assertEquals(composer.draft, "half typed");
    assertEquals(composer.sending, false);
  },
);

Deno.test("a submit without a binding is refused, not sent", async () => {
  let sent = 0;
  const composer = createComposer({
    send: () => {
      sent += 1;
    },
    canSubmit: () => true,
  });
  composer.draft = "half typed";

  await quietly(() => composer.submit());

  assertEquals(sent, 0);
  assertEquals(composer.draft, "half typed");
  assertEquals(composer.sending, false);
});

Deno.test("an in-flight send holds only its own conversation", async () => {
  const gate = deferred();
  const sent: string[] = [];
  const composer = createComposer({
    send: (text) => {
      sent.push(text);
      return gate.promise;
    },
    canSubmit: () => true,
  });
  composer.binding = { key: "r:a", restore: () => {} };
  composer.draft = "first";
  const first = composer.submit();
  assertEquals(composer.sending, true);

  // The reader moves on while that POST is in flight: the other
  // conversation's button is free, so its submit reaches the transport.
  composer.binding = { key: "r:b", restore: () => {} };
  assertEquals(composer.sending, false);
  composer.draft = "second";
  const second = composer.submit();
  assertEquals(sent, ["first", "second"]);
  assertEquals(composer.sending, true);

  gate.resolve();
  await first;
  await second;
  assertEquals(composer.sending, false);
});
