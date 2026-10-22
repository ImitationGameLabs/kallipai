// Behaviour tests for the attachments tray: a picked file belongs to the
// conversation that picked it, so an upload that finishes after a switch must
// never hand one conversation's minted record id to another's composer. The
// module is rune-bearing and `deno test` runs it uncompiled, so it loads behind
// a passthrough $state shim (the seam composer_submit_test uses).

import { assertEquals } from "@std/assert";
import type { AttachmentItem } from "./attachments.ts";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { createAttachmentTray } = await import("./attachmentTray.svelte.ts");

function item(id: string): AttachmentItem {
  return {
    id,
    file: new File([new Uint8Array(1)], `${id}.txt`),
    name: `${id}.txt`,
    size: 1,
    status: "uploading",
  };
}

Deno.test("an upload finishing after a switch lands in its own tray", () => {
  let key = "t1";
  const tray = createAttachmentTray(() => key);
  tray.add("t1", item("att-0"));

  // The reader moves to another conversation while the PUT is still running.
  key = "t2";
  tray.patch("t1", "att-0", { status: "ready", recordId: "rec-a" });

  // The new conversation's composer reads an empty tray, so the send cannot
  // carry the other conversation's record id.
  assertEquals(tray.current(), []);

  key = "t1";
  assertEquals(
    tray.current().map((a) => [a.id, a.status, a.recordId]),
    [["att-0", "ready", "rec-a"]],
  );
});

Deno.test("switching away keeps each conversation's tray intact", () => {
  let key = "t1";
  const tray = createAttachmentTray(() => key);
  tray.add("t1", item("att-0"));
  tray.add("t1", item("att-1"));

  key = "t2";
  tray.add("t2", item("att-2"));
  assertEquals(
    tray.current().map((a) => a.id),
    ["att-2"],
  );

  key = "t1";
  assertEquals(
    tray.current().map((a) => a.id),
    ["att-0", "att-1"],
  );
});

Deno.test("clearing the current tray spares the others", () => {
  let key = "t1";
  const tray = createAttachmentTray(() => key);
  tray.add("t1", item("att-0"));
  tray.add("t2", item("att-1"));

  key = "t2";
  tray.clear("t2");
  assertEquals(tray.current(), []);

  key = "t1";
  assertEquals(
    tray.current().map((a) => a.id),
    ["att-0"],
  );
});

Deno.test("removing an item only touches the current conversation", () => {
  let key = "t1";
  const tray = createAttachmentTray(() => key);
  tray.add("t1", item("att-0"));
  tray.add("t2", item("att-1"));

  // The same id in the other tray is not the item the reader removed.
  tray.remove("t1", "att-1");
  assertEquals(
    tray.current().map((a) => a.id),
    ["att-0"],
  );

  key = "t2";
  assertEquals(
    tray.current().map((a) => a.id),
    ["att-1"],
  );
});

Deno.test("a gesture write lands in the bucket it names", () => {
  // The page captures the key before the gesture: clear() and remove() act
  // on the conversation the list came from, not on wherever the reader is.
  let key = "t1";
  const tray = createAttachmentTray(() => key);
  tray.add("t1", item("att-0"));
  tray.add("t2", item("att-1"));

  tray.clear("t1");
  assertEquals(tray.current(), []);

  key = "t2";
  assertEquals(
    tray.current().map((a) => a.id),
    ["att-1"],
  );
});
