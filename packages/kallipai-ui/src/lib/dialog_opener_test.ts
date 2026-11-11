// Every controller that seeds a dialog from a read holds one request token and
// one way in. A second opener that skips the token brings back the stale-seed
// race the token exists to close, and it is easy to add back because it looks
// like a convenience. These counts are the shape: one bump, and the dialog
// opens only from the branches of that one opener.
import { assert, assertEquals } from "@std/assert";

const LIB = new URL("./", import.meta.url);

const MODULES = [
  { path: "manage/gateway/catalogSetDialogs.svelte.ts", opens: 2 },
  { path: "gateway/mineSetDialogs.svelte.ts", opens: 2 },
  { path: "gateway/mineCollectionDialogs.svelte.ts", opens: 3 },
  { path: "gateway/mineGroupDialogs.svelte.ts", opens: 2 },
];

async function source(path: string): Promise<string> {
  const text = await Deno.readTextFile(new URL(path, LIB));
  assert(text.length > 1000, `${path} read as a controller`);
  return text;
}

Deno.test("dialog controllers: one request token, bumped once", async () => {
  for (const { path } of MODULES) {
    const text = await source(path);
    assertEquals(
      text.match(/\+\+editRequest/g)?.length ?? 0,
      1,
      `${path} bumps its token once`,
    );
    assert(
      text.includes("const request = ++editRequest;"),
      `${path} keeps the clicked request`,
    );
  }
});

Deno.test("dialog controllers: the dialog opens from one opener", async () => {
  for (const { path, opens } of MODULES) {
    const text = await source(path);
    assertEquals(
      text.match(/\b\w*Open = true;/g)?.length ?? 0,
      opens,
      `${path} opens the dialog from ${opens} branch sites`,
    );
  }
});
