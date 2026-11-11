// Guard for the host route trees: each host app (kallipai-app, kallipai-web)
// serves every page through one rest route (`[...path]`) plus the root layout
// pair, and the route list lives once in routeTable.ts. A page file added
// back into either tree, a missing shell file, or a drift between the two
// shells all turn this red.
//
// The file name breaks the gate.ts <-> gate_test.ts pairing convention on
// purpose: the test target is the two host trees themselves, not a module in
// this package. Monorepo assumption: resolving four levels up reaches
// packages/, where both hosts live; if the workspace is ever split into
// repos this test fails, forcing the ownership question to be re-decided.

import { assert, assertEquals } from "@std/assert";

const APP_ROUTES = new URL(
  "../../../../kallipai-app/src/routes/",
  import.meta.url,
);
const WEB_ROUTES = new URL(
  "../../../../kallipai-web/src/routes/",
  import.meta.url,
);

/** Every file the routes tree may hold: the rest route and the root pair. */
const SHELL_FILES = [
  "+layout.svelte",
  "+layout.ts",
  "[...path]/+page.svelte",
  "[...path]/+page.ts",
];

/** The shell files both hosts share byte for byte (the layout is per host). */
const TWIN_FILES = [
  "+layout.ts",
  "[...path]/+page.svelte",
  "[...path]/+page.ts",
];

function collectFiles(dir: URL, prefix = ""): Map<string, Uint8Array> {
  const files = new Map<string, Uint8Array>();
  for (const entry of Deno.readDirSync(dir)) {
    const rel = prefix + entry.name;
    if (entry.isDirectory) {
      for (const [path, bytes] of collectFiles(
        new URL(`${entry.name}/`, dir),
        `${rel}/`,
      )) {
        files.set(path, bytes);
      }
    } else if (entry.isFile) {
      files.set(rel, Deno.readFileSync(new URL(entry.name, dir)));
    } else {
      // A symlink or any other entry type would be silently skipped and
      // punch a hole in the guard; refuse instead.
      throw new Error(`unexpected routes entry type at ${rel}`);
    }
  }
  return files;
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

// The tree walk reads outside this package; scope the grant to the two
// routes dirs instead of opening up the whole filesystem. Run via
// `deno task test`: a bare `deno test` cannot escalate this grant.
const READ_PERMISSIONS = { permissions: { read: [APP_ROUTES, WEB_ROUTES] } };

Deno.test(
  "each host routes tree is exactly the shell files",
  READ_PERMISSIONS,
  () => {
    for (const [host, dir] of [
      ["kallipai-app", APP_ROUTES],
      ["kallipai-web", WEB_ROUTES],
    ] as const) {
      const files = collectFiles(dir);
      assertEquals(
        [...files.keys()].sort(),
        [...SHELL_FILES].sort(),
        `${host} src/routes is not exactly the shell files; a per-page file belongs in the route table instead`,
      );
    }
  },
);

Deno.test(
  "the rest route files are byte-identical across hosts",
  READ_PERMISSIONS,
  () => {
    const app = collectFiles(APP_ROUTES);
    const web = collectFiles(WEB_ROUTES);
    for (const path of TWIN_FILES) {
      const appBytes = app.get(path);
      const webBytes = web.get(path);
      assert(
        appBytes !== undefined && webBytes !== undefined,
        `${path} is missing on one host`,
      );
      assert(
        bytesEqual(appBytes, webBytes),
        `the rest route drifted apart (${appBytes.length} vs ${webBytes.length} bytes): ${path}`,
      );
    }
  },
);
