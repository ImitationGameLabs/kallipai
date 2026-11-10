// The local cache is the one replay path the server cannot rescue: a row it
// holds without a replayed field (quote, attachment) stays that way in the
// transcript, because the window merge keeps the older line. These pins keep
// the fields on the round trip (row -> cache -> transcript) and keep every
// cache write in step.
//
// The module under test is rune-bearing: `deno test` runs it uncompiled, so a
// passthrough $state shim plus an ambient declaration let it load (the same
// seam relayWindow_test uses).

import { assert, assertEquals } from "@std/assert";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { cachedLineToLine } = await import("./conversation/shared.ts");
type CachedLine = import("@kallipai/kallipai-keryx-client").CachedLine;

/** The one place a cached row is minted: shared.ts's cache write entry. A
 *  second `cachePut({` anywhere else would write a row this pin never reads. */
const MINT = new URL("./conversation/shared.ts", import.meta.url);

/** The session tree this pin sweeps. The property -- no writer mints its own
 *  row -- has to hold for a module that does not exist here yet, so it is
 *  checked over the directory instead of a list of today's writers. The walk
 *  skips `_test.ts` by suffix, so a source file that carries a test seam
 *  (notify.ts stands in for `Notification`, realtimeReverify.ts takes its
 *  verifier as an argument) is read as production text; neither mints a row,
 *  and if one ever did this pin would red rather than stay quiet. */
const SESSION = new URL("./", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

/** Every non-test TypeScript source under the session tree, keyed by its path
 *  relative to that tree. */
async function sessionSources(): Promise<Map<string, string>> {
  const out = new Map<string, string>();
  async function walk(dir: URL, prefix: string): Promise<void> {
    for await (const entry of Deno.readDir(dir)) {
      if (entry.name.endsWith("_test.ts")) continue;
      if (entry.isDirectory) {
        await walk(new URL(`${entry.name}/`, dir), `${prefix}${entry.name}/`);
      } else if (entry.name.endsWith(".ts")) {
        out.set(`${prefix}${entry.name}`, source(new URL(entry.name, dir)));
      }
    }
  }
  await walk(SESSION, "");
  return out;
}

/** Whether a source reaches keryx's `put`: a value import naming it, with or
 *  without an alias, a namespace import of the client, or either form taken
 *  from a deep subpath. Every one of those mints the same row the text scan
 *  looks for, under a name the scan cannot see. */
function importsPut(text: string): boolean {
  const ns =
    /import\s*\*\s*as\s+\w+\s+from\s+"@kallipai\/kallipai-keryx-client(\/[^"]*)?"/;
  if (ns.test(text)) return true;
  const stmt =
    /import\s+(?!type\b)([\s\S]*?)from\s+"@kallipai\/kallipai-keryx-client(\/[^"]*)?"/g;
  for (const [, clause] of text.matchAll(stmt)) {
    const open = clause.indexOf("{");
    if (open < 0) continue;
    const names = clause.slice(open + 1, clause.lastIndexOf("}")).split(",");
    if (names.some((n) => /^\s*(?:type\s+)?put\b/.test(n))) return true;
  }
  return false;
}

Deno.test("a cached line keeps its quote id on the way back", () => {
  const cached = {
    conversationId: "conv-1",
    historyId: 7,
    role: "user",
    text: "quoting you",
    quote: 3,
  } as unknown as CachedLine;
  assertEquals(cachedLineToLine(cached).quote, 3);
  // A row cached before the field existed replays as quote-less, not as 0.
  const legacy = {
    conversationId: "conv-1",
    historyId: 8,
    role: "user",
    text: "older",
  } as unknown as CachedLine;
  assertEquals(cachedLineToLine(legacy).quote, undefined);
});

// Every field a cached row has to hand back to the transcript. `seq` is the
// one optional field here and the one a hydrate places rows by: dropping it
// leaves a cache row that still type-checks and still renders, one window down.
const REPLAYED_FIELDS = ["sender", "createdAt", "attachment", "quote", "seq"];

Deno.test(
  "every cache write carries the fields the row replays",
  { permissions: { read: [MINT, SESSION.pathname] } },
  async () => {
    // One minting point, carrying every field a replay reads.
    const src = source(MINT);
    const at = src.indexOf("cachePut({");
    assert(at >= 0, "shared.ts mints no cached row");
    const end = src.indexOf("});", at);
    assert(end > at, "a cachePut call has no close");
    const block = src.slice(at, end);
    for (const field of REPLAYED_FIELDS) {
      assert(
        block.includes(`${field}:`),
        `the cache write drops ${field}: ${block.slice(0, 80)}`,
      );
    }
    assertEquals(
      src.indexOf("cachePut({", end),
      -1,
      "shared.ts mints a second row shape",
    );
    // Every writer goes through that entry rather than minting its own row,
    // and that holds for the whole tree rather than for a list of files.
    const files = await sessionSources();
    assert(files.size > 10, `expected the session tree: ${files.size} files`);
    const minters = [...files.entries()]
      .filter(([, text]) => text.includes("cachePut("))
      .map(([path]) => path);
    assertEquals(minters, ["conversation/shared.ts"]);
    // The same row under another name: `put` is the keryx export behind the
    // mint, so a module that imports it directly writes a cached line the
    // text scan above never sees.
    const binders = [...files.entries()]
      .filter(([, text]) => importsPut(text))
      .map(([path]) => path);
    assertEquals(binders, ["conversation/shared.ts"]);
    // Call sites only: the entry's own definition line carries the same
    // name, and counting it would make the four writers look like five. The
    // count is exact, so a fifth writer has to be added here as well.
    let calls = 0;
    for (const text of files.values()) {
      for (const line of text.split("\n")) {
        if (line.includes("function putCacheLine(")) continue;
        calls += line.split("putCacheLine(").length - 1;
      }
    }
    assertEquals(calls, 4, `expected four cache writes: ${calls}`);
  },
);
