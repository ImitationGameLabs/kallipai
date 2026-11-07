// The build-version watch: which ids it believes, which it ignores, and that
// one answer ends the asking.

import { assertEquals } from "@std/assert";
import {
  startVersionWatch,
  usableVersionId,
  type VersionDocument,
} from "./buildVersion.ts";

/** Let the watch's fetch and judgment settle. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 5));

/** A document stand-in: listeners land here and the test says when the page
 *  is visible again. */
function fakeDocument(visibility: "visible" | "hidden" = "visible") {
  const listeners = new Set<() => void>();
  const doc: VersionDocument = {
    get visibilityState() {
      return visibility;
    },
    addEventListener: (_: string, fn: () => void) => listeners.add(fn),
    removeEventListener: (_: string, fn: () => void) => listeners.delete(fn),
  };
  return {
    doc,
    /** Flip visibility and fire the event, as the browser would. */
    become(next: "visible" | "hidden") {
      visibility = next;
      for (const fn of listeners) fn();
    },
    listenerCount: () => listeners.size,
  };
}

Deno.test("buildVersion: a different id reports once", async () => {
  const { doc } = fakeDocument();
  let announced = 0;
  let asks = 0;
  const stop = startVersionWatch({
    current: "1700000000000",
    fetchVersion: () => {
      asks += 1;
      return Promise.resolve("1800000000000");
    },
    onAvailable: () => (announced += 1),
    intervalMs: 2,
    doc,
  });
  await settle();
  await settle();
  stop();
  assertEquals(announced, 1, "one notice");
  assertEquals(asks, 1, "the answer ended the asking");
});

Deno.test("buildVersion: the running id is not news", async () => {
  const { doc } = fakeDocument();
  let announced = 0;
  const stop = startVersionWatch({
    current: "1700000000000",
    fetchVersion: () => Promise.resolve("1700000000000"),
    onAvailable: () => (announced += 1),
    intervalMs: 2,
    doc,
  });
  await settle();
  await settle();
  stop();
  assertEquals(announced, 0);
});

Deno.test(
  "buildVersion: an answer the watch cannot read is no answer",
  async () => {
    for (const value of ["", "   ", "x".repeat(65), null, undefined, 42]) {
      const { doc } = fakeDocument();
      let announced = 0;
      const stop = startVersionWatch({
        current: "1700000000000",
        fetchVersion: () => Promise.resolve(value),
        onAvailable: () => (announced += 1),
        intervalMs: 60_000,
        doc,
      });
      await settle();
      stop();
      assertEquals(announced, 0, `no notice for ${JSON.stringify(value)}`);
    }
  },
);

Deno.test(
  "buildVersion: a rollback below the running id still reports",
  async () => {
    // The served id is older than the running one: a page left on a superseded
    // bundle is exactly the page that should be told, so the judgment is
    // "different", not "newer".
    const { doc } = fakeDocument();
    let announced = 0;
    const stop = startVersionWatch({
      current: "2026-11-07-b",
      fetchVersion: () => Promise.resolve("2026-11-06-a"),
      onAvailable: () => (announced += 1),
      intervalMs: 60_000,
      doc,
    });
    await settle();
    stop();
    assertEquals(announced, 1);
  },
);

Deno.test(
  "buildVersion: a failed ask is retried on the next interval",
  async () => {
    const { doc } = fakeDocument();
    let announced = 0;
    let asks = 0;
    const stop = startVersionWatch({
      current: "1700000000000",
      fetchVersion: () => {
        asks += 1;
        return asks === 1
          ? Promise.reject(new Error("offline"))
          : Promise.resolve("1800000000000");
      },
      onAvailable: () => (announced += 1),
      intervalMs: 2,
      doc,
    });
    await settle();
    await settle();
    stop();
    assertEquals(announced, 1, "the second ask reports");
  },
);

Deno.test(
  "buildVersion: stop before the answer silences the notice",
  async () => {
    const { doc, listenerCount } = fakeDocument();
    let announced = 0;
    let resolve: (value: unknown) => void = () => {};
    const pending = new Promise((r) => (resolve = r));
    const stop = startVersionWatch({
      current: "1700000000000",
      fetchVersion: () => pending,
      onAvailable: () => (announced += 1),
      intervalMs: 60_000,
      doc,
    });
    stop();
    resolve("1800000000000");
    await settle();
    assertEquals(announced, 0);
    assertEquals(listenerCount(), 0, "the visibility hook is gone");
  },
);

Deno.test("buildVersion: returning to the page asks again", async () => {
  const { doc, become } = fakeDocument("hidden");
  let announced = 0;
  let asks = 0;
  const stop = startVersionWatch({
    current: "1700000000000",
    fetchVersion: () => {
      asks += 1;
      return Promise.resolve(asks === 1 ? null : "1800000000000");
    },
    onAvailable: () => (announced += 1),
    intervalMs: 60_000,
    doc,
  });
  await settle();
  assertEquals(announced, 0, "the first ask found nothing");
  become("visible");
  await settle();
  stop();
  assertEquals(announced, 1);
});

Deno.test(
  "buildVersion: only a bounded, whitespace-free string is an id",
  () => {
    assertEquals(usableVersionId("abc123"), true);
    assertEquals(usableVersionId("x".repeat(64)), true);
    assertEquals(usableVersionId(""), false);
    assertEquals(usableVersionId("x".repeat(65)), false);
    assertEquals(usableVersionId("a b"), false);
    assertEquals(usableVersionId(null), false);
    assertEquals(usableVersionId(7), false);
  },
);
