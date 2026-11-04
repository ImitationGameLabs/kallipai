// Behaviour tests for the stored-text owner: the block a read fills is marked
// before the answer is asked for, and an answer lands only in the block its
// read opened. The module is rune-bearing, but deno test runs it uncompiled,
// so a passthrough $state shim lets it run with plain fields (the profiles
// store test pattern).
declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  namespace $state {
    function snapshot<T>(v: T): T;
  }
}

(globalThis as Record<string, unknown>)["$state"] = Object.assign(
  (v: unknown) => v,
  {
    snapshot: (v: unknown) => v,
  },
);

const { assertEquals } = await import("@std/assert");
const { BlockTexts } = await import("./blockTexts.svelte.ts");
const { emptyBodyText } = await import("./turnView.ts");

function text(value: string, total = value.length) {
  return { ...emptyBodyText(), text: value, total, atEnd: true };
}

Deno.test("a read marks its block before the answer arrives", () => {
  const texts = new BlockTexts();
  const token = texts.begin("7:0");
  const entry = texts.entries["7:0"];
  assertEquals(entry?.loading, true);
  assertEquals(entry?.token, token);
  assertEquals(entry?.text, "");
});

Deno.test("an answer with no open read writes nothing", () => {
  const texts = new BlockTexts();
  texts.settle("7:0", 1, { kind: "text", text: text("late") });
  assertEquals(texts.entries["7:0"], undefined);
});

Deno.test("a block that moved past a read keeps what it shows", () => {
  const texts = new BlockTexts();
  const first = texts.begin("7:0");
  const second = texts.begin("7:0");
  texts.settle("7:0", first, { kind: "text", text: text("stale") });
  assertEquals(texts.entries["7:0"]?.text, "");
  assertEquals(texts.entries["7:0"]?.loading, true);
  texts.settle("7:0", second, { kind: "text", text: text("fresh") });
  assertEquals(texts.entries["7:0"]?.text, "fresh");
  assertEquals(texts.entries["7:0"]?.loading, false);
});

Deno.test("a failure lands in the block its read opened", () => {
  const texts = new BlockTexts();
  const token = texts.begin("7:1");
  texts.settle("7:1", token, { kind: "failed", error: "boom" });
  assertEquals(texts.entries["7:1"]?.error, "boom");
  assertEquals(texts.entries["7:1"]?.loading, false);
});

Deno.test("clearing drops every block and voids reads still in flight", () => {
  const texts = new BlockTexts();
  const token = texts.begin("7:0");
  texts.clear();
  assertEquals(texts.entries, {});
  texts.settle("7:0", token, { kind: "text", text: text("late") });
  assertEquals(texts.entries["7:0"], undefined);
});

Deno.test("a block handed out cannot be changed from outside", () => {
  const texts = new BlockTexts();
  const token = texts.begin("7:0");
  const opened = texts.entries["7:0"];
  assertEquals(Object.isFrozen(opened), true);
  try {
    (opened as { text: string }).text = "written from outside";
  } catch {
    // a frozen entry refuses the write outright in strict mode
  }
  assertEquals(texts.entries["7:0"]?.text, "");
  texts.settle("7:0", token, { kind: "text", text: text("kept") });
  assertEquals(texts.entries["7:0"]?.text, "kept");
});
