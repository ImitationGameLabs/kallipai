// Behaviour tests for a discussion's window: the page a read may keep, the
// reset that makes an older page a stranger even when the reader came back to
// the same discussion, and the sync that reads a pair once. The module is
// rune-bearing and `deno test` runs it uncompiled, so it loads behind a
// passthrough shim for `$state` and `$derived` (the seam directTranscript_test
// uses). Under that shim the computed cells are snapshots of the call, so these
// cases assert what a command writes to the window's own state, never a derived
// field: `pages`, `read`, `loading` are the facts a page landing changes.

import { assertEquals } from "@std/assert";
import type { RoomLine } from "../room-message.ts";
import type { ThreadPage } from "../threads.ts";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  function $derived<T>(fn: () => T): T;
}

const shims = globalThis as Record<string, unknown>;
shims["$state"] = (v: unknown) => v;
// `$derived(expression)` in the source, not a thunk: Svelte's compiler is what
// wraps it, and `deno test` runs the file as written, so the shim hands the
// value straight back. A derived cell is therefore a snapshot at factory time
// here -- which is why the cases below read the window's own state, never a
// computed field.
const derived = (value: unknown) => value;
Object.assign(derived, { by: (fn: () => unknown) => fn() });
shims["$derived"] = derived;

const { createThreadWindow } = await import("./threadWindow.svelte.ts");

/** One drawn row, named so a page can be told from another page. */
function line(seq: number, text: string): RoomLine {
  return { seq, text } as unknown as RoomLine;
}

/** A page of one row: the root it belongs to, under a name the test can read. */
function page(root: number, text: string): ThreadPage {
  return { lines: [line(root, text)], lastSeq: root, hasMore: false };
}

/** Let every pending promise chain and timer settle: reads are fired without
 *  being awaited, so a test waits on the window rather than on the call. */
async function settle(): Promise<void> {
  for (let i = 0; i < 8; i += 1) await new Promise((r) => setTimeout(r, 0));
}

interface Deferred {
  readonly promise: Promise<ThreadPage>;
  resolve(page: ThreadPage): void;
}

function deferred(): Deferred {
  let resolve!: (page: ThreadPage) => void;
  const promise = new Promise<ThreadPage>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

Deno.test(
  "sync reads a discussion once and again only when the pair changes",
  async () => {
    const asked: number[] = [];
    let root: number | null = 5;
    const window = createThreadWindow({
      roomId: () => "room-1",
      root: () => root,
      lines: () => [],
      selfId: () => "me",
      fetchPage: (_room, asked_root) => {
        asked.push(asked_root);
        return Promise.resolve(page(asked_root, "first"));
      },
    });

    window.sync();
    await settle();
    assertEquals(asked, [5], "the open discussion is read once");
    assertEquals(window.pages, 1);

    window.sync();
    await settle();
    assertEquals(asked, [5], "the same pair is not read twice");
    assertEquals(window.pages, 1, "and the window keeps its page");

    root = 6;
    window.sync();
    await settle();
    assertEquals(asked, [5, 6], "a new discussion is read");
    assertEquals(window.pages, 1, "the previous window's page is gone");
    assertEquals(window.read?.lines[0]?.text, "first");
  },
);

Deno.test(
  "a clear forgets the pair, so the next sync of it reads again",
  async () => {
    const asked: number[] = [];
    const window = createThreadWindow({
      roomId: () => "room-1",
      root: () => 5,
      lines: () => [],
      selfId: () => "me",
      fetchPage: (_room, asked_root) => {
        asked.push(asked_root);
        return Promise.resolve(page(asked_root, "first"));
      },
    });

    window.sync();
    await settle();
    assertEquals(window.pages, 1);

    window.clear();
    assertEquals(window.pages, 0, "a closed discussion holds nothing");
    assertEquals(window.read, null);

    window.sync();
    await settle();
    assertEquals(asked, [5, 5], "the pair is read again after a close");
    assertEquals(window.pages, 1);
  },
);

Deno.test(
  "a page read before the last reset never lands, even back on its own discussion",
  async () => {
    const asked: number[] = [];
    const firstVisit = deferred();
    const secondVisit = deferred();
    let root: number | null = 5;
    const window = createThreadWindow({
      roomId: () => "room-1",
      root: () => root,
      lines: () => [],
      selfId: () => "me",
      fetchPage: (_room, asked_root) => {
        asked.push(asked_root);
        if (asked.length === 1 || asked.length === 3) {
          return asked.length === 1 ? firstVisit.promise : secondVisit.promise;
        }
        return Promise.resolve(page(asked_root, "the other discussion"));
      },
    });

    window.sync();
    await settle();
    root = 6;
    window.sync();
    await settle();
    assertEquals(window.read?.lines[0]?.text, "the other discussion");

    root = 5;
    window.sync();
    await settle();
    assertEquals(asked, [5, 6, 5], "the revisit reads its own page");
    assertEquals(window.pages, 0, "nothing has landed for the revisit yet");

    // The first visit's page answers now. Its room and root match the window on
    // screen; only the reset between them tells the two apart.
    firstVisit.resolve(page(5, "the first visit"));
    await settle();
    assertEquals(window.pages, 0, "the earlier window's page is dropped");
    assertEquals(window.read, null, "and nothing of it is drawn");

    secondVisit.resolve(page(5, "the revisit"));
    await settle();
    assertEquals(window.pages, 1);
    assertEquals(window.read?.lines[0]?.text, "the revisit");
  },
);
