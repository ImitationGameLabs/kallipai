// The ui package consumes faces, not effects: `kallipai-common` and the
// `*-client` packages turn an effect into a `Promise` or an `AsyncGenerator`
// and hand that up (docs/en/development/frontend.md, "Effect Stays Below
// `kallipai-ui`"). This scan is the voice that rule lacked: `effect` resolves
// from the hoisted root `node_modules`, so `deno check` and `deno lint` both
// pass on an import written here, and nothing else said no.
//
// It walks every TypeScript and Svelte file under src and reports a specifier
// naming the package -- `from "effect"`, a subpath such as `effect/Stream`, a
// side-effect `import "effect"`, a dynamic `import("effect")`, and the same
// behind `export ... from`. Components are in the set on purpose: `deno lint`
// does not parse them, so a `.svelte` file was the one place an import could
// land unseen. Test files are not skipped either: a test that holds an effect
// is still this package holding one. The one file it passes over
// is this one, whose shims are import text by definition.
//
// It judges import specifiers, so it proves nothing about what the ui holds at
// runtime. Three blind spots, all deliberate: a computed module name
// (`import(name)`) never matches; a local module that re-exports the package
// from outside src is one hop away and invisible; the comment filter drops
// lines that open a comment, but the inner lines of a block comment that do
// not lead with `*` still read as code, and a trailing comment on a code line
// is not stripped. A green scan is not the rule held everywhere; it is that
// no file in this tree names the package in an import.

import { assert, assertEquals } from "@std/assert";
import { codeLinesOf } from "./testing/source_pins.ts";

const SRC = new URL("../", import.meta.url);
const SOURCE = /\.(mts|cts|tsx|ts|svelte)$/u;
const SELF = new URL(import.meta.url).href.slice(SRC.href.length);
/** Every form that names the package: static, dynamic, side effect, subpath, prefixed. */
const SPECIFIER =
  /(?:\bfrom|\bimport)\s*(?:\(\s*)?["']((?:npm:|jsr:)?effect(?:\/[^"']*)?)["']/gu;

/** The import specifiers in the text that name the package. */
function effectImports(text: string): string[] {
  return [...codeLinesOf(text).matchAll(SPECIFIER)].map((match) =>
    match[0].replace(/\s+/gu, " "),
  );
}

type Source = { name: string; text: string };

async function tree(dir: URL, prefix = ""): Promise<Source[]> {
  const out: Source[] = [];
  for await (const entry of Deno.readDir(dir)) {
    const name = prefix + entry.name;
    if (entry.isDirectory) {
      out.push(...(await tree(new URL(entry.name + "/", dir), name + "/")));
    } else if (SOURCE.test(entry.name)) {
      out.push({
        name,
        text: await Deno.readTextFile(new URL(entry.name, dir)),
      });
    }
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

Deno.test("the scan reads the specifier, not the word", () => {
  const hits = [
    'import { Effect } from "effect";',
    'import type { Effect } from "effect";',
    "import { Stream } from 'effect/Stream';",
    'const Effect = await import("effect");',
    'await import(\n  "effect"\n);',
    'import "effect";',
    'export { Data } from "effect";',
    'import { Effect } from "npm:effect";',
  ];
  for (const text of hits) {
    assert(
      effectImports(text).length === 1,
      `this import names the package: ${text}`,
    );
  }
  const misses = [
    'import { x } from "./effect.ts";',
    'import { x } from "effectual";',
    'import { x } from "npm:effectual";',
    'import { x } from "npm:effect-extra";',
    '// import { Effect } from "effect";',
    '/* import { Effect } from "effect"; */',
    'const where = "effect";',
  ];
  assertEquals(
    misses.flatMap((text) => effectImports(text)),
    [],
    "a relative module, another package, or a mention is not an import",
  );
});

Deno.test("no module in the ui package imports effect", async () => {
  const files = (await tree(SRC)).filter((file) => file.name !== SELF);
  assert(
    files.some((file) => file.name.endsWith(".svelte")),
    "the scan must cover components: deno lint does not parse them",
  );
  assert(
    files.some((file) => file.name.endsWith(".ts")),
    "the scan must cover modules",
  );
  const offenders = files.flatMap((file) =>
    effectImports(file.text).map((specifier) => `${file.name}: ${specifier}`),
  );
  assertEquals(
    offenders,
    [],
    "the ui consumes faces; add the call on the matching *-client package",
  );
});
