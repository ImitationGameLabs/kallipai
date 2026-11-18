// The link's error field describes the current connection attempt, not the
// session's history: a drain failure sets it, a fresh transport attaching
// clears it, and the next failure sets it again. Without the second failing
// assertion a "clear once and never re-arm" implementation passes, and the
// layout banner stays silent through a second outage -- the same bad state as
// never clearing at all.
//
// The module under test is rune-bearing, so a passthrough `$state` shim lets
// `deno test` load it uncompiled (the seam hydrate_test uses).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { Link } = await import("./link.svelte.ts");

type Transport = import("../transport.ts").Transport;
type IncomingFrame = import("../transport.ts").IncomingFrame;
type SignalEvent = import("@kallipai/kallipai-lesche-client").SignalEvent;
type ConversationSender = import("../../transcript.ts").ConversationSender;

const SENDER: ConversationSender = { id: "me", kind: "user", handle: "me" };

/** Streams nothing, or throws before the first frame: the two ends a drain
 *  has to tell apart. The stream is still a generator, which is what the one
 *  lint exception below is about. */
function drained<T>(failure: unknown | null): AsyncGenerator<T> {
  // deno-lint-ignore require-yield
  return (async function* () {
    if (failure !== null) throw failure;
  })();
}

/** A transport whose three streams either throw `failure` or end cleanly. */
function transport(failure: unknown | null): Transport {
  return {
    localSender: SENDER,
    replies: () => drained<IncomingFrame>(failure),
    signals: () => drained<SignalEvent>(failure),
    status: () => drained<never>(failure),
    close() {},
  };
}

/** A link over a short retry ladder, with every lifecycle hook inert. */
function newLink(): InstanceType<typeof Link> {
  const link = new Link(
    () => true,
    () => true,
    () => {},
    () => {},
    () => {},
    () => {},
  );
  link.backoff = { min: 1, max: 1 };
  return link;
}

/** A drain failure logs to the console by design; keep the deliberate ones
 *  out of the test output while the link runs. */
async function quietRun(link: InstanceType<typeof Link>): Promise<void> {
  const log = console.error;
  console.error = () => {};
  try {
    await link.run();
  } finally {
    console.error = log;
  }
}

Deno.test("a drain failure sets the error and the error status", async () => {
  const boom = new Error("stream died");
  const link = newLink();
  link.transport = transport(boom);
  await quietRun(link);
  assertEquals(link.status, "error");
  assertEquals(link.error, boom);
});

Deno.test(
  "a rebuilt transport clears the error of the failed one",
  async () => {
    const link = newLink();
    link.restartStream = () => {
      link.restartStream = null;
      return transport(null);
    };
    link.transport = transport(new Error("stream died"));
    await quietRun(link);
    assertEquals(link.error, null);
    assertEquals(link.status === "error", false);
  },
);

Deno.test(
  "a failure after a successful rebuild sets the error again",
  async () => {
    const second = new Error("stream died twice");
    const link = newLink();
    let rebuilds = 0;
    link.restartStream = () => {
      rebuilds += 1;
      if (rebuilds === 1) return transport(null);
      link.restartStream = null;
      return transport(second);
    };
    link.transport = transport(new Error("stream died"));
    await quietRun(link);
    assertEquals(link.status, "error");
    assertEquals(link.error, second);
  },
);
