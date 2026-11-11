// The base layer is everything under src/lib/ except the manage/ overlay: the
// modules the two faces share (the set draft and its save helper under
// gateway/, the loose helpers such as compute.ts) plus the user-side code that
// predates this split. Imports point from the overlay into the base, never
// back. lib/session/ reaches back twice, both against the management
// backend: a type-only import in statusCard.svelte.ts and a runtime import
// in statusCard_test.ts. The edge is on record in pending/frontend.md
// (51d48ab), and lib/session/ joins the scan once that record is retired.
//
// A test file may take the admin row types, because the set draft is built
// from an admin row; that reference has to be `import type`, so nothing
// runtime crosses the seam.
import { assert, assertEquals } from "@std/assert";

const LIB = new URL("./", import.meta.url);
const GATEWAY = new URL("gateway/", import.meta.url);
const SKIP = ["manage", "session"];
const EDGE = new RegExp("[\"'](\\.\\.?\\/)+manage\\/");
const TYPE_IMPORT = /import\s+type\s+[\s\S]*?from\s+"[^"]*";/g;
const SOURCE = /\.(ts|svelte)$/;

type Source = { name: string; text: string };

async function read(dir: URL, entry: string, name: string): Promise<Source> {
  return { name, text: await Deno.readTextFile(new URL(entry, dir)) };
}

async function tree(dir: URL, prefix = ""): Promise<Source[]> {
  const out: Source[] = [];
  for await (const entry of Deno.readDir(dir)) {
    const name = prefix + entry.name;
    if (entry.isDirectory) {
      if (SKIP.includes(name)) continue;
      out.push(...(await tree(new URL(entry.name + "/", dir), name + "/")));
    } else if (SOURCE.test(entry.name)) {
      out.push(await read(dir, entry.name, name));
    }
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

Deno.test("base layer: the modules both faces share live here", async () => {
  const names = (await tree(LIB)).map((file) => file.name);
  assert(names.length > 20, `scanned ${names.length} base files`);
  const expected = [
    "compute.ts",
    "gateway/setDraft.ts",
    "gateway/saveSet.ts",
    "gateway/store.svelte.ts",
  ];
  for (const name of expected) {
    assert(names.includes(name), `${name} belongs to the base layer`);
  }
});

Deno.test(
  "base layer: no module here reaches into the manage overlay",
  async () => {
    const scanned = (await tree(LIB)).filter(
      (file) => !file.name.endsWith("_test.ts"),
    );
    assert(scanned.length > 20, `scanned ${scanned.length} base modules`);
    const offenders = scanned
      .filter((file) => EDGE.test(file.text))
      .map((file) => file.name);
    assertEquals(offenders, []);
  },
);

Deno.test(
  "base layer: a gateway test file reaches the overlay only for types",
  async () => {
    const scanned = (await tree(GATEWAY)).filter((file) =>
      file.name.endsWith("_test.ts"),
    );
    assert(scanned.length > 3, `scanned ${scanned.length} gateway test files`);
    const offenders = scanned
      .filter((file) => EDGE.test(file.text.replace(TYPE_IMPORT, "")))
      .map((file) => file.name);
    assertEquals(offenders, []);
  },
);
